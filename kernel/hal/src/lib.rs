// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! The hardware abstraction layer (HAL) of the Radix kernel.
//!
//! The arch-independent kernel (`kernel/radix`) reaches the hardware only
//! through the traits in this crate. Each architecture crate implements all
//! of them on one zero-sized type, which the kernel names `Arch`:
//! `radix-arch-x86_64` for real, and `radix-arch-aarch64` with stubs that
//! only have to compile. CI checks the kernel for AArch64 too, so code that
//! assumes x86-64 cannot creep into the common part (roadmap Phase 1).
//!
//! The traits follow the plan's trait set: [`PageTable`],
//! [`InterruptController`], [`Timer`] and [`CpuLocal`], plus [`Cpu`] for the
//! processor itself, [`Smp`] for starting and stopping the other CPUs,
//! [`Threads`] for kernel threads' contexts and switching between them
//! (Phase 4), [`UserMode`] for programs' address spaces, system calls and
//! faults (Phase 5), [`Devices`] for PCI configuration space, device
//! registers, DMA memory and message-signalled interrupts (Phase 7, with
//! the safe types of `radix-hw`), and [`Platform`] for machine-wide
//! services such as restarting. All methods are associated functions:
//! there is exactly one machine.
//!
//! The architecture owns the entry point. It sets up the CPU, the consoles it
//! knows about and the kernel's page tables, then calls the kernel's
//! `radix_kernel_main` with a [`BootParams`].

#![cfg_attr(not(test), no_std)]
#![forbid(unsafe_code)]

pub mod boot;
pub mod clock;
pub mod cmdline;
pub mod user;

use core::ops::Range;
use core::sync::atomic::AtomicU32;

pub use boot::{
    BootModule, BootParams, Channel, Firmware, Framebuffer, MemoryKind, MemoryRegion, PixelFormat,
    Reservation,
};
pub use radix_hw::pci::Address as PciAddress;
pub use radix_hw::{Dma, Mmio, MsiMessage, Page};
pub use user::{
    MapPageError, PageEdit, Scanned, SignalFrame, SyscallRegs, UserAccess, UserFault,
    UserFaultKind, UserMode,
};

/// Most CPUs the kernel manages. Per-CPU tables are sized by this.
pub const MAX_CPUS: usize = 256;

/// The processor running the current code.
pub trait Cpu {
    /// Architecture name, such as `x86_64`.
    const NAME: &'static str;

    /// Stops this CPU for good, with interrupts disabled.
    fn halt() -> !;

    /// Sleeps until the next interrupt arrives.
    fn wait_for_interrupt();

    /// Sleeps until an interrupt arrives or another CPU writes to `wake`
    /// (which must then be non-zero), in the CPU's light sleep state:
    /// MONITOR and MWAIT on x86-64 when it has them, so a waker need not
    /// send an interrupt, else HLT. Call with interrupts masked; it returns
    /// with them masked, after any interrupt that woke it was handled.
    /// Returns at once if `wake` is already non-zero.
    fn idle_until(wake: &AtomicU32);

