// SPDX-FileCopyrightText: 2026 The OrthrOS contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! The kernel's page tables and TLB shootdown.
//!
//! The boot trampoline's tables map the first 4 GiB with large pages and no
//! protection. [`build_and_switch`] replaces them with the kernel's own:
//!
//! - the kernel image where it runs ([`memory::kernel_base`]), with 4 KiB
//!   pages: text read-only and executable, read-only data non-executable,
//!   data and bss writable and non-executable, and the boot CPU's stack
//!   guard page left unmapped;
//! - every RAM range of the memory map in the direct map
//!   ([`memory::direct_map`]), writable and non-executable, with 1 GiB
//!   pages where the CPU and the range allow, 2 MiB pages next, and 4 KiB
//!   pages at the edges; the image's own text and read-only data are
//!   read-only there too;
//! - the mapping window ([`memory::window`]) for device registers, the
//!   framebuffer, stacks and other areas, handed out by a
//!   [`radix_mm::Ranges`] allocator with an unmapped guard page below
//!   every area.
//!
//! Nothing is writable and executable at once (W^X), which
//! [`build_and_switch`] checks by walking the finished tables; nothing is
//! mapped at the bottom of the address space, so null pointers fault. On
//! CPUs that have them, SMEP and SMAP stop the kernel from running or
//! touching user pages, and PCIDs tag TLB entries by address space
//! ([`crate::space`]). The page attribute table is reprogrammed so that
//! device memory can be mapped uncached and the framebuffer
//! write-combining.
//!
//! Removing a mapping that other CPUs may have cached is a *TLB
//! shootdown*: [`shootdown`] flushes the range locally, sends the others
//! the TLB IPI and waits until each has flushed it too.

use core::ops::Range;
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use radix_hal::MAX_CPUS;
use radix_mm::{AllocError, Owner, Ranges};
use radix_sync::SpinLock;

use crate::cpu::{self, msr};
use crate::memory::{self, HUGE_PAGE, LARGE_PAGE, PAGE};
use crate::percpu::{self, PerCpu};
use crate::{apic, interrupts, time};

const PRESENT: u64 = 1;
const WRITABLE: u64 = 1 << 1;
const USER: u64 = 1 << 2;
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

/// Where the SMEP and SMAP self-tests map their user page.
const USER_TEST_PAGE: u64 = 0x4000_0000;

/// Above this many pages a shootdown flushes the whole TLB instead.
const FLUSH_ALL_PAGES: u64 = 32;

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

/// Why a mapping could not be made.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MapError {
    /// No memory for a page table.
    NoMemory,
    /// A large page already maps the address.
    LargePage,
}

struct State {
    /// Physical address of the kernel's PML4.
    pml4: u64,
    /// The no-execute bit, or 0 if the CPU lacks it.
    nx: u64,
    /// The global bit, or 0 if the CPU lacks it.
    global: u64,
    /// Free space in the mapping window.
    window: Ranges,
}

static STATE: SpinLock<State> =
    SpinLock::new(State { pml4: 0, nx: 0, global: 0, window: Ranges::new() });

/// Whether the kernel's own tables are live (page tables may then come
/// from anywhere in RAM; before, only from the first 4 GiB).
static SWITCHED: AtomicBool = AtomicBool::new(false);
/// Physical address of the kernel's PML4, readable without the lock.
static KERNEL_PML4: AtomicU64 = AtomicU64::new(0);

/// Protection features in use.
pub static SMEP: AtomicBool = AtomicBool::new(false);
/// See [`SMEP`].
pub static SMAP: AtomicBool = AtomicBool::new(false);
/// See [`SMEP`].
pub static PCID: AtomicBool = AtomicBool::new(false);
/// The no-execute bit of user pages, or 0 without NX. Kept here because
/// asking CPUID is a VM exit under a hypervisor, too slow for every page.
static USER_NX: AtomicU64 = AtomicU64::new(0);
/// Whether the direct map uses 1 GiB pages.
static HUGE_DIRECT_MAP: AtomicBool = AtomicBool::new(false);
/// The W^X check: 4 KiB pages checked, and how many of them were both
/// writable and executable.
static WX_AUDIT: [AtomicU64; 2] = [AtomicU64::new(0), AtomicU64::new(0)];

