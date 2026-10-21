// SPDX-FileCopyrightText: 2026 The OrthrOS contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Per-CPU data, the global descriptor table and the task-state segment.
//!
//! Every CPU has a [`PerCpu`] in a static array; its GS base register points
//! at its own entry, so `gs:[8]` is the running CPU's index, `gs:[16]` its
//! preemption count and `gs:[20]` its interrupt depth, each changed by one
//! instruction that a migration cannot split. `gs:[24]` is the top of the
//! running thread's kernel stack and `gs:[32]` a scratch word, which the
//! system call entry uses before it has a stack (`crate::user`). The boot
//! CPU sets GS base first thing in [`crate::entry`], before any code that
//! asks which CPU it is running on. In user mode GS base is the program's
//! (zero); the entry paths swap it with `swapgs`.
//!
//! Each CPU also has its own GDT and TSS. The GDT has the kernel's code and
//! data segments, then the user segments in the order SYSRET needs them.
//! The TSS holds the stack an interrupt from user mode switches to (the
//! running thread's kernel stack, set at every context switch) and the
//! interrupt stack table: double faults, NMIs and machine checks switch to
//! dedicated stacks, so a kernel stack overflow still produces a report.

use core::arch::asm;
use core::ops::Range;
use core::sync::atomic::{AtomicBool, AtomicU8, AtomicU32, AtomicU64, AtomicUsize, Ordering};

use radix_hal::{Fault, MAX_CPUS};
use radix_sync::{Once, SpinLock, TakeOnce};

use crate::cpu::{self, msr};

/// Code segment selector.
pub const KERNEL_CS: u16 = 0x08;
/// Data segment selector.
pub const KERNEL_DS: u16 = 0x10;
/// The selector SYSRET's user selectors are relative to: user data is 8
/// above it and user code 16 above. It carries RPL 3 itself, because only
/// Intel CPUs force SYSRET's SS selector to RPL 3: AMD ones load it as it
/// is, and a program's SS of 0x20 (RPL 0) under a CS of RPL 3 makes the
/// `iretq` that ends the next interrupt from user mode fault.
pub const USER_BASE: u16 = 0x18 | 3;
/// User data (and stack) segment selector, RPL 3.
pub const USER_DS: u16 = 0x20 | 3;
/// User 64-bit code segment selector, RPL 3.
pub const USER_CS: u16 = 0x28 | 3;
/// Task-state segment selector.
const TSS_SELECTOR: u16 = 0x30;

// SYSRET loads the user selectors from `USER_BASE`; interrupt returns to
// user mode take them from the frame, which the CPU pushed with these.
const _: () = assert!(USER_DS == USER_BASE + 8 && USER_CS == USER_BASE + 16);

/// Offset of [`PerCpu`]'s top-of-kernel-stack word, for the entry code.
pub const KERNEL_RSP_OFFSET: usize = 24;
/// Offset of [`PerCpu`]'s scratch word, for the entry code.
pub const SCRATCH_OFFSET: usize = 32;

/// Interrupt stack table slots (1-based, as the IDT names them).
pub const IST_DOUBLE_FAULT: u8 = 1;
pub const IST_NMI: u8 = 2;
pub const IST_MACHINE_CHECK: u8 = 3;
/// Number of IST stacks each CPU has.
pub const IST_STACKS: usize = 3;

/// The kernel stacks of one CPU, for backtraces and overflow reports.
#[derive(Clone, Debug)]
pub struct Stacks {
    /// The ordinary kernel stack.
    pub kernel: Range<usize>,
    /// The interrupt stack table stacks.
    pub ist: [Range<usize>; IST_STACKS],
    /// Guard pages below each stack (unmapped), same order: kernel, then
    /// the IST stacks. Empty ranges where there is none.
    pub guards: [Range<usize>; IST_STACKS + 1],
}

