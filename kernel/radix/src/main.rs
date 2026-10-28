// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Radix, the Dukovo kernel.
//!
//! The architecture crate boots the CPU, moves the kernel to a random
//! address, builds its page tables and calls [`radix_kernel_main`], which
//! brings up the consoles (serial, the framebuffer, and the crash log kept
//! in RAM across a restart), reports the memory map and the address-space
//! layout, starts the interrupt controllers, timers and the frame
//! allocator (Phase 2, `radix-mm`), turns itself into the first thread and
//! starts the scheduler (Phase 4, `sched`, with resource domains in
//! `domains` and the out-of-memory policy in `oom`), sets up the capability
//! store (Phase 3, `radix-cap`, in `caps`) and user mode with its system
//! calls and IPC (Phase 5, `user`), finds the disks and sets up the file
//! tree (Phase 7, `fs`), starts every other CPU and
//! checks that each one takes timer interrupts and answers IPIs. A panic
//! stops the other CPUs and shows a full-screen report with a symbolized
//! backtrace; `kdb` debugs the kernel over the serial port.
//!
//! Memory comes from `radix_mm::HEAP` with explicit flags: the global
//! allocator refuses every request (ADR 0017), and `alloc` is built without
//! its infallible APIs, so every allocation states its context and handles
//! failure.
//!
//! Everything here is independent of the processor architecture: hardware
//! is reached only through the `radix-hal` traits, implemented by
//! `radix-arch-x86_64` (and by AArch64 stubs that CI compiles, so this
//! crate stays portable). The command-line flags are listed in
//! `radix_hal::cmdline::FLAGS` and documented in dukovo-boot(7).
//!
//! The crate needs `unsafe` in six places only, each allowed where it is
//! used: exporting the entry point under a fixed name, switching the locks
//! into panic mode, the context switch (`sched::switch`), which reaches
//! the two threads' contexts through pointers the scheduler keeps valid,
//! borrowing the running thread from its CPU's record without counting a
//! reference (`sched::running`), building an open file in the allocation
//! no other reference reaches yet (`fs::FileObject::opened`), and per-CPU
//! values only their own CPU uses (`percpu::Own`), which need no lock
//! because no other CPU reaches them.

#![no_std]
#![no_main]
#![deny(unsafe_code)]
#![feature(allocator_ext)]
#![feature(get_mut_unchecked)]
#![feature(vec_push_within_capacity)]

extern crate alloc;

mod caps;
mod captest;
mod console;
mod crashlog;
mod domains;
mod fs;
mod join;
mod kdb;
mod mce;
mod oom;
mod panic;
mod percpu;
mod report;
mod sched;
mod schedtest;
mod selftest;
mod symbols;
mod user;
mod usertest;
mod watchdog;
mod work;

#[cfg(target_arch = "aarch64")]
use radix_arch_aarch64::Arch;
#[cfg(target_arch = "x86_64")]
use radix_arch_x86_64::Arch;

use radix_hal::cmdline::CommandLine;
use radix_hal::{BootParams, Cpu, InterruptController, Ipi, PageTable, Platform, Smp, Timer};
use radix_klog::{Level, error, info, warn};
use radix_mm::{Corruption, ExplicitOnly, HEAP};
use radix_sync::Once;

/// The global allocator refuses every request: allocations go through
/// `HEAP.with(flags)` instead.
#[global_allocator]
static GLOBAL: ExplicitOnly = ExplicitOnly;

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
    /// `lockdep=on|off`, or the bare `lockdep` for on.
    pub lockdep: Option<bool>,
    /// `fuzz-seed=number`: the first seed of `selftest=fuzz`.
    pub fuzz_seed: Option<u64>,
}

static SETTINGS: Once<Settings> = Once::new();

/// The kernel's name and version, and the Dukovo release it is part of:
/// the first line of the boot log and `/kernel/version`
/// (radix-sysfs(5)), whose second word the C library gives as the
/// release (uname(1)).
pub const BANNER: &str = concat!(
    "Radix ",
    env!("CARGO_PKG_VERSION"),
    " (Dukovo Server ",
    env!("CARGO_PKG_VERSION"),
    ")"
);

