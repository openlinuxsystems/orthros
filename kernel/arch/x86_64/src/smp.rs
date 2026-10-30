// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Starting the other CPUs (application processors, APs).
//!
//! The boot CPU copies a small trampoline to a page below 1 MiB and wakes
//! each AP in turn with INIT and start-up IPIs. The AP starts there in
//! 16-bit real mode, switches to protected mode and then long mode on the
//! kernel's own page tables (which map the trampoline page at its physical
//! address while the APs start), and jumps to [`ap_entry`] in the higher
//! half on a stack the boot CPU allocated for it. The APs start one at a
//! time, so they can share the trampoline's parameter block.

use core::sync::atomic::{AtomicBool, Ordering};

use radix_hal::{CpuInfo, MAX_CPUS};
use radix_sync::Once;

use crate::fpu;
use crate::memory::{self, PAGE};
use crate::paging;
use crate::percpu::{self, IST_STACKS};
use crate::{acpi, apic, cpu, interrupts, kvm, mce, microcode, time};

core::arch::global_asm!(
    r#"
    .section .rodata.ap_trampoline, "a"
    .balign 16
    .global ap_trampoline_start
ap_trampoline_start:
    .code16
    cli
    cld
    xorl %ebx, %ebx
    movw %cs, %bx
    movw %bx, %ds
    shll $4, %ebx
    lgdtl (ap_gdt_ptr - ap_trampoline_start)
    movl %cr0, %eax
    orl $1, %eax
    movl %eax, %cr0
    ljmpl *(ap_far32 - ap_trampoline_start)

    .code32
    .global ap_trampoline_32
ap_trampoline_32:
    movw $0x10, %ax
    movw %ax, %ds
    movw %ax, %es
    movw %ax, %ss
    movl (ap_cr3 - ap_trampoline_start)(%ebx), %eax
    movl %eax, %cr3
    movl %cr4, %eax
    orl $(1 << 5), %eax
    movl %eax, %cr4
    movl $0xC0000080, %ecx
    rdmsr
    orl $(1 << 8), %eax
    testl $1, (ap_nx - ap_trampoline_start)(%ebx)
    jz 1f
    orl $(1 << 11), %eax
1:
    wrmsr
    movl %cr0, %eax
    orl $((1 << 31) | (1 << 16)), %eax
    movl %eax, %cr0
    ljmpl *(ap_far64 - ap_trampoline_start)(%ebx)

    .code64
    .global ap_trampoline_64
ap_trampoline_64:
    movl %ebx, %ebx
    movq (ap_stack - ap_trampoline_start)(%rbx), %rsp
    movq (ap_index - ap_trampoline_start)(%rbx), %rdi
    movq (ap_entry_address - ap_trampoline_start)(%rbx), %rax
    xorl %ebp, %ebp
    pushq $0
    jmp *%rax

    .balign 8
    .global ap_gdt
ap_gdt:
    /* The accessed bits are set, so loading a segment never writes here:
       the page is mapped read-only once paging is on. */
    .quad 0
    .quad 0x00CF9B000000FFFF    /* 0x08: 32-bit code */
    .quad 0x00CF93000000FFFF    /* 0x10: data */
    .quad 0x00AF9B000000FFFF    /* 0x18: 64-bit code */
ap_gdt_end:
    .balign 8
    .global ap_gdt_ptr
ap_gdt_ptr:
    .word ap_gdt_end - ap_gdt - 1
    .long 0                     /* patched: linear address of ap_gdt */
    .balign 8
    .global ap_far32
ap_far32:
    .long 0                     /* patched: linear address of ap_trampoline_32 */
    .word 0x08
    .balign 8
    .global ap_far64
ap_far64:
    .long 0                     /* patched: linear address of ap_trampoline_64 */
    .word 0x18
    .balign 8
    .global ap_cr3
ap_cr3:
    .long 0
    .global ap_nx
ap_nx:
    .long 0
    .global ap_stack
ap_stack:
    .quad 0
    .global ap_index
ap_index:
    .quad 0
    .global ap_entry_address
ap_entry_address:
    .quad 0
    .global ap_trampoline_end
ap_trampoline_end:
    .code64
    .text
"#,
    options(att_syntax)
);