impl Stacks {
    /// The stack containing `addr`.
    pub fn containing(&self, addr: usize) -> Option<Range<usize>> {
        core::iter::once(&self.kernel)
            .chain(self.ist.iter())
            .find(|stack| stack.contains(&addr))
            .cloned()
    }

    /// Whether `addr` is in a guard page.
    pub fn in_guard(&self, addr: usize) -> bool {
        self.guards.iter().any(|guard| guard.contains(&addr))
    }
}

/// The 64-bit task-state segment.
#[repr(C, packed)]
struct Tss {
    _reserved0: u32,
    rsp: [u64; 3],
    _reserved1: u64,
    ist: [u64; 7],
    _reserved2: u64,
    _reserved3: u16,
    iomap_base: u16,
}

/// A CPU's GDT (null, kernel code and data, an unused slot, user data and
/// code, then the two-slot TSS descriptor) and TSS.
#[repr(C, align(16))]
struct Descriptors {
    gdt: [u64; 8],
    tss: Tss,
}

/// Everything the kernel keeps per CPU.
#[repr(C, align(64))]
pub struct PerCpu {
    /// This structure's own address: `gs:[0]`.
    self_addr: AtomicUsize,
    /// The CPU's index: `gs:[8]`.
    index: AtomicUsize,
    /// The preemption count: `gs:[16]`.
    preempt: AtomicU32,
    /// Interrupt handlers running, nested: `gs:[20]`.
    irq_depth: AtomicU32,
    /// The top of the running thread's kernel stack: `gs:[24]`.
    kernel_rsp: AtomicU64,
    /// Scratch for the system call entry (the program's stack pointer):
    /// `gs:[32]`.
    scratch: AtomicU64,
    /// The address of this CPU's TSS, once loaded.
    tss: AtomicUsize,
    /// Local APIC ID.
    pub apic_id: AtomicU32,
    /// Listed in the MADT as usable.
    pub present: AtomicBool,
    /// NUMA node, from the SRAT (0 without one).
    pub node: AtomicU8,
    /// Running kernel code.
    pub online: AtomicBool,
    /// Stopped by the panic path.
    pub stopped: AtomicBool,
    /// Timer interrupts taken.
    pub ticks: AtomicU64,
    /// Ping IPIs received.
    pub pings: AtomicU64,
    /// The last TLB shootdown request this CPU has carried out.
    pub tlb_generation: AtomicU64,
    /// The address space this CPU runs (its address), or 0 for the
    /// kernel's own tables.
    pub space: AtomicUsize,
    /// Exception handlers running on this CPU, to catch recursion.
    pub exception_depth: AtomicU32,
    /// The fault this CPU is panicking for.
    pub fault: SpinLock<Option<Fault>>,
    /// The CPU's stacks.
    pub stacks: Once<Stacks>,
    /// The stack of the thread running here (start and end), set by the
    /// context switch; both 0 before threads.
    pub thread_stack: [AtomicUsize; 2],
    /// The address of the running thread's context block, or 0.
    pub context: AtomicUsize,
    /// Reschedule IPIs received.
    pub reschedules: AtomicU64,
    /// When the next periodic tick is due, in nanoseconds.
    pub next_tick_ns: AtomicU64,
    /// The deadline the kernel asked for, or `u64::MAX`.
    pub deadline_ns: AtomicU64,
    /// When the timer is armed to fire, or `u64::MAX` when it is not (or
    /// has fired and its interrupt is not handled yet).
    pub armed_ns: AtomicU64,
    /// Times the timer was written to arm it.
    pub timer_writes: AtomicU64,
    /// Interprocessor interrupts this CPU sent.
    pub ipis_sent: AtomicU64,
    /// KVM's paravirtual end-of-interrupt word: the hypervisor sets bit 0
    /// when the interrupt it delivers needs no EOI write (`apic`).
    pub pv_eoi: AtomicU32,
    descriptors: TakeOnce<Descriptors>,
}