    /// Whether [`Cpu::idle_until`] notices writes to `wake` without an
    /// interrupt (MWAIT), and its name for the boot report.
    fn idle_mode() -> (bool, &'static str);

    /// Whether the kernel runs in a virtual machine, where halting and
    /// interprocessor interrupts are exits to the hypervisor.
    fn virtualized() -> bool;

    /// Whether this CPU accepts interrupts.
    fn interrupts_enabled() -> bool;

    /// Lets this CPU accept interrupts.
    fn enable_interrupts();

    /// Masks interrupts on this CPU and returns whether they were enabled.
    fn disable_interrupts() -> bool;

    /// Undoes [`Cpu::disable_interrupts`].
    fn restore_interrupts(were_enabled: bool) {
        if were_enabled {
            Self::enable_interrupts();
        }
    }

    /// The caller's frame pointer, where a backtrace starts.
    fn frame_pointer() -> usize;

    /// Walks the chain of stack frames that starts at `frame_pointer` and
    /// stores each frame's return address in `out`, innermost first.
    /// Returns how many it stored. Only memory inside the current CPU's
    /// kernel stacks is read, so a corrupt chain ends the walk instead of
    /// faulting.
    fn backtrace(frame_pointer: usize, out: &mut [usize]) -> usize;

    /// Stops in the kernel debugger's breakpoint handler, as a breakpoint
    /// instruction placed in the code would (`int3` on x86-64).
    fn breakpoint();

    /// Writes the cache lines holding `bytes` back to memory, so that they
    /// survive a reset that discards the caches.
    fn flush_to_memory(bytes: &[u8]);

    /// How many banks each CPU reports hardware errors in, through its
    /// machine-check architecture (Phase 11, ADR 0145): 0 when it has none
    /// or reporting was turned off.
    fn machine_check_banks() -> usize;

    /// Whether those banks are of AMD's scalable machine-check
    /// architecture (Zen, ADR 0204), whose errors name the block of the
    /// chip they come from.
    fn scalable_machine_checks() -> bool;

    /// Reads the errors this CPU's machine-check banks hold and clears
    /// them, calling `found` with each ([`MachineCheck`]). Runs in thread
    /// context on the CPU whose banks it reads.
    fn poll_machine_checks(found: &mut MachineCheckFound<'_>);

    /// What interrupts a CPU when its banks corrected errors (ADR 0169),
    /// for the log: Intel's CMCI or AMD's thresholds; `None` if the banks
    /// are only polled.
    fn corrected_error_interrupt() -> Option<&'static str>;

    /// Calls `handler`, in an interrupt handler with interrupts masked,
    /// when a CPU's banks say they corrected errors, for the kernel to
    /// read them soon.
    fn set_corrected_error_handler(handler: fn());

    /// Masks this CPU's interrupt for corrected errors, or unmasks it: off
    /// during a storm of them.
    fn mask_corrected_errors(masked: bool);

    /// The running CPU's control of its own speed (ADR 0202), Intel's HWP
    /// or AMD's CPPC, if CPUID says it has one.
    fn performance_driver() -> Option<radix_pstate::Driver>;

    /// Turns the running CPU's control on, if it has one, and returns its
    /// levels and the request the firmware left.
    fn start_performance() -> Option<(radix_pstate::Levels, radix_pstate::Request)>;

    /// Asks the running CPU for `request`, if it has a control.
    fn request_performance(request: radix_pstate::Request);

    /// The running CPU's APERF and MPERF counts, if it has the counters,
    /// which count at its own frequency and at the cycle counter's while
    /// it runs.
    fn performance_counts() -> Option<(u64, u64)>;

    /// The frequencies of the running CPU's `levels`, and the level its
    /// highest frequency is that of where the CPUs rank their cores
    /// (AMD's), whose highest levels are then ranks.
    fn performance_scale(levels: radix_pstate::Levels) -> Option<(radix_pstate::Scale, u8)>;

    /// Calls `found` with each of the processor's temperature sensors that
    /// its chipset reaches (ADR 0205): AMD's, which Linux's `k10temp`
    /// shows. They are looked for at the first call.
    fn chipset_temperatures(found: &mut dyn FnMut(radix_sensors::k10temp::Sensor));

    /// Reads one of them, in thousandths of a degree Celsius.
    fn read_chipset_temperature(sensor: radix_sensors::k10temp::Sensor) -> Option<i32>;

    /// What the running CPU's own temperature sensors say (ADR 0205):
    /// Intel's digital thermal sensors, which Linux's `coretemp` shows.
    fn core_temperature() -> Option<radix_sensors::coretemp::Sample>;
}

/// An error a machine-check bank held, as [`Cpu::poll_machine_checks`]
/// found it.
pub struct MachineCheck<'a> {
    /// Its bank.
    pub bank: usize,
    /// Whether the hardware corrected it.
    pub corrected: bool,
    /// How many errors it stands for: the bank's count of the corrected
    /// errors since it was read, where it keeps one (CMCI, AMD's
    /// thresholds), else 1.
    pub count: u64,
    /// For a memory controller's error, `Some` with the channel it was
    /// on, if the bank says.
    pub memory: Option<Option<u8>>,
    /// The physical address of the page whose memory it says went bad, to
    /// be retired.
    pub page: Option<u64>,
    /// What it was.
    pub what: &'a dyn core::fmt::Display,
}

/// What [`Cpu::poll_machine_checks`] calls with each error a bank held.
pub type MachineCheckFound<'a> = dyn FnMut(&MachineCheck<'_>) + 'a;

