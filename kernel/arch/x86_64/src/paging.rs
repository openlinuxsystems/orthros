//! The kernel's page tables.
//!
//! The boot trampoline's tables map the first 4 GiB with large pages and no
//! protection. [`build_and_switch`] replaces them with the kernel's own:
//!
//! - the kernel image at `KERNEL_OFFSET`, with 4 KiB pages: text read-only
//!   and executable, read-only data non-executable, data and bss writable
//!   and non-executable, and the boot CPU's stack guard page left unmapped;
//! - every RAM range of the memory map at `DIRECT_MAP` + its physical
//!   address, writable and non-executable, with 2 MiB pages where the range
//!   allows and 4 KiB pages at its edges;
//! - a window at `MAPPING_BASE` for device registers, the framebuffer and
//!   the other CPUs' stacks, filled in on demand.
//!
//! Nothing is mapped at the bottom of the address space, so null pointers
//! fault. The page attribute table is reprogrammed so that device memory
//! can be mapped uncached and the framebuffer write-combining.

use core::sync::atomic::{AtomicU64, Ordering};

use radix_sync::SpinLock;

use crate::cpu::{self, msr};
use crate::memory::{self, DIRECT_MAP, KERNEL_OFFSET, LARGE_PAGE, PAGE};

/// Start of the window for device mappings and stacks (PML4 slot 384).
pub const MAPPING_BASE: u64 = 0xFFFF_C000_0000_0000;
/// Size of that window.
const MAPPING_SIZE: u64 = 512 << 30;

const PRESENT: u64 = 1;
const WRITABLE: u64 = 1 << 1;
const WRITE_THROUGH: u64 = 1 << 3;
const CACHE_DISABLE: u64 = 1 << 4;
const LARGE: u64 = 1 << 7;
const GLOBAL: u64 = 1 << 8;
const NO_EXECUTE: u64 = 1 << 63;
const ADDRESS: u64 = 0x000f_ffff_ffff_f000;

/// The page attribute table the kernel programs: index 0 write-back,
/// 1 write-combining, 2 uncached-minus, 3 uncached, then the defaults for
/// the indices it does not use (write-back, write-protect, uncached-minus,
/// write-through).
const PAT_VALUE: u64 = 0x0407_0506_0007_0106;

/// How a mapping is cached.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Cache {
    /// Normal memory.
    WriteBack,
    /// Framebuffers: writes are combined, reads are uncached.
    WriteCombining,
    /// Device registers.
    Uncached,
}

impl Cache {
    fn bits(self) -> u64 {
        match self {
            Cache::WriteBack => 0,
            Cache::WriteCombining => WRITE_THROUGH,
            Cache::Uncached => WRITE_THROUGH | CACHE_DISABLE,
        }
    }
}

/// Access rights of a mapping.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Access {
    /// Read-only and executable.
    Code,
    /// Read-only.
    ReadOnly,
    /// Readable and writable.
    ReadWrite,
}

struct State {
    /// Physical address of the PML4.
    pml4: u64,
    /// The no-execute bit, or 0 if the CPU lacks it.
    nx: u64,
    /// The global bit, or 0 if the CPU lacks it.
    global: u64,
    /// Next free address in the mapping window.
    next_mapping: u64,
}

static STATE: SpinLock<State> =
    SpinLock::new(State { pml4: 0, nx: 0, global: 0, next_mapping: MAPPING_BASE });

/// Bumped whenever a mapping is removed; CPUs flush their TLB when they
/// see a newer value.
static TLB_GENERATION: AtomicU64 = AtomicU64::new(0);

/// A page table, reached through the direct map.
fn table(phys: u64) -> &'static mut [u64; 512] {
    // SAFETY: `phys` is a page-table page the kernel allocated, in RAM
    // below 4 GiB, so the direct map (or the boot alias before it exists)
    // reaches it. Page tables are only changed with `STATE` locked, and no
    // other Rust reference to them exists.
    unsafe { &mut *(memory::phys_to_virt(phys) as *mut [u64; 512]) }
}

fn new_table() -> u64 {
    memory::alloc_pages(1, true).expect("out of memory for page tables")
}

/// The entry for `virt` in the table at `level` (4 = PML4, 3 = PDPT,
/// 2 = page directory, 1 = page table), creating tables on the way down.
/// Fails if a large page is in the way.
fn entry(state: &State, virt: u64, level: u32) -> Option<&'static mut u64> {
    let mut table_phys = state.pml4;
    for current in ((level + 1)..=4).rev() {
        let index = ((virt >> (12 + 9 * (current - 1))) & 0x1ff) as usize;
        let slot = &mut table(table_phys)[index];
        if *slot & PRESENT == 0 {
            *slot = new_table() | PRESENT | WRITABLE;
        } else if *slot & LARGE != 0 {
            return None;
        }
        table_phys = *slot & ADDRESS;
    }
    let index = ((virt >> (12 + 9 * (level - 1))) & 0x1ff) as usize;
    Some(&mut table(table_phys)[index])
}