impl PerCpu {
    const fn new() -> Self {
        Self {
            self_addr: AtomicUsize::new(0),
            index: AtomicUsize::new(0),
            preempt: AtomicU32::new(0),
            irq_depth: AtomicU32::new(0),
            kernel_rsp: AtomicU64::new(0),
            scratch: AtomicU64::new(0),
            tss: AtomicUsize::new(0),
            apic_id: AtomicU32::new(0),
            present: AtomicBool::new(false),
            node: AtomicU8::new(0),
            online: AtomicBool::new(false),
            stopped: AtomicBool::new(false),
            ticks: AtomicU64::new(0),
            pings: AtomicU64::new(0),
            tlb_generation: AtomicU64::new(0),
            space: AtomicUsize::new(0),
            exception_depth: AtomicU32::new(0),
            fault: SpinLock::new(None),
            stacks: Once::new(),
            thread_stack: [const { AtomicUsize::new(0) }; 2],
            context: AtomicUsize::new(0),
            reschedules: AtomicU64::new(0),
            next_tick_ns: AtomicU64::new(0),
            deadline_ns: AtomicU64::new(u64::MAX),
            armed_ns: AtomicU64::new(u64::MAX),
            timer_writes: AtomicU64::new(0),
            ipis_sent: AtomicU64::new(0),
            pv_eoi: AtomicU32::new(0),
            descriptors: TakeOnce::new(Descriptors {
                gdt: [0; 8],
                tss: Tss {
                    _reserved0: 0,
                    rsp: [0; 3],
                    _reserved1: 0,
                    ist: [0; 7],
                    _reserved2: 0,
                    _reserved3: 0,
                    iomap_base: 0,
                },
            }),
        }
    }

    /// The CPU's index.
    pub fn index(&self) -> usize {
        self.index.load(Ordering::Relaxed)
    }

    /// Sets the stack that system calls and interrupts from user mode
    /// switch to: the top of the kernel stack of the thread about to run.
    /// With interrupts masked, on this CPU.
    pub fn set_kernel_stack(&self, top: usize) {
        self.kernel_rsp.store(top as u64, Ordering::Relaxed);
        let tss = self.tss.load(Ordering::Relaxed);
        if tss != 0 {
            // SAFETY: `tss` is this CPU's TSS, which lives in a static and
            // is only written here, by its own CPU with interrupts masked;
            // the CPU reads RSP0 only when an interrupt or exception
            // arrives from user mode, which cannot happen meanwhile. The
            // TSS is packed, so the write is unaligned.
            unsafe {
                let rsp0 = core::ptr::addr_of_mut!((*(tss as *mut Tss)).rsp[0]);
                rsp0.write_unaligned(top as u64);
            }
        }
    }

    /// The running thread's stack, if it is not one of [`Stacks`].
    pub fn thread_stack(&self) -> Option<Range<usize>> {
        let [start, end] = &self.thread_stack;
        let (start, end) = (start.load(Ordering::Relaxed), end.load(Ordering::Relaxed));
        let stacks = self.stacks.get();
        (start < end && !stacks.is_some_and(|s| s.containing(start).is_some()))
            .then_some(start..end)
    }

    /// The stack containing `addr`: one of this CPU's own, or the running
    /// thread's.
    pub fn stack_containing(&self, addr: usize) -> Option<Range<usize>> {
        self.stacks
            .get()
            .and_then(|stacks| stacks.containing(addr))
            .or_else(|| self.thread_stack().filter(|stack| stack.contains(&addr)))
    }

    /// Whether `addr` is in the guard page of one of this CPU's stacks or
    /// of the running thread's.
    pub fn in_stack_guard(&self, addr: usize) -> bool {
        self.stacks.get().is_some_and(|stacks| stacks.in_guard(addr))
            || self.thread_stack().is_some_and(|stack| {
                (stack.start.saturating_sub(4096)..stack.start).contains(&addr)
            })
    }
}