/// A page table, reached through the direct map.
pub(crate) fn table(phys: u64) -> &'static mut [u64; 512] {
    // SAFETY: `phys` is a page-table page the kernel allocated, in RAM
    // that the direct map (or the boot alias of its first 4 GiB, where
    // tables come from before the kernel's tables are live) reaches. Page
    // tables are only changed with `STATE` (or their address space's
    // lock) held, and no other Rust reference to them exists.
    unsafe { &mut *(memory::phys_to_virt(phys) as *mut [u64; 512]) }
}

/// A zeroed page for a page table. A PML4 must be below 4 GiB (the AP
/// trampoline loads CR3 in 32-bit mode), as must every table before the
/// kernel's tables are live.
pub(crate) fn new_table(pml4: bool) -> Result<u64, MapError> {
    let low = pml4 || !SWITCHED.load(Ordering::Relaxed);
    memory::alloc_pages(1, low, Owner::PageTable).ok_or(MapError::NoMemory)
}

/// The entry for `virt` in the table at `level` (4 = PML4, 3 = PDPT,
/// 2 = page directory, 1 = page table) under the PML4 at `root`, creating
/// tables on the way down (reachable from user mode if `user`).
pub(crate) fn entry(
    root: u64,
    virt: u64,
    level: u32,
    user: bool,
) -> Result<&'static mut u64, MapError> {
    let mut table_phys = root;
    for current in ((level + 1)..=4).rev() {
        let index = ((virt >> (12 + 9 * (current - 1))) & 0x1ff) as usize;
        let slot = &mut table(table_phys)[index];
        if *slot & PRESENT == 0 {
            *slot = new_table(false)? | PRESENT | WRITABLE | if user { USER } else { 0 };
        } else if *slot & LARGE != 0 {
            return Err(MapError::LargePage);
        } else if user {
            *slot |= USER;
        }
        table_phys = *slot & ADDRESS;
    }
    let index = ((virt >> (12 + 9 * (level - 1))) & 0x1ff) as usize;
    Ok(&mut table(table_phys)[index])
}

/// The 4 KiB leaf entry for `virt` under `root`, if its tables exist.
pub(crate) fn leaf(root: u64, virt: u64) -> Option<&'static mut u64> {
    let mut table_phys = root;
    for level in (1..4).rev() {
        let index = ((virt >> (12 + 9 * level)) & 0x1ff) as usize;
        let slot = table(table_phys)[index];
        if slot & PRESENT == 0 || slot & LARGE != 0 {
            return None;
        }
        table_phys = slot & ADDRESS;
    }
    Some(&mut table(table_phys)[((virt >> 12) & 0x1ff) as usize])
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

/// The flags of a user page: no global bit, so that it is tagged with its
/// address space.
pub(crate) fn user_flags(access: Access) -> u64 {
    let nx = USER_NX.load(Ordering::Relaxed);
    match access {
        Access::Code => PRESENT | USER,
        Access::ReadOnly => PRESENT | USER | nx,
        Access::ReadWrite => PRESENT | USER | WRITABLE | nx,
    }
}

/// The access rights a user page-table entry grants.
pub(crate) fn access_of(entry: u64) -> Access {
    if entry & WRITABLE != 0 {
        Access::ReadWrite
    } else if entry & NO_EXECUTE != 0 || USER_NX.load(Ordering::Relaxed) == 0 {
        // Without NX every readable page is executable; it counts as
        // read-only, since that is what was asked for.
        Access::ReadOnly
    } else {
        Access::Code
    }
}

fn map_4k(root: u64, virt: u64, phys: u64, flags: u64) -> Result<(), MapError> {
    *entry(root, virt, 1, flags & USER != 0)? = phys | flags;
    Ok(())
}