fn leaf_flags(state: &State, access: Access, cache: Cache) -> u64 {
    let mut flags = PRESENT | state.global | cache.bits();
    match access {
        Access::Code => {}
        Access::ReadOnly => flags |= state.nx,
        Access::ReadWrite => flags |= WRITABLE | state.nx,
    }
    flags
}

fn map_4k(state: &State, virt: u64, phys: u64, flags: u64) {
    let slot = entry(state, virt, 1).expect("mapping over a large page");
    *slot = phys | flags;
}

fn map_2m(state: &State, virt: u64, phys: u64, flags: u64) {
    // In large-page entries the PAT bit moves to bit 12; the kernel only
    // maps write-back memory with large pages, so it stays clear.
    let slot = entry(state, virt, 2).expect("mapping over a large page");
    *slot = phys | flags | LARGE;
}

unsafe extern "C" {
    static __kernel_text_start: u8;
    static __kernel_text_end: u8;
    static __kernel_rodata_start: u8;
    static __kernel_rodata_end: u8;
    static __kernel_data_start: u8;
    static __kernel_data_end: u8;
    #[link_name = "bsp_stack_guard"]
    static BSP_STACK_GUARD: u8;
}

/// Address of a linker symbol.
macro_rules! symbol {
    ($name:ident) => {
        &raw const $name as u64
    };
}

/// Whether `address` is in the kernel's code.
pub fn is_kernel_text(address: usize) -> bool {
    let (start, end) = (symbol!(__kernel_text_start), symbol!(__kernel_text_end));
    (start..end).contains(&(address as u64))
}

/// The guard page below the boot CPU's stack.
pub fn bsp_stack_guard() -> core::ops::Range<usize> {
    let guard = symbol!(BSP_STACK_GUARD) as usize;
    guard..guard + PAGE as usize
}

/// Programs the page attribute table and enables no-execute and global
/// pages on this CPU. Every CPU runs this before using the kernel's tables.
pub fn init_cpu() {
    if cpu::feature::pat() {
        // SAFETY: the PAT MSR exists (CPUID says so); the new value keeps
        // index 0 write-back, which is what every existing mapping uses.
        unsafe { cpu::wrmsr(msr::PAT, PAT_VALUE) };
    }
    if cpu::feature::nx() {
        // SAFETY: EFER exists on every x86-64 CPU; setting NXE only makes
        // the NX bit of page-table entries meaningful.
        unsafe { cpu::wrmsr(msr::EFER, cpu::rdmsr(msr::EFER) | (1 << 11)) };
    }
    if cpu::feature::pge() {
        // SAFETY: setting CR4.PGE (bit 7) only lets global TLB entries
        // survive CR3 reloads.
        unsafe { cpu::write_cr4(cpu::cr4() | (1 << 7)) };
    }
    // CR0.WP (bit 16): the kernel cannot write read-only pages either.
    // SAFETY: the kernel never writes to its read-only mappings.
    unsafe { cpu::write_cr0(cpu::cr0() | (1 << 16)) };
}

/// Builds the kernel's page tables and switches to them. Called once, by
/// the boot CPU, after the memory map and the allocator are ready.
pub fn build_and_switch() {
    let mut state = STATE.lock();
    state.nx = if cpu::feature::nx() { NO_EXECUTE } else { 0 };
    state.global = if cpu::feature::pge() { GLOBAL } else { 0 };
    state.pml4 = new_table();

    // The kernel image.
    let guard = bsp_stack_guard();
    let sections = [
        (symbol!(__kernel_text_start), symbol!(__kernel_text_end), Access::Code),
        (symbol!(__kernel_rodata_start), symbol!(__kernel_rodata_end), Access::ReadOnly),
        (symbol!(__kernel_data_start), symbol!(__kernel_data_end), Access::ReadWrite),
    ];
    for (start, end, access) in sections {
        let flags = leaf_flags(&state, access, Cache::WriteBack);
        for virt in (start..end).step_by(PAGE as usize) {
            if !guard.contains(&(virt as usize)) {
                map_4k(&state, virt, virt - KERNEL_OFFSET, flags);
            }
        }
    }

    // The direct map of RAM.
    let flags = leaf_flags(&state, Access::ReadWrite, Cache::WriteBack);
    for region in memory::memory_map() {
        if !memory::is_ram(region.start..region.end) {
            continue;
        }
        let mut phys = region.start & !(PAGE - 1);
        let end = region.end.next_multiple_of(PAGE);
        while phys < end {
            if phys.is_multiple_of(LARGE_PAGE) && phys + LARGE_PAGE <= end {
                map_2m(&state, DIRECT_MAP + phys, phys, flags);
                phys += LARGE_PAGE;
            } else {
                map_4k(&state, DIRECT_MAP + phys, phys, flags);
                phys += PAGE;
            }
        }
    }

    // Create the mapping window's PDPT now, so the PML4 never changes
    // after the other CPUs start.
    let _ = entry(&state, MAPPING_BASE, 3);

    init_cpu();
    // SAFETY: the new tables map the kernel image at the addresses it was
    // linked at (so the running code, the stack and all statics stay put)
    // and RAM in the direct map at the same addresses the boot alias used,
    // so every pointer made from boot information stays valid. The boot
    // CPU's GDT and IDT are in the kernel image and already loaded.
    unsafe { cpu::write_cr3(state.pml4) };
}