/// Adds one to the running CPU's preemption count.
#[inline(always)]
pub fn preempt_disable() {
    // SAFETY: GS base points at this CPU's `PerCpu` (see `current_index`),
    // and offset 16 is its `preempt` field, an `AtomicU32` only ever
    // changed by its own CPU. One instruction, so it cannot be split by an
    // interrupt or a migration; the asm is a compiler barrier.
    unsafe { asm!("inc dword ptr gs:[16]", options(nostack)) };
}

/// Takes one from the running CPU's preemption count and returns the rest.
#[inline(always)]
pub fn preempt_enable() -> u32 {
    let left: u32;
    // SAFETY: as in `preempt_disable`. Reading the result back separately
    // is fine: a thread is only preempted while the count is 0, and then
    // whichever CPU it runs on next has a count of 0 too.
    unsafe { asm!("dec dword ptr gs:[16]", "mov {:e}, gs:[16]", out(reg) left, options(nostack)) };
    left
}

/// The running CPU's preemption count.
#[inline(always)]
pub fn preempt_count() -> u32 {
    let count: u32;
    // SAFETY: as in `preempt_disable`, reading only.
    unsafe {
        asm!("mov {:e}, gs:[16]", out(reg) count, options(nostack, readonly, preserves_flags))
    };
    count
}

/// Sets the running CPU's preemption count.
pub fn set_preempt_count(count: u32) {
    // SAFETY: as in `preempt_disable`.
    unsafe { asm!("mov gs:[16], {:e}", in(reg) count, options(nostack, preserves_flags)) };
}

/// Notes that an interrupt handler starts on this CPU.
#[inline(always)]
pub fn interrupt_enter() {
    // SAFETY: as in `preempt_disable`, for the `irq_depth` field at 20.
    unsafe { asm!("inc dword ptr gs:[20]", options(nostack)) };
}

/// Notes that an interrupt handler is done, and returns the depth left.
#[inline(always)]
pub fn interrupt_exit() -> u32 {
    let left: u32;
    // SAFETY: as in `interrupt_enter`.
    unsafe { asm!("dec dword ptr gs:[20]", "mov {:e}, gs:[20]", out(reg) left, options(nostack)) };
    left
}

/// The running CPU's interrupt depth.
#[inline(always)]
pub fn interrupt_depth() -> u32 {
    let depth: u32;
    // SAFETY: as in `interrupt_enter`, reading only.
    unsafe {
        asm!("mov {:e}, gs:[20]", out(reg) depth, options(nostack, readonly, preserves_flags))
    };
    depth
}

static CPUS: [PerCpu; MAX_CPUS] = [const { PerCpu::new() }; MAX_CPUS];

const _: () = {
    assert!(core::mem::offset_of!(PerCpu, index) == 8);
    assert!(core::mem::offset_of!(PerCpu, preempt) == 16);
    assert!(core::mem::offset_of!(PerCpu, irq_depth) == 20);
    assert!(core::mem::offset_of!(PerCpu, kernel_rsp) == KERNEL_RSP_OFFSET);
    assert!(core::mem::offset_of!(PerCpu, scratch) == SCRATCH_OFFSET);
};

/// The data of CPU `index`.
pub fn get(index: usize) -> Option<&'static PerCpu> {
    CPUS.get(index)
}

/// Index of the running CPU.
pub fn current_index() -> usize {
    let index: usize;
    // SAFETY: GS base points at this CPU's `PerCpu` from the first
    // instruction of `entry` (boot CPU) or `ap_entry` (others), and offset
    // 8 is its `index` field, an `AtomicUsize` read with a plain load.
    unsafe { asm!("mov {}, gs:[8]", out(reg) index, options(nostack, preserves_flags, readonly)) };
    index
}

/// The running CPU's data.
pub fn current() -> &'static PerCpu {
    &CPUS[current_index().min(MAX_CPUS - 1)]
}