fn map_large(root: u64, virt: u64, phys: u64, flags: u64, level: u32) -> Result<(), MapError> {
    // In large-page entries the PAT bit moves to bit 12; the kernel only
    // maps write-back memory with large pages, so it stays clear.
    *entry(root, virt, level, false)? = phys | flags | LARGE;
    Ok(())
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

/// Address of a linker symbol (where it runs, after KASLR moved it).
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
pub fn bsp_stack_guard() -> Range<usize> {
    let guard = symbol!(BSP_STACK_GUARD) as usize;
    guard..guard + PAGE as usize
}

/// Programs the page attribute table and enables no-execute, global pages,
/// write protection, SMEP, SMAP and PCIDs on this CPU, as far as the boot
/// CPU chose to use them. Every CPU runs this before using the kernel's
/// tables.
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
    let mut cr4 = cpu::cr4();
    if cpu::feature::pge() {
        cr4 |= 1 << 7;
    }
    if SMEP.load(Ordering::Relaxed) {
        cr4 |= 1 << 20;
    }
    if SMAP.load(Ordering::Relaxed) {
        cr4 |= 1 << 21;
    }
    // PCIDE may only be set while CR3 names PCID 0, which holds here: the
    // boot tables and the kernel's tables are both loaded with PCID 0.
    if PCID.load(Ordering::Relaxed) {
        cr4 |= 1 << 17;
    }
    // SAFETY: every bit set is supported (CPUID says so, or the boot CPU
    // checked before setting the flag); PGE lets global entries survive
    // CR3 loads, SMEP and SMAP only forbid the kernel user pages, which it
    // does not use, and PCIDE only tags TLB entries.
    unsafe { cpu::write_cr4(cr4) };
    // CR0.WP (bit 16): the kernel cannot write read-only pages either.
    // SAFETY: the kernel never writes to its read-only mappings.
    unsafe { cpu::write_cr0(cpu::cr0() | (1 << 16)) };
}

/// Builds the kernel's page tables and switches to them. Called once, by
/// the boot CPU, after the memory map, the layout and the bump allocator
/// are ready. `use_pcid` is false with the `nopcid` flag.
pub fn build_and_switch(use_pcid: bool) {
    let mut state = STATE.lock();
    state.nx = if cpu::feature::nx() { NO_EXECUTE } else { 0 };
    USER_NX.store(state.nx, Ordering::Relaxed);
    state.global = if cpu::feature::pge() { GLOBAL } else { 0 };
    SMEP.store(cpu::feature::smep(), Ordering::Relaxed);
    SMAP.store(cpu::feature::smap(), Ordering::Relaxed);
    // Kernel mappings are global, so INVLPG reaches them whatever PCID is
    // current; without global pages PCIDs would need INVPCID instead.
    PCID.store(use_pcid && cpu::feature::pcid() && cpu::feature::pge(), Ordering::Relaxed);
    let root = new_table(true).expect("out of memory for the kernel's page tables");
    state.pml4 = root;
    KERNEL_PML4.store(root, Ordering::Relaxed);
    let fail = |err: MapError| -> ! { panic!("building the kernel's page tables: {err:?}") };

    // The kernel image.
    let guard = bsp_stack_guard();
    let text = symbol!(__kernel_text_start)..symbol!(__kernel_text_end);
    let rodata = symbol!(__kernel_rodata_start)..symbol!(__kernel_rodata_end);
    let data = symbol!(__kernel_data_start)..symbol!(__kernel_data_end);
    for (range, access) in [
        (text.clone(), Access::Code),
        (rodata.clone(), Access::ReadOnly),
        (data, Access::ReadWrite),
    ] {
        let flags = leaf_flags(&state, access, Cache::WriteBack);
        for virt in range.step_by(PAGE as usize) {
            if !guard.contains(&(virt as usize)) {
                map_4k(root, virt, memory::image_phys(virt), flags).unwrap_or_else(|e| fail(e));
            }
        }
    }

    // The direct map of RAM. The image's text and read-only data stay
    // read-only in it, so they cannot be changed through that alias.
    let base = memory::direct_map();
    let read_only = memory::image_phys(text.start)..memory::image_phys(rodata.end);
    let (rw, ro) = (
        leaf_flags(&state, Access::ReadWrite, Cache::WriteBack),
        leaf_flags(&state, Access::ReadOnly, Cache::WriteBack),
    );
    let huge = cpu::feature::huge_pages();
    HUGE_DIRECT_MAP.store(huge, Ordering::Relaxed);
    let limit = memory::DIRECT_MAP_AREA.end - base;
    for region in memory::memory_map() {
        if !memory::is_ram(region.start..region.end) {
            continue;
        }
        let mut phys = region.start & !(PAGE - 1);
        let end = region.end.next_multiple_of(PAGE).min(limit);
        while phys < end {
            let fits = |size: u64| {
                phys.is_multiple_of(size)
                    && phys + size <= end
                    && (phys + size <= read_only.start || read_only.end <= phys)
            };
            // Overlapping memory map entries may map a page twice: the
            // direct map is linear, so the first mapping is as good.
            let (size, result) = if huge && fits(HUGE_PAGE) {
                (HUGE_PAGE, map_large(root, base + phys, phys, rw, 3))
            } else if fits(LARGE_PAGE) {
                (LARGE_PAGE, map_large(root, base + phys, phys, rw, 2))
            } else {
                let flags = if read_only.contains(&phys) { ro } else { rw };
                (PAGE, map_4k(root, base + phys, phys, flags))
            };
            match result {
                Ok(()) | Err(MapError::LargePage) => {}
                Err(err) => fail(err),
            }
            phys += size;
        }
    }

    // The mapping window's PDPTs, created now so that the kernel's half of
    // the PML4 never changes and address spaces can share it.
    let window = memory::window();
    for virt in [window, window + memory::WINDOW_SIZE - 1] {
        entry(root, virt, 3, false).unwrap_or_else(|e| fail(e));
    }
    state.window.init(window, window + memory::WINDOW_SIZE);

    init_cpu();
    // SAFETY: the new tables map the kernel image where it runs (so the
    // running code, the stack and all statics stay put) and RAM in the
    // direct map at the same addresses the boot alias used, so every
    // pointer made from boot information stays valid. The boot CPU's GDT
    // and IDT are in the kernel image and already loaded.
    unsafe { cpu::write_cr3(root) };
    SWITCHED.store(true, Ordering::Relaxed);
    let (checked, writable_code) = audit(root, 4, 0, (true, true), state.nx);
    WX_AUDIT[0].store(checked, Ordering::Relaxed);
    WX_AUDIT[1].store(writable_code, Ordering::Relaxed);
}