/// Data and identity of the current CPU.
pub trait CpuLocal {
    /// Index of the CPU running this code; the boot CPU is 0, and the
    /// others are numbered in the order the firmware lists them.
    fn current_cpu() -> usize;

    /// Adds one to this CPU's preemption count. While it is not zero the
    /// running thread is not preempted (spin-lock holders, RCU readers).
    /// A single instruction, so it cannot be split by a migration.
    fn preempt_disable();

    /// Takes one from this CPU's preemption count and returns what is left.
    fn preempt_enable() -> u32;

    /// This CPU's preemption count.
    fn preempt_count() -> u32;

    /// Sets this CPU's preemption count: the scheduler saves and restores
    /// it with each thread.
    fn set_preempt_count(count: u32);

    /// How many interrupt handlers this CPU is running, nested (0 in thread
    /// context).
    fn interrupt_depth() -> u32;

    /// Whether the interrupt this CPU is handling came while it ran user
    /// code: what the timer interrupt samples to split a thread's CPU time
    /// between user mode and the kernel. False outside an interrupt.
    fn interrupted_user() -> bool;

    /// The word the scheduler keeps for the thread running on this CPU (0
    /// before it has one), read in one instruction that a migration cannot
    /// split: whatever CPU the code is on, the thread running there is the
    /// code's own.
    fn running_word() -> usize;

    /// Sets this CPU's running-thread word: the scheduler does at each
    /// switch.
    fn set_running_word(word: usize);
}

/// One CPU known to the kernel.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CpuInfo {
    /// Index used throughout the kernel.
    pub index: usize,
    /// The interrupt controller's ID for the CPU (the APIC ID on x86-64).
    pub hardware_id: u32,
    /// Whether this is the CPU the firmware started.
    pub boot_cpu: bool,
    /// Whether the CPU is running kernel code.
    pub online: bool,
}

/// Where one CPU sits in the machine.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub struct CpuPlace {
    /// Its package (socket).
    pub package: u32,
    /// Its core, unique within the package.
    pub core: u32,
    /// Its hardware thread, unique within the core.
    pub thread: u32,
    /// Its NUMA node.
    pub node: u8,
}

/// Starting and stopping the other CPUs.
pub trait Smp {
    /// Number of CPUs the firmware reports as present, online or not.
    fn cpu_count() -> usize;

    /// Information about CPU `index`.
    fn cpu_info(index: usize) -> Option<CpuInfo>;

    /// Starts every present CPU except the boot CPU. Each runs `entry` with
    /// its index once it is ready, with interrupts disabled. Returns how
    /// many CPUs came online, including the boot CPU.
    fn start_secondary_cpus(entry: fn(usize) -> !) -> usize;

    /// Asks every other online CPU to stop and waits briefly for them.
    /// Returns how many confirmed. Used on the panic path.
    fn stop_other_cpus() -> usize;

    /// Where CPU `index` was when [`Smp::stop_other_cpus`] stopped it: the
    /// interrupted instruction, then the return addresses of its callers,
    /// as many as fit in `out`. Returns how many it stored: 0 for a CPU
    /// that did not stop.
    fn stopped_trace(index: usize, out: &mut [usize]) -> usize;

    /// The package, core, hardware thread and NUMA node of CPU `index`.
    fn cpu_place(index: usize) -> Option<CpuPlace>;
}

/// Kinds of inter-processor interrupt the common kernel sends.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Ipi {
    /// Does nothing but count; used to test that IPIs arrive.
    Ping,
    /// Makes the target check whether it should switch threads, on its way
    /// back from the interrupt (see [`Threads::set_preempt_handler`]).
    Reschedule,
}

/// The interrupt controllers.
pub trait InterruptController {
    /// Short description, such as `x2APIC`.
    fn name() -> &'static str;

    /// Sends `ipi` to CPU `cpu`.
    fn send_ipi(cpu: usize, ipi: Ipi);