/// The settings, or defaults before they are parsed.
pub fn settings() -> Settings {
    SETTINGS.get().copied().unwrap_or(Settings {
        qemu_exit: false,
        kdb: false,
        panic_restart: None,
        selftest: None,
        lockdep: None,
        fuzz_seed: None,
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
pub extern "Rust" fn radix_kernel_main(mut params: BootParams) -> ! {
    let flags = CommandLine::new(params.command_line);
    let settings = Settings {
        qemu_exit: flags.has("qemu-exit"),
        kdb: flags.has("kdb"),
        panic_restart: flags.value("panic").and_then(|v| v.parse().ok()),
        selftest: flags.value("selftest"),
        lockdep: match flags.value("lockdep") {
            Some("on") => Some(true),
            Some("off") => Some(false),
            _ if flags.has("lockdep") => Some(true),
            _ => None,
        },
        fuzz_seed: flags.value("fuzz-seed").and_then(|v| v.parse().ok()),
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

    info!("{BANNER}");
    info!("Boot loader: {}", params.boot_loader);
    info!("Firmware: {}", params.firmware);
    info!("Command line: {}", params.command_line);
    for flag in flags.unknown() {
        warn!("Unknown kernel flag `{flag}` ignored; see dukovo-boot(7)");
    }
    let _ =
        BOOT.set(BootSummary { memory_map: params.memory_map, reservations: params.reservations });
    report::memory(&params);
    Arch::describe(&mut |args| info!("{args}"));
    console::report();
    crashlog::report_previous();

    if let Some(value) = flags.value("lockdep")
        && !matches!(value, "on" | "off")
    {
        warn!("Unknown lockdep setting `{value}`; it is on or off");
    }

    HEAP.set_corruption_handler(on_heap_corruption);
    Arch::init(&params);
    report::frames();
    domains::init();
    sched::init(settings.lockdep);
    oom::init();
    if settings.kdb {
        Arch::set_breakpoint_handler(kdb::on_breakpoint);
    }
    Arch::enable_interrupts();
    // With interrupts on: the store's lock is a mutex.
    caps::init();
    user::init();
    fs::init();
    fs::sysfs::set_command_line(params.command_line);
    user::launch::set_modules(params.modules);
    user::init::set_modules(params.modules);

    let online = if flags.has("nosmp") {
        info!("SMP: disabled by the nosmp flag");
        1
    } else {
        Arch::start_secondary_cpus(ap_main)
    };
    report::cpus(online);
    sched::start_threads();
    work::start();
    join::start();
    oom::start();
    fs::disk::init();
    fs::writeback::start();
    fs::kmsg::start();
    fs::keyboard::start();
    user::power::start();
    watchdog::find();
    mce::start(flags.value("mce-poll"));
    check_timers(online);
    check_ipis(online);

    if let Some(name) = settings.selftest {
        selftest::run(name);
    }

    info!("Boot complete.");
    // The first program, unless a self-test has the machine to itself:
    // `init-after-selftest` starts it after the test too, for the disk
    // check that also runs the benchmarks (ADR 0124).
    let init = settings.selftest.is_none() || flags.has("init-after-selftest");
    if init && user::init::has_archive() {
        let path = flags.value("init").unwrap_or(user::init::DEFAULT_INIT);
        user::init::start(path, flags.init_args());
    }
    if settings.kdb {
        kdb::run_at_boot();
    }
    if settings.qemu_exit {
        Arch::emulator_exit(true);
    }
    // Boot is done; the other threads carry on.
    sched::exit()
}

/// Where the other CPUs start, with interrupts off: each becomes a
/// scheduler CPU, its start-up code its idle thread.
fn ap_main(index: usize) -> ! {
    sched::run_cpu(index)
}

/// The heap found damage it could not explain: the kernel cannot trust its
/// memory any more.
fn on_heap_corruption(corruption: &Corruption) {
    panic!("{corruption}");
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
