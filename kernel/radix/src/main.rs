//! Radix, the OrthrOS kernel.
//!
//! Phase 1 brings the machine up: the architecture crate boots the CPU and
//! calls [`radix_kernel_main`], which brings up the consoles (serial, the
//! framebuffer, and the crash log kept in RAM across a restart), reports the
//! memory map, starts the interrupt controllers and timers, starts every
//! other CPU and checks that each one takes timer interrupts and answers
//! IPIs. A panic stops the other CPUs and shows a full-screen report with a
//! symbolized backtrace; `kdb` debugs the kernel over the serial port.
//!
//! Everything here is independent of the processor architecture: hardware
//! is reached only through the `radix-hal` traits, implemented by
//! `radix-arch-x86_64` (and by AArch64 stubs that CI compiles, so this
//! crate stays portable). The command-line flags are listed in
//! `radix_hal::cmdline::FLAGS` and documented in orthros-boot(7).
//!
//! The crate needs `unsafe` in two places only, each allowed where it is
//! used: exporting the entry point under a fixed name, and switching the
//! locks into panic mode.

#![no_std]
#![no_main]
#![deny(unsafe_code)]

mod console;
mod crashlog;
mod kdb;
mod panic;
mod report;
mod selftest;
mod symbols;

#[cfg(target_arch = "aarch64")]
use radix_arch_aarch64::Arch;
#[cfg(target_arch = "x86_64")]
use radix_arch_x86_64::Arch;

use radix_hal::cmdline::CommandLine;
use radix_hal::{BootParams, Cpu, InterruptController, Ipi, Platform, Smp, Timer};
use radix_klog::{Level, error, info, warn};
use radix_sync::Once;

/// Kernel settings from the command line.
#[derive(Clone, Copy, Debug)]
pub struct Settings {
    /// `qemu-exit`: leave QEMU when boot finishes or the kernel panics.
    pub qemu_exit: bool,
    /// `kdb`: run the kernel debugger on the serial port.
    pub kdb: bool,
    /// `panic=seconds`: restart this long after a panic.
    pub panic_restart: Option<u64>,
    /// `selftest=name`.
    pub selftest: Option<&'static str>,
}

static SETTINGS: Once<Settings> = Once::new();

/// The settings, or defaults before they are parsed.
pub fn settings() -> Settings {
    SETTINGS.get().copied().unwrap_or(Settings {
        qemu_exit: false,
        kdb: false,
        panic_restart: None,
        selftest: None,
    })
}

/// What the boot loader and firmware told the kernel, kept for the
/// debugger's `mem` command.
pub struct BootSummary {
    /// The memory map.
    pub memory_map: &'static [radix_hal::MemoryRegion],
    /// Reserved ranges.
    pub reservations: &'static [radix_hal::Reservation],
}

static BOOT: Once<BootSummary> = Once::new();

/// The boot summary, once recorded.
pub fn boot_summary() -> Option<&'static BootSummary> {
    BOOT.get()
}

/// The kernel's entry point, called by the architecture crate on the boot
/// CPU with interrupts off.
// The architecture crate declares this symbol with the same signature
// (`radix_hal::KernelMain`), so exporting it unmangled is sound.
#[allow(unsafe_code)]
#[unsafe(no_mangle)]
pub fn radix_kernel_main(mut params: BootParams) -> ! {
    let flags = CommandLine::new(params.command_line);
    let settings = Settings {
        qemu_exit: flags.has("qemu-exit"),
        kdb: flags.has("kdb"),
        panic_restart: flags.value("panic").and_then(|v| v.parse().ok()),
        selftest: flags.value("selftest"),
    };
    let _ = SETTINGS.set(settings);
    if let Some(level) = flags.value("loglevel") {
        match Level::parse(level) {
            Some(level) => radix_klog::set_console_level(level),
            None => warn!("Unknown log level `{level}`; using info"),
        }
    }

    // The screen first, so everything from here on shows on machines
    // without a serial port; it replays what was logged before.
    if let Some(framebuffer) = params.framebuffer.take() {
        console::init_framebuffer(framebuffer, flags.value("font"));
    }
    crashlog::init();

    info!("Radix {} (OrthrOS Phase 1)", env!("CARGO_PKG_VERSION"));
    info!("Boot loader: {}", params.boot_loader);
    info!("Firmware: {}", params.firmware);
    info!("Command line: {}", params.command_line);
    for flag in flags.unknown() {
        warn!("Unknown kernel flag `{flag}` ignored; see orthros-boot(7)");
    }
    let _ =
        BOOT.set(BootSummary { memory_map: params.memory_map, reservations: params.reservations });
    report::memory(&params);
    console::report();
    crashlog::report_previous();

    Arch::init(&params);
    if settings.kdb {
        Arch::set_breakpoint_handler(kdb::on_breakpoint);
    }
    Arch::enable_interrupts();

    let online = if flags.has("nosmp") {
        info!("SMP: disabled by the nosmp flag");
        1
    } else {
        Arch::start_secondary_cpus(ap_main)
    };
    report::cpus(online);
    check_timers(online);
    check_ipis(online);

    if let Some(name) = settings.selftest {
        selftest::run(name);
    }

    info!("Boot complete.");
    if settings.kdb {
        kdb::run_at_boot();
    }
    if settings.qemu_exit {
        Arch::emulator_exit(true);
    }
    idle()
}

/// Where the other CPUs start, with interrupts off.
fn ap_main(_index: usize) -> ! {
    Arch::enable_interrupts();
    idle()
}

/// Sleeps between interrupts forever.
fn idle() -> ! {
    Arch::enable_interrupts();
    loop {
        Arch::wait_for_interrupt();
    }
}

/// Waits until every online CPU has taken timer interrupts.
fn check_timers(online: usize) {
    if Arch::tick_hz() == 0 {
        error!("Timer: the periodic timer is not running");
        return;
    }
    let deadline = Arch::now_ns() + 2_000_000_000;
    let ticking = |cpu: usize| Arch::ticks(cpu) >= 3;
    while !(0..online).all(ticking) && Arch::now_ns() < deadline {
        Arch::delay_us(1_000);
    }
    let mut counts = radix_klog::TextBuf::<256>::new();
    for cpu in 0..online {
        use core::fmt::Write;
        let _ = write!(counts, "{}cpu{cpu} {}", if cpu == 0 { "" } else { ", " }, Arch::ticks(cpu));
    }
    if (0..online).all(ticking) {
        info!(
            "Timer: all {online} CPUs take timer interrupts at {} Hz ({counts})",
            Arch::tick_hz()
        );
    } else {
        error!("Timer: not every CPU takes timer interrupts ({counts})");
    }
}

/// Pings every other online CPU in turn and waits for each answer.
fn check_ipis(online: usize) {
    if online < 2 {
        return;
    }
    let mut missing = 0;
    for cpu in 1..online {
        let before = Arch::pings_received(cpu);
        Arch::send_ipi(cpu, Ipi::Ping);
        let deadline = Arch::now_ns() + 500_000_000;
        while Arch::pings_received(cpu) == before && Arch::now_ns() < deadline {
            Arch::delay_us(100);
        }
        if Arch::pings_received(cpu) == before {
            error!("IPI: CPU {cpu} did not answer a ping");
            missing += 1;
        }
    }
    if missing == 0 {
        info!("IPI: all {} other CPUs answered a ping ({})", online - 1, Arch::name());
    }
}