    /// How many [`Ipi::Ping`] interrupts CPU `cpu` has handled.
    fn pings_received(cpu: usize) -> u64;

    /// How many interprocessor interrupts CPU `cpu` has sent, of any kind.
    fn ipis_sent(cpu: usize) -> u64;
}

/// Clocks and the periodic timer interrupt.
pub trait Timer {
    /// Nanoseconds since an arbitrary point early in boot. Monotonic and
    /// consistent across CPUs; 0 until the clock has been calibrated.
    fn now_ns() -> u64;

    /// Rate of the periodic timer interrupt on every online CPU, or 0 when
    /// it is not running.
    fn tick_hz() -> u32;

    /// Timer interrupts CPU `cpu` has taken.
    fn ticks(cpu: usize) -> u64;

    /// Waits at least `us` microseconds without sleeping.
    fn delay_us(us: u64);

    /// Registers the function the timer interrupt calls, with interrupts
    /// masked: on every periodic tick (with `true`), and when the deadline
    /// set by [`Timer::set_deadline`] has passed.
    fn set_timer_handler(handler: fn(tick: bool));

    /// Asks for a timer interrupt on this CPU at `deadline_ns` (on the
    /// [`Timer::now_ns`] clock), besides the periodic tick. A later call
    /// replaces it; `u64::MAX` cancels it. Call with interrupts masked.
    fn set_deadline(deadline_ns: u64);

    /// How many times CPU `cpu` wrote its timer hardware to arm it. Under
    /// a hypervisor each write is usually a VM exit, so the benchmarks
    /// report it.
    fn timer_writes(cpu: usize) -> u64;

    /// How the timer is programmed, for the boot report.
    fn timer_mode() -> &'static str;

    /// The time of day: nanoseconds since 1970-01-01 00:00:00 UTC, read
    /// from the machine's real-time clock at boot (which is taken to keep
    /// UTC) and advanced with [`Timer::now_ns`]; `None` when the machine
    /// has no clock the kernel can read.
    fn wall_clock_ns() -> Option<u64>;
}

/// One PCI segment's configuration space, as the firmware describes it
/// (the ACPI MCFG table on x86-64).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PciSegment {
    /// Physical address of the segment's ECAM window, where bus 0 would
    /// start.
    pub base: u64,
    /// The segment group number.
    pub segment: u16,
    /// The first bus the window decodes.
    pub first_bus: u8,
    /// The last bus the window decodes.
    pub last_bus: u8,
}

/// A hardware watchdog the kernel drives (Phase 11), as
/// [`Devices::find_watchdog`] finds it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Watchdog {
    /// What it is, for the log and `/kernel/class/watchdog`.
    pub name: &'static str,
    /// The shortest timeout it takes, in seconds.
    pub min_seconds: u32,
    /// The longest timeout it takes, in seconds.
    pub max_seconds: u32,
    /// Whether it restarted the machine: the boot before this one ended
    /// with it running out.
    pub restarted: bool,
}

/// The firmware's SMBIOS tables (Phase 11e, ADR 0231), as
/// [`Platform::smbios`] finds them, where the firmware keeps them.
#[derive(Clone, Copy, Debug)]
pub struct Smbios {
    /// The entry point, `_SM3_` or `_SM_`, as long as it says it is.
    pub entry: &'static [u8],
    /// The structures, up to and with the one that ends them.
    pub table: &'static [u8],
}

/// Devices: what drivers need from the machine (Phase 7).
///
/// Drivers never see raw pointers: device registers come as
/// [`radix_hw::Mmio`] windows and DMA memory as [`radix_hw::Dma`], whose
/// accesses are checked; only the architecture creates them. Interrupts
/// are message-signalled (MSI or MSI-X), so no firmware routing table is
/// needed (ADR 0052).
pub trait Devices {
    /// Calls `found` with each PCI segment the firmware describes.
    fn pci_segments(found: &mut dyn FnMut(PciSegment));

    /// Maps `len` bytes of device registers from physical address `phys`,
    /// uncached, for the rest of the kernel's life. `None` when the
    /// mapping window or memory for page tables runs out.
    fn map_device(phys: u64, len: usize) -> Option<radix_hw::Mmio>;