/// Physical address of the kernel's PML4, for the AP trampoline.
pub fn kernel_pml4() -> u64 {
    STATE.lock().pml4
}

/// Maps `len` bytes of physical memory from `phys` into the mapping window
/// and returns the virtual address of `phys`.
pub fn map_physical(phys: u64, len: u64, access: Access, cache: Cache) -> usize {
    let mut state = STATE.lock();
    let first = phys & !(PAGE - 1);
    let last = (phys + len.max(1)).next_multiple_of(PAGE);
    let virt = state.next_mapping;
    assert!(virt + (last - first) <= MAPPING_BASE + MAPPING_SIZE, "mapping window full");
    state.next_mapping += last - first;
    let flags = leaf_flags(&state, access, cache);
    for offset in (0..last - first).step_by(PAGE as usize) {
        map_4k(&state, virt + offset, first + offset, flags);
    }
    (virt + (phys - first)) as usize
}

/// Allocates a stack of `pages` pages in the mapping window, with an
/// unmapped guard page below it. Returns (guard, stack).
pub fn allocate_stack(pages: u64) -> (core::ops::Range<usize>, core::ops::Range<usize>) {
    let phys = memory::alloc_pages(pages, false).expect("out of memory for a stack");
    let mut state = STATE.lock();
    let guard = state.next_mapping;
    let stack = guard + PAGE;
    state.next_mapping += (pages + 1) * PAGE;
    let flags = leaf_flags(&state, Access::ReadWrite, Cache::WriteBack);
    for page in 0..pages {
        map_4k(&state, stack + page * PAGE, phys + page * PAGE, flags);
    }
    let end = stack + pages * PAGE;
    (guard as usize..stack as usize, stack as usize..end as usize)
}

/// Identity-maps one low page, executable, for the AP trampoline.
pub fn map_identity(phys: u64) {
    let state = STATE.lock();
    map_4k(&state, phys, phys, PRESENT | WRITABLE);
}

/// Removes an identity mapping made by [`map_identity`] and asks every
/// CPU to flush its TLB.
pub fn unmap_identity(phys: u64) {
    let state = STATE.lock();
    if let Some(slot) = entry(&state, phys, 1) {
        *slot = 0;
    }
    drop(state);
    cpu::invlpg(phys as usize);
    TLB_GENERATION.fetch_add(1, Ordering::AcqRel);
    catch_up_tlb();
}

/// Flushes this CPU's TLB if a mapping was removed since it last did.
pub fn catch_up_tlb() {
    let generation = TLB_GENERATION.load(Ordering::Acquire);
    let cpu = crate::percpu::current();
    if cpu.tlb_generation.load(Ordering::Relaxed) != generation {
        cpu::flush_tlb();
        cpu.tlb_generation.store(generation, Ordering::Relaxed);
    }
}

/// The physical address `virt` maps to.
pub fn translate(virt: usize) -> Option<u64> {
    let virt = virt as u64;
    let state = STATE.lock();
    if state.pml4 == 0 {
        return None;
    }
    let mut table_phys = state.pml4;
    for level in (0..=3).rev() {
        let index = ((virt >> (12 + 9 * level)) & 0x1ff) as usize;
        let entry = table(table_phys)[index];
        if entry & PRESENT == 0 {
            return None;
        }
        let page_size = 1u64 << (12 + 9 * level);
        if level == 0 || (entry & LARGE != 0 && level <= 2) {
            return Some((entry & ADDRESS & !(page_size - 1)) | (virt & (page_size - 1)));
        }
        table_phys = entry & ADDRESS;
    }
    None
}

/// Copies mapped RAM from `virt` into `buf`, for the debugger.
pub fn read(virt: usize, buf: &mut [u8]) -> bool {
    let Some(end) = virt.checked_add(buf.len()) else { return false };
    // Check every page first, so nothing is copied unless all of it is RAM.
    let mut page = virt & !(PAGE as usize - 1);
    while page < end {
        match translate(page) {
            Some(phys) if memory::is_ram(phys..phys + PAGE) => {}
            _ => return false,
        }
        page += PAGE as usize;
    }
    let mut done = 0;
    while done < buf.len() {
        let at = virt + done;
        let chunk = (PAGE as usize - at % PAGE as usize).min(buf.len() - done);
        let Some(phys) = translate(at) else { return false };
        let source = memory::phys_to_virt(phys) as *const u8;
        for (i, byte) in buf[done..done + chunk].iter_mut().enumerate() {
            // SAFETY: `phys` starts a run of `chunk` bytes in one page of
            // RAM (checked above), which the direct map covers. A volatile
            // read of RAM has no side effects; the value may be torn if
            // another CPU writes it meanwhile, which is fine for a dump.
            *byte = unsafe { core::ptr::read_volatile(source.add(i)) };
        }
        done += chunk;
    }
    true
}