/// Points GS base at CPU `index`'s data. Called first thing on each CPU.
pub fn set_current(index: usize) {
    let cpu = &CPUS[index];
    cpu.self_addr.store(cpu as *const PerCpu as usize, Ordering::Relaxed);
    cpu.index.store(index, Ordering::Relaxed);
    // SAFETY: GS_BASE exists on every x86-64 CPU, and the kernel uses GS
    // only to reach `PerCpu`, which this now points at for this CPU.
    unsafe { cpu::wrmsr(msr::GS_BASE, cpu as *const PerCpu as u64) };
}

fn tss_descriptor(base: u64, limit: u32) -> [u64; 2] {
    let low = u64::from(limit & 0xffff)
        | ((base & 0xff_ffff) << 16)
        | (0x89 << 40) // present, type 9: available 64-bit TSS
        | (u64::from((limit >> 16) & 0xf) << 48)
        | (((base >> 24) & 0xff) << 56);
    [low, base >> 32]
}

/// Loads this CPU's GDT and TSS, with interrupt stacks whose tops are
/// `ist_tops`. Call once per CPU, after [`set_current`].
pub fn load_descriptors(ist_tops: [usize; IST_STACKS]) {
    let cpu = current();
    let Some(descriptors) = cpu.descriptors.take() else {
        panic!("CPU {} loaded its descriptor tables twice", cpu.index());
    };
    let mut ist = [0u64; 7];
    for (slot, top) in ist.iter_mut().zip(ist_tops) {
        *slot = top as u64;
    }
    descriptors.tss = Tss {
        _reserved0: 0,
        rsp: [0; 3],
        _reserved1: 0,
        ist,
        _reserved2: 0,
        _reserved3: 0,
        iomap_base: core::mem::size_of::<Tss>() as u16,
    };
    let tss_base = &descriptors.tss as *const Tss as u64;
    let [tss_low, tss_high] = tss_descriptor(tss_base, core::mem::size_of::<Tss>() as u32 - 1);
    // The user descriptors have DPL 3 and their accessed bits set, so the
    // CPU never writes the table to set them.
    descriptors.gdt = [
        0,
        0x00AF_9A00_0000_FFFF, // 0x08: kernel code, 64-bit
        0x00CF_9200_0000_FFFF, // 0x10: kernel data
        0,                     // 0x18: SYSRET's 32-bit user code, unused
        0x00CF_F300_0000_FFFF, // 0x20: user data
        0x00AF_FB00_0000_FFFF, // 0x28: user code, 64-bit
        tss_low,               // 0x30: the TSS
        tss_high,
    ];
    cpu.tss.store(tss_base as usize, Ordering::Relaxed);

    #[repr(C, packed)]
    struct Pointer {
        limit: u16,
        base: u64,
    }
    let pointer = Pointer {
        limit: (core::mem::size_of::<[u64; 8]>() - 1) as u16,
        base: descriptors.gdt.as_ptr() as u64,
    };
    // SAFETY: the GDT lives in a static that is never written again (the
    // `TakeOnce` was just claimed), its code and data descriptors match the
    // flat 64-bit model the kernel runs in, and the TSS descriptor points
    // at the TSS next to it. Reloading CS through a far return and the data
    // segments keeps every address the same. GS base is restored by the
    // caller's earlier `set_current` value below, since loading GS clears it.
    unsafe {
        asm!(
            "lgdt [{pointer}]",
            "push {cs}",
            "lea {tmp}, [rip + 2f]",
            "push {tmp}",
            "retfq",
            "2:",
            "mov ds, {ds:x}",
            "mov es, {ds:x}",
            "mov ss, {ds:x}",
            "xor {tmp:e}, {tmp:e}",
            "mov fs, {tmp:x}",
            "mov gs, {tmp:x}",
            "ltr {tss:x}",
            pointer = in(reg) &pointer,
            cs = in(reg) u64::from(KERNEL_CS),
            ds = in(reg) u64::from(KERNEL_DS),
            tss = in(reg) u64::from(TSS_SELECTOR),
            tmp = out(reg) _,
        )
    };
    // Loading GS zeroed its base; point it back at this CPU.
    set_current(cpu.index());
    crate::user::init_cpu();
}