    /// Allocates `pages` pages of zeroed, physically contiguous memory for
    /// the PCI function `source` to read and write, below 4 GiB if
    /// `below_4g`. Behind an IOMMU, `source` reaches it, at its physical
    /// address, until it is given back, when the [`radix_hw::Dma`] is
    /// dropped (ADR 0164).
    fn alloc_dma(source: PciAddress, pages: usize, below_4g: bool) -> Option<radix_hw::Dma>;

    /// Lets the PCI function `source` reach the pages of `frames`
    /// (physical address, length) at their physical addresses, for one
    /// request, until [`Devices::unmap_dma`] takes them back: behind an
    /// IOMMU a device reaches only its DMA memory and what is mapped for
    /// it. A page mapped twice stays until it is unmapped twice. Returns
    /// false, mapping nothing, when memory for the IOMMU's tables runs out
    /// or a page is past what they reach. Without an IOMMU it does nothing.
    fn map_dma(source: PciAddress, frames: &[(u64, u32)]) -> bool;

    /// Takes back what [`Devices::map_dma`] gave `source` for `frames`,
    /// once the device is done with them: when it returns, the device can
    /// no longer reach them.
    fn unmap_dma(source: PciAddress, frames: &[(u64, u32)]);

    /// Allocates a zeroed page of ordinary RAM for the page cache, which
    /// disks read into and write from by DMA. It may sleep, and never takes
    /// the frame allocator's reserves; `None` when memory runs out even
    /// after reclaim. It is given back when the
    /// [`radix_hw::Page`] is dropped.
    fn alloc_cache_page() -> Option<radix_hw::Page>;

    /// Allocates an interrupt vector whose interrupts call `handler(arg)`,
    /// with interrupts masked, on the CPU they arrive at, for the PCI
    /// function `source` to raise. Returns the vector's number and the
    /// message `source` writes to raise it. Behind an IOMMU that remaps
    /// interrupts only `source` can raise it (ADR 0166). `None` when no
    /// vector is free, or the IOMMU could not give it an entry.
    fn allocate_interrupt(
        source: PciAddress,
        handler: fn(usize),
        arg: usize,
    ) -> Option<(u32, radix_hw::MsiMessage)>;

    /// As [`Devices::allocate_interrupt`], with the interrupts sent to CPU
    /// `cpu` rather than the boot CPU: for a device with a queue for each
    /// CPU. A CPU the message cannot name gets the boot CPU's.
    fn allocate_interrupt_at(
        source: PciAddress,
        cpu: usize,
        handler: fn(usize),
        arg: usize,
    ) -> Option<(u32, radix_hw::MsiMessage)>;

    /// Frees a vector from [`Devices::allocate_interrupt`]; the device
    /// must no longer send its message. Behind an IOMMU that remaps
    /// interrupts, it can no longer raise it when this returns.
    fn free_interrupt(vector: u32);

    /// How many interrupts vector `vector` has delivered.
    fn interrupt_count(vector: u32) -> u64;

    /// Starts the machine's built-in keyboard, if it has one: on a PC the
    /// PS/2 keyboard behind the i8042 controller (Phase 9). `byte` is
    /// called, in an interrupt handler with interrupts masked, with each
    /// byte the keyboard sends. Returns what the keyboard is, for the log,
    /// or `None` if the machine has none.
    fn start_keyboard(byte: fn(u8)) -> Option<&'static str>;

    /// Sends `byte` to the built-in keyboard: a command, or a command's
    /// argument, which the keyboard answers through the `byte` function of
    /// [`Devices::start_keyboard`]. Returns false if the controller did
    /// not take it in time.
    fn keyboard_send(byte: u8) -> bool;

    /// Hears the machine's power button, if the kernel can: on a PC the
    /// ACPI button that is a fixed feature, through the System Control
    /// Interrupt (Phase 11). `pressed` is called, in an interrupt handler
    /// with interrupts masked, each time it is pressed. Returns what the
    /// button is, for the log, or `None`, leaving the button to the
    /// firmware, if the kernel cannot hear it.
    fn start_power_button(pressed: fn()) -> Option<&'static str>;