/// Walks the table at `phys` (at `level`, mapping from `base`), returning
/// how many 4 KiB pages it maps and how many of them are writable and
/// executable. `(writable, executable)` is what the levels above allow;
/// `nx` is the no-execute bit in use.
fn audit(phys: u64, level: u32, base: u64, above: (bool, bool), nx: u64) -> (u64, u64) {
    let (mut checked, mut bad) = (0, 0);
    for (index, &entry) in table(phys).iter().enumerate() {
        if entry & PRESENT == 0 {
            continue;
        }
        let writable = above.0 && entry & WRITABLE != 0;
        let executable = above.1 && (nx == 0 || entry & NO_EXECUTE == 0);
        let virt = base | ((index as u64) << (12 + 9 * (level - 1)));
        if level == 1 || entry & LARGE != 0 {
            let pages = 1u64 << (9 * (level - 1));
            checked += pages;
            if writable && executable {
                bad += pages;
            }
        } else {
            let (c, b) = audit(entry & ADDRESS, level - 1, virt, (writable, executable), nx);
            checked += c;
            bad += b;
        }
    }
    (checked, bad)
}

/// Physical address of the kernel's PML4, for the AP trampoline and new
/// address spaces.
pub fn kernel_pml4() -> u64 {
    KERNEL_PML4.load(Ordering::Relaxed)
}

/// Takes `pages` pages of the mapping window plus a guard page below them
/// and returns the first page after the guard.
fn window_alloc(state: &mut State, pages: u64) -> Option<u64> {
    let start = state.window.alloc((pages + 1) * PAGE, PAGE)?;
    Some(start + PAGE)
}

/// Maps `len` bytes of physical memory from `phys` into the mapping window
/// and returns the virtual address of `phys`. For device memory and
/// firmware tables at boot; panics if the window or memory is exhausted.
pub fn map_physical(phys: u64, len: u64, access: Access, cache: Cache) -> usize {
    let mut state = STATE.lock();
    let first = phys & !(PAGE - 1);
    let last = (phys + len.max(1)).next_multiple_of(PAGE);
    let virt = window_alloc(&mut state, (last - first) / PAGE).expect("the mapping window is full");
    let flags = leaf_flags(&state, access, cache);
    for offset in (0..last - first).step_by(PAGE as usize) {
        map_4k(state.pml4, virt + offset, first + offset, flags)
            .expect("out of memory for page tables");
    }
    (virt + (phys - first)) as usize
}

