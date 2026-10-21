// SPDX-FileCopyrightText: 2026 The OrthrOS contributors
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
//! processor itself, [`Smp`] for starting and stopping the other CPUs, and
//! [`Platform`] for machine-wide services such as restarting. All methods
//! are associated functions: there is exactly one machine.
//!
//! The architecture owns the entry point. It sets up the CPU, the consoles it
//! knows about and the kernel's page tables, then calls the kernel's
//! `radix_kernel_main` with a [`BootParams`].

#![cfg_attr(not(test), no_std)]
#![forbid(unsafe_code)]

pub mod boot;
pub mod cmdline;

pub use boot::{
    BootParams, Channel, Firmware, Framebuffer, MemoryKind, MemoryRegion, PixelFormat, Reservation,
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
}

/// Data and identity of the current CPU.
pub trait CpuLocal {
    /// Index of the CPU running this code; the boot CPU is 0, and the
    /// others are numbered in the order the firmware lists them.
    fn current_cpu() -> usize;
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
}

/// Kinds of inter-processor interrupt the common kernel sends.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Ipi {
    /// Does nothing but count; used to test that IPIs arrive.
    Ping,
}

/// The interrupt controllers.
pub trait InterruptController {
    /// Short description, such as `x2APIC`.
    fn name() -> &'static str;

    /// Sends `ipi` to CPU `cpu`.
    fn send_ipi(cpu: usize, ipi: Ipi);

    /// How many [`Ipi::Ping`] interrupts CPU `cpu` has handled.
    fn pings_received(cpu: usize) -> u64;
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
}

/// Signature of the kernel's entry point, `radix_kernel_main`, which the
/// architecture calls once it has set up the boot CPU.
pub type KernelMain = fn(BootParams) -> !;
