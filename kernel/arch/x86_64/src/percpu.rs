//! Per-CPU data, the global descriptor table and the task-state segment.
//!
//! Every CPU has a [`PerCpu`] in a static array; its GS base register points
//! at its own entry, so `gs:[8]` is the running CPU's index. The boot CPU
//! sets GS base first thing in [`crate::entry`], before any code that asks
//! which CPU it is running on.
//!
//! Each CPU also has its own GDT and TSS. The TSS holds the interrupt stack
//! table: double faults, NMIs and machine checks switch to dedicated stacks,
//! so a kernel stack overflow still produces a report.

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
/// Task-state segment selector.
const TSS_SELECTOR: u16 = 0x18;

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

/// A CPU's GDT (null, code, data, then the two-slot TSS descriptor) and TSS.
#[repr(C, align(16))]
struct Descriptors {
    gdt: [u64; 5],
    tss: Tss,
}

/// Everything the kernel keeps per CPU.
#[repr(C, align(64))]
pub struct PerCpu {
    /// This structure's own address: `gs:[0]`.
    self_addr: AtomicUsize,
    /// The CPU's index: `gs:[8]`.
    index: AtomicUsize,
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
    descriptors: TakeOnce<Descriptors>,
}

impl PerCpu {
    const fn new() -> Self {
        Self {
            self_addr: AtomicUsize::new(0),
            index: AtomicUsize::new(0),
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
            descriptors: TakeOnce::new(Descriptors {
                gdt: [0; 5],
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
}

static CPUS: [PerCpu; MAX_CPUS] = [const { PerCpu::new() }; MAX_CPUS];

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
    descriptors.gdt = [0, 0x00AF_9A00_0000_FFFF, 0x00CF_9200_0000_FFFF, tss_low, tss_high];

    #[repr(C, packed)]
    struct Pointer {
        limit: u16,
        base: u64,
    }
    let pointer = Pointer {
        limit: (core::mem::size_of::<[u64; 5]>() - 1) as u16,
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
}