/// Allocates a stack of `pages` pages in the mapping window, with an
/// unmapped guard page below it. Returns (guard, stack).
pub fn allocate_stack(pages: u64) -> (Range<usize>, Range<usize>) {
    let area = map_area(pages as usize).expect("out of memory for a stack") as u64;
    let guard = area - PAGE;
    (guard as usize..area as usize, area as usize..(area + pages * PAGE) as usize)
}

/// Maps `pages` pages of fresh zeroed memory, writable, into the mapping
/// window with an unmapped guard page below, and returns the address of
/// the first. Fails without side effects when memory or the window runs
/// out. [`unmap_area`] gives it all back.
pub fn map_area(pages: usize) -> Result<usize, AllocError> {
    let pages = pages as u64;
    if pages == 0 {
        return Err(AllocError);
    }
    let mut state = STATE.lock();
    let area = window_alloc(&mut state, pages).ok_or(AllocError)?;
    let flags = leaf_flags(&state, Access::ReadWrite, Cache::WriteBack);
    for page in 0..pages {
        let virt = area + page * PAGE;
        let mapped = match memory::alloc_pages(1, false, Owner::Area) {
            Some(phys) => map_4k(state.pml4, virt, phys, flags).inspect_err(|_| {
                memory::free_pages(phys, 1, Owner::Area);
            }),
            None => Err(MapError::NoMemory),
        };
        if mapped.is_err() {
            drop(state);
            // Give back what was mapped, then the rest of the reservation.
            let rest = if page > 0 {
                unmap_area(area as usize, page as usize);
                (area + page * PAGE, (pages - page) * PAGE)
            } else {
                (area - PAGE, (pages + 1) * PAGE)
            };
            let _ = STATE.lock().window.release(rest.0, rest.1);
            return Err(AllocError);
        }
    }
    Ok(area as usize)
}

/// Unmaps an area from [`map_area`] (or [`allocate_stack`]), flushes it
/// from every CPU's TLB, and frees its memory and its place in the window.
pub fn unmap_area(virt: usize, pages: usize) {
    let (area, pages) = (virt as u64, pages as u64);
    // Mark the pages not present first, keeping their frame addresses:
    // the frames are freed only once no CPU can reach them any more.
    let root = {
        let state = STATE.lock();
        for page in 0..pages {
            match leaf(state.pml4, area + page * PAGE) {
                Some(slot) if *slot & PRESENT != 0 => *slot &= !PRESENT,
                _ => panic!("unmapping {:#x}: not a mapped area", area + page * PAGE),
            }
        }
        state.pml4
    };
    shootdown(area..area + pages * PAGE);
    let mut state = STATE.lock();
    for page in 0..pages {
        if let Some(slot) = leaf(root, area + page * PAGE) {
            let phys = *slot & ADDRESS;
            *slot = 0;
            memory::free_pages(phys, 1, Owner::Area);
        }
    }
    if let Err(err) = state.window.release(area - PAGE, (pages + 1) * PAGE) {
        panic!("unmapping {area:#x}: {err:?}");
    }
}

/// Points the page at `virt` (from [`map_area`]) to frame `phys` on every
/// CPU and returns the frame it mapped before, for the `tlb` self-test.
pub fn remap(virt: u64, phys: u64) -> u64 {
    let state = STATE.lock();
    let slot = leaf(state.pml4, virt).filter(|slot| **slot & PRESENT != 0);
    let Some(slot) = slot else { panic!("remapping {virt:#x}: not mapped") };
    let old = core::mem::replace(slot, (*slot & !ADDRESS) | phys) & ADDRESS;
    drop(state);
    shootdown(virt..virt + PAGE);
    old
}

/// Identity-maps one low page, read-only and executable, for the AP
/// trampoline (the boot CPU writes it through the direct map).
pub fn map_identity(phys: u64) {
    let state = STATE.lock();
    map_4k(state.pml4, phys, phys, PRESENT).expect("out of memory for page tables");
}

/// Removes an identity mapping made by [`map_identity`] from every CPU.
pub fn unmap_identity(phys: u64) {
    let state = STATE.lock();
    if let Some(slot) = leaf(state.pml4, phys) {
        *slot = 0;
    }
    drop(state);
    shootdown(phys..phys + PAGE);
}