unsafe extern "C" {
    static ap_trampoline_start: u8;
    static ap_trampoline_end: u8;
    static ap_trampoline_32: u8;
    static ap_trampoline_64: u8;
    static ap_gdt: u8;
    static ap_gdt_ptr: u8;
    static ap_far32: u8;
    static ap_far64: u8;
    static ap_cr3: u8;
    static ap_nx: u8;
    static ap_stack: u8;
    static ap_index: u8;
    static ap_entry_address: u8;
}

/// Offset of a trampoline symbol from its start.
macro_rules! offset {
    ($name:ident) => {
        (&raw const $name as usize) - (&raw const ap_trampoline_start as usize)
    };
}

/// Kernel stack size of each AP, in pages.
const KERNEL_STACK_PAGES: u64 = 16;
/// Size of each interrupt stack, in pages.
const IST_STACK_PAGES: u64 = 4;

/// One CPU the firmware lists.
#[derive(Clone, Copy, Debug)]
struct CpuEntry {
    apic_id: u32,
    acpi_uid: Option<u32>,
}

struct CpuTable {
    entries: [CpuEntry; MAX_CPUS],
    count: usize,
}

static CPUS: Once<CpuTable> = Once::new();
static TRAMPOLINE: Once<u64> = Once::new();
static ENTRY: Once<fn(usize) -> !> = Once::new();
/// Set while an AP runs its bring-up, cleared when it is online.
static STARTING: AtomicBool = AtomicBool::new(false);

/// Records the low page the trampoline runs from (already reserved).
pub fn set_trampoline_page(phys: u64) {
    let _ = TRAMPOLINE.set(phys);
}

/// Builds the CPU table from the MADT: the boot CPU first, then the other
/// usable CPUs in the order the table lists them.
pub fn discover_cpus() {
    let boot_id = apic::id();
    let mut table =
        CpuTable { entries: [CpuEntry { apic_id: 0, acpi_uid: None }; MAX_CPUS], count: 1 };
    table.entries[0] = CpuEntry { apic_id: boot_id, acpi_uid: None };
    let madt = acpi::get().and_then(|acpi| acpi.madt.as_ref());
    for entry in madt.into_iter().flat_map(radix_acpi::Madt::entries) {
        if let radix_acpi::MadtEntry::LocalApic { processor_uid, apic_id, enabled: true, .. } =
            entry
        {
            // A CPU listed twice (as xAPIC and x2APIC), or out of reach of
            // xAPIC's 8-bit destinations, is left out.
            let listed = table.entries[1..table.count].iter().any(|e| e.apic_id == apic_id);
            let reachable = apic_id <= 0xfe || apic::is_x2apic();
            if apic_id == boot_id {
                table.entries[0].acpi_uid = Some(processor_uid);
            } else if !listed && reachable && table.count < MAX_CPUS {
                table.entries[table.count] = CpuEntry { apic_id, acpi_uid: Some(processor_uid) };
                table.count += 1;
            }
        }
    }
    for (index, entry) in table.entries[..table.count].iter().enumerate() {
        let cpu = percpu::get(index).expect("index below MAX_CPUS");
        cpu.apic_id.store(entry.apic_id, Ordering::Relaxed);
        cpu.present.store(true, Ordering::Relaxed);
    }
    percpu::get(0).expect("CPU 0").online.store(true, Ordering::Relaxed);
    let _ = CPUS.set(table);
}

/// How APIC IDs split into hardware thread, core and package: the number
/// of low bits for the thread, and for thread and core together.
static SHIFTS: Once<(u32, u32)> = Once::new();

/// Reads the APIC ID layout from CPUID: leaf 0xB where the CPU has it,
/// else leaf 1's count of logical CPUs per package.
fn topology_shifts() -> (u32, u32) {
    if cpu::max_leaf() >= 0xb {
        let (mut thread, mut package) = (0, None);
        for level in 0..8 {
            let leaf = cpu::cpuid(0xb, level);
            let kind = (leaf.ecx >> 8) & 0xff;
            if kind == 0 {
                break;
            }
            let shift = leaf.eax & 0x1f;
            if kind == 1 {
                thread = shift;
            } else {
                package = Some(shift);
            }
        }
        if let Some(package) = package {
            return (thread, package.max(thread));
        }
    }
    let leaf = cpu::cpuid(1, 0);
    if leaf.edx & (1 << 28) != 0 {
        let logical = ((leaf.ebx >> 16) & 0xff).max(1);
        return (0, logical.next_power_of_two().trailing_zeros());
    }
    (0, 0)
}