    /// Finds the machine's hardware watchdog, if it has one the kernel
    /// drives: on a PC, QEMU's i6300esb, or the TCO timer of an Intel
    /// ICH9 or ICH10 chipset (Phase 11). It is left stopped. Called once,
    /// after the PCI scan; `None` if there is none.
    fn find_watchdog() -> Option<Watchdog>;

    /// Starts the watchdog [`Devices::find_watchdog`] found, or changes its
    /// timeout, so that it restarts the machine unless petted within
    /// `seconds`, clamped to the timeouts it takes. Its count starts again
    /// from now. Returns the timeout set. The caller serializes the
    /// watchdog's methods.
    fn watchdog_start(seconds: u32) -> u32;

    /// Pets the running watchdog: its count starts again.
    fn watchdog_pet();

    /// Stops the watchdog.
    fn watchdog_stop();

    /// Finds the chipset's SMBus host controller, Intel's or that of AMD's
    /// FCH (Phase 11e, ADR 0206), and logs it and the memory modules'
    /// EEPROMs that answer on it. Called once, after the PCI scan; whether
    /// there is one.
    fn find_smbus() -> bool;

    /// Writes the name of the controller [`Devices::find_smbus`] found, as
    /// Linux names its adapter; fails if there is none.
    fn smbus_name(out: &mut dyn core::fmt::Write) -> core::fmt::Result;

    /// Reads I/O port `port` of the Chromium EC's host interface (Phase
    /// 11e, ADR 0207): its data and command ports (0x200 and 0x204), its
    /// packet (0x800 to 0x8ff) and its memory map (255 bytes from 0x900 or
    /// 0xe00). All ones for any other port, and where there are no I/O
    /// ports.
    fn ec_read(port: u16) -> u8;

    /// Writes the EC's command port or packet; does nothing for any other
    /// port.
    fn ec_write(port: u16, value: u8);
}

/// What the interpreter of the firmware's AML needs from the machine
/// (Phase 11c, ADR 0200): on a PC, where the ACPI tables start, physical
/// memory, I/O ports and the System Control Interrupt.
///
/// AML reaches whatever the firmware's code names, so these do what they
/// are asked without knowing why. Only the interpreter calls them, for
/// the firmware's AML, which the kernel trusts as it trusts the firmware.
pub trait Aml {
    /// The physical address of the ACPI RSDP, or of a copy of it that
    /// stays in RAM; `None` on a machine without ACPI.
    fn acpi_rsdp() -> Option<u64>;

    /// Maps `len` bytes of physical memory from `phys`, readable and
    /// writable, until [`Aml::unmap_firmware`]: RAM as RAM, anything
    /// else uncached, as it may be device registers. Returns the address
    /// of `phys`; `None` when the mapping window or memory for page tables
    /// runs out, or for more than 256 MiB of what is not RAM.
    fn map_firmware(phys: u64, len: usize) -> Option<usize>;

    /// Undoes the [`Aml::map_firmware`] that returned `virt` for `len`
    /// bytes. Panics if nothing is mapped there.
    fn unmap_firmware(virt: usize, len: usize);

    /// Reads `width` bytes (1, 2 or 4) from I/O port `port`.
    fn read_port(port: u16, width: u8) -> u32;

    /// Writes `width` bytes (1, 2 or 4) of `value` to I/O port `port`.
    fn write_port(port: u16, width: u8, value: u32);

    /// Routes the firmware's interrupt `irq` (on a PC, the FADT's SCI, an
    /// ISA interrupt) so that each one calls `handler`, in an interrupt
    /// handler with interrupts masked, on the boot CPU: level-triggered and
    /// active low unless the firmware's tables say otherwise. Once only;
    /// returns false if it cannot, or did before.
    fn start_firmware_interrupt(irq: u32, handler: fn()) -> bool;

    /// Stops the interrupt [`Aml::start_firmware_interrupt`] routed.
    fn stop_firmware_interrupt();
}

/// Kernel threads: their saved contexts and switching between them.
///
/// A context is what a thread leaves behind while another runs on its CPU:
/// its stack, its saved registers and its extended state (the FPU and
/// vector registers, saved eagerly on every switch). The scheduler decides
/// what runs; this trait only switches, so it knows nothing of run queues.
pub trait Threads {
    /// A saved execution context. Dropping one frees its stack, unless it
    /// is running on a CPU, in which case the memory is leaked and a
    /// warning logged.
    type Context: Send + Sync;