/// Maps a page for the SMEP or SMAP self-test at a user address: a `ret`
/// instruction, executable, for SMEP; data for SMAP. Returns its address.
pub fn map_user_test_page(code: bool) -> usize {
    let phys = memory::alloc_pages(1, false, Owner::Kernel).expect("out of memory");
    // SAFETY: the page was just allocated for this test and is RAM in the
    // direct map.
    unsafe { (memory::phys_to_virt(phys) as *mut u8).write(0xc3) };
    let state = STATE.lock();
    let access = if code { Access::Code } else { Access::ReadOnly };
    map_4k(state.pml4, USER_TEST_PAGE, phys, user_flags(access)).expect("out of memory");
    // The table above the page's tables may already exist without USER
    // (the AP start-up page's identity mapping shares it), and the CPU may
    // have cached it that way, which would make this a kernel page and
    // the test pass for the wrong reason. INVLPG drops cached table
    // entries as well as the page's own.
    cpu::invlpg(USER_TEST_PAGE as usize);
    USER_TEST_PAGE as usize
}

/// The TLB shootdown in progress.
struct Shootdown {
    /// Held by the CPU that sends a request until every target answered.
    lock: SpinLock<()>,
    /// Number of the latest request.
    generation: AtomicU64,
    start: AtomicU64,
    end: AtomicU64,
    /// CPUs that still have to flush, one bit each.
    pending: [AtomicU64; MAX_CPUS / 64],
}

static SHOOTDOWN: Shootdown = Shootdown {
    lock: SpinLock::new(()),
    generation: AtomicU64::new(0),
    start: AtomicU64::new(0),
    end: AtomicU64::new(0),
    pending: [const { AtomicU64::new(0) }; MAX_CPUS / 64],
};

/// DIAG (temporary, removed before merge): shootdowns sent; nanoseconds
/// in the IPI writes, waiting for the targets, from before the write to a
/// target's handler, in the target's flush, in its EOI; EOIs the
/// hypervisor said needed no write.
pub static SHOOT_DIAG: [AtomicU64; 7] = [const { AtomicU64::new(0) }; 7];
/// DIAG: when the last TLB interrupt handler started.
pub static SHOOT_SEEN: AtomicU64 = AtomicU64::new(0);

/// Flushes `range` from this CPU's TLB.
pub(crate) fn flush_local(range: Range<u64>) {
    if (range.end - range.start) / PAGE > FLUSH_ALL_PAGES {
        cpu::flush_tlb_all();
    } else {
        for page in range.step_by(PAGE as usize) {
            cpu::invlpg(page as usize);
        }
    }
}

/// Flushes `range` from the TLB of every online CPU, waiting until all
/// have done so. Call with no spin lock held that another CPU might be
/// waiting for, since that CPU could not answer.
pub fn shootdown(range: Range<u64>) {
    shootdown_on(range, |_| true);
}