/// The package, core, thread and node of CPU `index`.
pub fn place(index: usize) -> Option<radix_hal::CpuPlace> {
    if index >= count() {
        return None;
    }
    let cpu = percpu::get(index)?;
    let apic_id = cpu.apic_id.load(Ordering::Relaxed);
    let &(thread, package) = SHIFTS.call_once(topology_shifts);
    let mask = |bits: u32| if bits >= 32 { u32::MAX } else { (1u32 << bits) - 1 };
    Some(radix_hal::CpuPlace {
        package: if package >= 32 { 0 } else { apic_id >> package },
        core: (apic_id >> thread) & mask(package - thread),
        thread: apic_id & mask(thread),
        node: cpu.node.load(Ordering::Relaxed),
    })
}

/// Number of CPUs the firmware lists as usable.
pub fn count() -> usize {
    CPUS.get().map_or(1, |table| table.count)
}

/// The ACPI processor UID of CPU `index`.
pub fn acpi_uid(index: usize) -> Option<u32> {
    CPUS.get().and_then(|table| table.entries.get(index)).and_then(|entry| entry.acpi_uid)
}

/// The HAL's view of CPU `index`.
pub fn info(index: usize) -> Option<CpuInfo> {
    let table = CPUS.get()?;
    let entry = table.entries[..table.count].get(index)?;
    Some(CpuInfo {
        index,
        hardware_id: entry.apic_id,
        boot_cpu: index == 0,
        online: percpu::get(index)?.online.load(Ordering::Acquire),
    })
}

/// Allocates CPU `index`'s stacks and records them.
fn allocate_stacks(index: usize) -> usize {
    let (guard, kernel) = paging::allocate_stack(KERNEL_STACK_PAGES);
    let mut ist = [0..0, 0..0, 0..0];
    let mut guards = [guard, 0..0, 0..0, 0..0];
    for slot in 0..IST_STACKS {
        let (guard, stack) = paging::allocate_stack(IST_STACK_PAGES);
        ist[slot] = stack;
        guards[slot + 1] = guard;
    }
    let top = kernel.end;
    let _ = percpu::get(index).expect("index below MAX_CPUS").stacks.set(percpu::Stacks {
        kernel,
        ist,
        guards,
    });
    top
}

/// Writes `value` into the trampoline copy at `offset`.
fn patch(base: usize, offset: usize, value: &[u8]) {
    // SAFETY: `base` is the direct-map address of the reserved trampoline
    // page, and every offset is inside the copied trampoline, which is
    // smaller than a page (checked when copying).
    unsafe {
        core::ptr::copy_nonoverlapping(value.as_ptr(), (base + offset) as *mut u8, value.len());
    };
}