    /// Makes a context of the code this CPU runs now, on the stack it runs
    /// on (the boot thread, or a CPU's start-up code that becomes its idle
    /// thread). Its stack is not freed when the context is dropped.
    fn adopt_current() -> Option<Self::Context>;

    /// A new context that, when first switched to, runs `entry(arg)` on a
    /// fresh stack of `stack_pages` pages with an unmapped guard page
    /// below. `entry` starts with interrupts masked and a preemption count
    /// of 1, as if it had just returned from [`Threads::switch`]. `None`
    /// when memory runs out.
    fn new_context(entry: fn(usize) -> !, arg: usize, stack_pages: usize) -> Option<Self::Context>;

    /// Saves the running context `from` and continues `to`, waiting first
    /// if another CPU is still switching away from `to`. Returns when some
    /// CPU switches back to `from`. Call with interrupts masked; panics if
    /// `from` is not the context running on this CPU.
    fn switch(from: &Self::Context, to: &Self::Context);

    /// Whether `context` is on a CPU: from the switch to it until the
    /// switch away from it has saved everything.
    fn is_running(context: &Self::Context) -> bool;

    /// The stack of `context`.
    fn stack(context: &Self::Context) -> Range<usize>;

    /// Registers the function called on the way out of an interrupt that
    /// came in from thread context with interrupts on and a preemption
    /// count of 0, with interrupts masked: the scheduler switches threads
    /// there if it must.
    fn set_preempt_handler(handler: fn());

    /// Describes how extended state is saved, for the boot report.
    fn extended_state() -> &'static str;

    /// Fills the vector registers with a pattern made from `seed`, for the
    /// self-test that checks every switch keeps them.
    fn self_test_fill_vectors(seed: u64);

    /// Whether the vector registers still hold the pattern made from
    /// `seed`.
    fn self_test_vectors_hold(seed: u64) -> bool;
}

/// The kernel's page tables.
pub trait PageTable {
    /// The physical address that kernel virtual address `virt` maps to, or
    /// `None` if it is not mapped.
    fn translate(virt: usize) -> Option<u64>;

    /// Copies kernel memory from `virt` into `buf`, for the debugger.
    /// Returns `false`, copying nothing, unless every byte is mapped and
    /// backed by RAM; device memory is never read, because reads can have
    /// side effects.
    fn read(virt: usize, buf: &mut [u8]) -> bool;

    /// How far KASLR moved the kernel image from the address it was linked
    /// at (0 when it did not). Symbol tables hold link addresses.
    fn kernel_slide() -> u64;

    /// Describes the address-space layout and the memory protections in
    /// use, one line per call of `line`, for the boot report.
    fn describe(line: &mut dyn FnMut(core::fmt::Arguments<'_>));

    /// Maps `pages` pages of fresh zeroed memory, writable, at a free place
    /// in the kernel's address space with an unmapped guard page below,
    /// and returns the address of the first. Returns `None` when memory or
    /// address space runs out.
    fn map_area(pages: usize) -> Option<usize>;

    /// Unmaps an area from [`PageTable::map_area`], removes it from every
    /// CPU's TLB and frees its memory.
    fn unmap_area(virt: usize, pages: usize);

    /// Checks that TLB shootdowns reach every CPU and that switching
    /// address spaces never uses stale TLB entries. `on_others` runs a
    /// function on every other online CPU and returns once all have run
    /// it. Returns what went wrong, if anything.
    fn self_test_tlb(on_others: &dyn Fn(fn())) -> Result<(), &'static str>;

    /// What the TLB shootdowns CPU `cpu` made cost it, for the
    /// benchmarks.
    fn shootdowns(cpu: usize) -> Shootdowns;
}

/// What a CPU's TLB shootdowns cost it, counted since boot.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Shootdowns {
    /// Nanoseconds spent waiting for other CPUs' answers.
    pub wait_ns: u64,
    /// Other CPUs left to the hypervisor to flush, because the host had
    /// stopped them, instead of being interrupted or waited for.
    pub left: u64,
}

/// A deliberate fault for a self-test of the fault path.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FaultTest {
    /// Write to an address that is never mapped.
    Unmapped,
    /// Run code in a user page, which SMEP forbids the kernel.
    UserCode,
    /// Read a user page, which SMAP forbids the kernel.
    UserData,
}