/// Like [`shootdown`], for the other online CPUs `wanted` picks.
pub(crate) fn shootdown_on(range: Range<u64>, wanted: impl Fn(&PerCpu) -> bool) {
    flush_local(range.clone());
    let me = percpu::current_index();
    // Two CPUs may shoot down at once: the one waiting for the lock keeps
    // answering the other's request.
    let guard = loop {
        if let Some(guard) = SHOOTDOWN.lock.try_lock() {
            break guard;
        }
        serve_shootdown();
        cpu::pause();
    };
    let mut targets = [0u64; MAX_CPUS / 64];
    for (index, cpu) in (0..MAX_CPUS).filter_map(|i| Some((i, percpu::get(i)?))) {
        if index != me
            && cpu.online.load(Ordering::Acquire)
            && !cpu.stopped.load(Ordering::Relaxed)
            && wanted(cpu)
        {
            targets[index / 64] |= 1 << (index % 64);
        }
    }
    if targets.iter().all(|&word| word == 0) {
        return;
    }
    // Only a request that is sent takes a number: a CPU that marked a
    // number as its own without sending it would take the next request
    // under that number for one it had already served, and never flush.
    let generation = SHOOTDOWN.generation.load(Ordering::Relaxed) + 1;
    percpu::current().tlb_generation.store(generation, Ordering::Relaxed);
    SHOOTDOWN.start.store(range.start, Ordering::Relaxed);
    SHOOTDOWN.end.store(range.end, Ordering::Relaxed);
    for (slot, &word) in SHOOTDOWN.pending.iter().zip(&targets) {
        slot.store(word, Ordering::Relaxed);
    }
    SHOOTDOWN.generation.store(generation, Ordering::Release);
    let t0 = time::now_ns();
    for index in (0..MAX_CPUS).filter(|&i| targets[i / 64] & (1 << (i % 64)) != 0) {
        if let Some(cpu) = percpu::get(index) {
            apic::send_fixed(cpu.apic_id.load(Ordering::Relaxed), interrupts::VECTOR_TLB);
        }
    }
    let started = time::now_ns();
    while let Some(word) = SHOOTDOWN.pending.iter().position(|w| w.load(Ordering::Acquire) != 0) {
        if time::now_ns().saturating_sub(started) > 2_000_000_000 {
            let bits = SHOOTDOWN.pending[word].load(Ordering::Relaxed);
            panic!(
                "TLB shootdown: CPU {} did not flush {:#x}-{:#x} within 2 s",
                word * 64 + bits.trailing_zeros() as usize,
                range.start,
                range.end
            );
        }
        cpu::pause();
    }
    let t2 = time::now_ns();
    SHOOT_DIAG[0].fetch_add(1, Ordering::Relaxed);
    SHOOT_DIAG[1].fetch_add(started - t0, Ordering::Relaxed);
    SHOOT_DIAG[2].fetch_add(t2 - started, Ordering::Relaxed);
    SHOOT_DIAG[3]
        .fetch_add(SHOOT_SEEN.load(Ordering::Relaxed).saturating_sub(t0), Ordering::Relaxed);
    drop(guard);
}

/// Carries out the latest shootdown request if it is meant for this CPU
/// and not done yet. The TLB IPI and the timer tick call it.
pub fn serve_shootdown() {
    let generation = SHOOTDOWN.generation.load(Ordering::Acquire);
    let cpu = percpu::current();
    if cpu.tlb_generation.swap(generation, Ordering::Relaxed) == generation {
        return;
    }
    let index = cpu.index();
    let bit = 1u64 << (index % 64);
    let pending = &SHOOTDOWN.pending[index / 64];
    if pending.load(Ordering::Relaxed) & bit != 0 {
        let start = SHOOTDOWN.start.load(Ordering::Relaxed);
        let end = SHOOTDOWN.end.load(Ordering::Relaxed);
        flush_local(start..end);
        pending.fetch_and(!bit, Ordering::Release);
    }
}

/// The page-table entries the CPU walks for `virt` under the current CR3,
/// top level first, stopping at a missing or large page (0 after it). For
/// self-test diagnostics.
pub fn walk(virt: usize) -> [u64; 4] {
    let mut entries = [0; 4];
    let mut table_phys = cpu::cr3() & ADDRESS;
    for (slot, level) in entries.iter_mut().zip((0..=3).rev()) {
        let index = ((virt as u64 >> (12 + 9 * level)) & 0x1ff) as usize;
        *slot = table(table_phys)[index];
        if *slot & PRESENT == 0 || *slot & LARGE != 0 {
            break;
        }
        table_phys = *slot & ADDRESS;
    }
    entries
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

/// Writes the protection summary for the boot report.
pub fn describe(line: &mut dyn FnMut(core::fmt::Arguments<'_>)) {
    let (checked, bad) = (WX_AUDIT[0].load(Ordering::Relaxed), WX_AUDIT[1].load(Ordering::Relaxed));
    let on = |flag: &AtomicBool| if flag.load(Ordering::Relaxed) { "on" } else { "off" };
    if !cpu::feature::nx() {
        line(format_args!("Paging: W^X not enforced: this CPU has no no-execute bit"));
    } else if bad == 0 {
        line(format_args!(
            "Paging: W^X holds: none of {} MiB of kernel mappings is writable and executable",
            (checked * PAGE) >> 20
        ));
    } else {
        line(format_args!(
            "Paging: W^X violated: {bad} pages of kernel mappings are writable and executable"
        ));
    }
    line(format_args!(
        "Paging: SMEP {}, SMAP {}, PCID {}; direct map in {} pages",
        on(&SMEP),
        on(&SMAP),
        on(&PCID),
        if HUGE_DIRECT_MAP.load(Ordering::Relaxed) { "1 GiB" } else { "2 MiB" }
    ));
}