/// Starts every listed CPU but the boot CPU; each calls `entry` with its
/// index once ready. Returns how many CPUs are online.
pub fn start_all(entry: fn(usize) -> !) -> usize {
    let _ = ENTRY.set(entry);
    let total = count();
    let Some(&page) = TRAMPOLINE.get() else {
        radix_klog::warn!("SMP: no low page for the start-up trampoline; using one CPU");
        return 1;
    };
    if total <= 1 {
        return 1;
    }

    // Copy the trampoline and fill in its fixed parameters.
    let size = offset!(ap_trampoline_end);
    assert!(size <= PAGE as usize, "AP trampoline larger than a page");
    let base = memory::phys_to_virt(page);
    // SAFETY: the source is the trampoline in the kernel image, `size`
    // bytes long; the destination is the reserved low page, reached through
    // the direct map.
    unsafe {
        core::ptr::copy_nonoverlapping(&raw const ap_trampoline_start, base as *mut u8, size);
    }
    let linear = |offset: usize| (page as u32 + offset as u32).to_le_bytes();
    patch(base, offset!(ap_gdt_ptr) + 2, &linear(offset!(ap_gdt)));
    patch(base, offset!(ap_far32), &linear(offset!(ap_trampoline_32)));
    patch(base, offset!(ap_far64), &linear(offset!(ap_trampoline_64)));
    patch(base, offset!(ap_cr3), &(paging::kernel_pml4() as u32).to_le_bytes());
    patch(base, offset!(ap_nx), &u32::from(cpu::feature::nx()).to_le_bytes());
    patch(base, offset!(ap_entry_address), &(ap_entry as *const () as u64).to_le_bytes());
    paging::map_identity(page);

    let mut online = 1;
    for index in 1..total {
        let apic_id = percpu::get(index).map_or(0, |cpu| cpu.apic_id.load(Ordering::Relaxed));
        let stack_top = allocate_stacks(index);
        patch(base, offset!(ap_stack), &(stack_top as u64).to_le_bytes());
        patch(base, offset!(ap_index), &(index as u64).to_le_bytes());
        cpu::mfence();
        STARTING.store(true, Ordering::Release);
        radix_klog::tracepoint!("smp.start", index, apic_id);

        apic::send_init(apic_id);
        time::delay_us(10_000);
        let mut started = false;
        for attempt in 0..2 {
            apic::send_startup(apic_id, (page >> 12) as u8);
            // The first SIPI usually suffices; give a slow emulator a whole
            // second before trying the second one.
            let wait_us = if attempt == 0 { 1_000_000 } else { 2_000_000 };
            let mut waited = 0;
            while waited < wait_us && STARTING.load(Ordering::Acquire) {
                time::delay_us(50);
                waited += 50;
            }
            if !STARTING.load(Ordering::Acquire) {
                started = true;
                break;
            }
        }
        if started {
            online += 1;
            radix_klog::debug!("SMP: CPU {index} (APIC ID {apic_id}) is online");
        } else {
            // Hold it in reset: one that started after all would read the
            // next CPU's stack and index from the shared parameter block.
            apic::send_init(apic_id);
            STARTING.store(false, Ordering::Release);
            radix_klog::error!("SMP: CPU {index} (APIC ID {apic_id}) did not start");
        }
    }

    paging::unmap_identity(page);
    online
}

/// Where each AP arrives from the trampoline, on its own stack.
extern "C" fn ap_entry(index: usize) -> ! {
    percpu::set_current(index);
    let cpu = percpu::current();
    let stacks = cpu.stacks.get().expect("stacks are allocated before the AP starts");
    let ist_tops = [stacks.ist[0].end, stacks.ist[1].end, stacks.ist[2].end];
    percpu::load_descriptors(ist_tops);
    interrupts::load();
    // Before anything can use the APIC, a panic's NMIs to the other CPUs
    // included: in x2APIC mode its registers do not exist until then.
    apic::enable();
    paging::init_cpu();
    fpu::init_cpu();
    microcode::load(false);
    mce::init_cpu();
    let madt = acpi::get().and_then(|acpi| acpi.madt.as_ref());
    apic::init_cpu(madt, acpi_uid(index));
    mce::init_cpu_interrupt();
    kvm::init_cpu();
    time::start_tick();
    radix_klog::tracepoint!("smp.online", index, apic::id());
    cpu.online.store(true, Ordering::Release);
    STARTING.store(false, Ordering::Release);
    match ENTRY.get() {
        Some(entry) => entry(index),
        None => cpu::halt_forever(),
    }
}

/// Stops every other online CPU with an NMI and waits up to 100 ms for
/// them. Returns how many stopped.
pub fn stop_others() -> usize {
    let others = (0..count())
        .filter(|&i| i != percpu::current_index())
        .filter(|&i| percpu::get(i).is_some_and(|cpu| cpu.online.load(Ordering::Relaxed)))
        .count();
    if others == 0 {
        return 0;
    }
    interrupts::STOPPING.store(true, Ordering::Release);
    apic::send_nmi_to_others();
    for _ in 0..1000 {
        if interrupts::STOPPED.load(Ordering::Acquire) >= others {
            break;
        }
        time::delay_us(100);
    }
    interrupts::STOPPED.load(Ordering::Acquire)
}