/// Register state saved when an exception or breakpoint interrupted the
/// kernel.
#[derive(Clone, Copy, Debug)]
pub struct Fault {
    /// Address of the instruction that faulted, or that follows a
    /// breakpoint.
    pub pc: usize,
    /// Frame pointer at that point, where its backtrace starts.
    pub frame_pointer: usize,
    /// Stack pointer at that point.
    pub stack_pointer: usize,
    /// Every register worth showing, as (name, value) pairs.
    pub registers: [(&'static str, u64); 24],
    /// How many entries of `registers` are used.
    pub register_count: usize,
}

impl Fault {
    /// The used part of [`Fault::registers`].
    pub fn registers(&self) -> &[(&'static str, u64)] {
        &self.registers[..self.register_count.min(self.registers.len())]
    }
}

/// Machine-wide services.
pub trait Platform {
    /// Discovers the machine's firmware tables, interrupt controllers and
    /// timers, and starts the periodic timer on the boot CPU. Called once,
    /// on the boot CPU, before [`Smp::start_secondary_cpus`].
    fn init(params: &BootParams);

    /// The fault this CPU is panicking for, if the panic came from an
    /// exception handler.
    fn current_fault() -> Option<Fault>;

    /// Registers the function a breakpoint instruction calls. Without one,
    /// breakpoints are logged and skipped.
    fn set_breakpoint_handler(handler: fn(&Fault));

    /// Restarts the machine without powering it off, so RAM keeps its
    /// contents where the hardware allows.
    fn reset() -> !;

    /// Cuts the power: ACPI's soft-off state, S5, on a PC (Phase 9). The
    /// caller has put everything on disk first. Returns only if the
    /// machine cannot be powered off, and then the caller halts or
    /// restarts it.
    fn power_off();

    /// A byte typed on the debug console (the serial port), if any.
    fn console_read() -> Option<u8>;

    /// Whether a key was pressed on the built-in keyboard since the last
    /// call.
    fn key_pressed() -> bool;

    /// The memory reserved for the crash log, which the next boot reads.
    /// Returns it once; `None` if there is none on this machine.
    fn take_crash_log_region() -> Option<&'static mut [u8]>;

    /// Tells an emulator that runs the kernel for tests to exit with the
    /// given outcome. Returns when there is no such emulator.
    fn emulator_exit(success: bool);

    /// Prints the firmware tables found by [`Platform::init`], one per call
    /// of `line`.
    fn describe_firmware(line: &mut dyn FnMut(core::fmt::Arguments<'_>));

    /// The firmware's SMBIOS tables, which [`Platform::init`] found: on a
    /// PC, through the UEFI system table or in the BIOS's area. `None`
    /// where there are none.
    fn smbios() -> Option<Smbios>;

    /// Whether there is a debug console (a serial port) for the kernel
    /// debugger to talk on.
    fn has_debug_console() -> bool;

    /// Writes to the debug console (the serial port) only, for the kernel
    /// debugger.
    fn console_write(text: &str);

    /// The kernel symbol table that `ob build` wrote into the image (see
    /// `radix-ksyms`), or an empty slice.
    fn kernel_symbols() -> &'static [u8];

    /// Causes the fault `test` names, to test the fault path: the fault
    /// panics with a full report. Returns, with the reason, only when this
    /// machine cannot run the test (no SMEP or SMAP).
    fn self_test_fault(test: FaultTest) -> &'static str;

    /// Reads a model-specific register every processor has, which must
    /// read, and one none has, which must fault and go on: what that read
    /// gave where it did not fault (as under QEMU without KVM), or what
    /// went wrong. `None` where the architecture has no such registers.
    fn self_test_msr() -> Option<Result<Option<u64>, &'static str>>;
}

/// Signature of the kernel's entry point, `radix_kernel_main`, which the
/// architecture calls once it has set up the boot CPU.
pub type KernelMain = fn(BootParams) -> !;
