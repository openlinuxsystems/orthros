// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! `kdb`, the kernel debugger, on the serial port.
//!
//! With the `kdb` flag the kernel enters it at the end of boot, at every
//! breakpoint, and after a panic. It reads commands with a small line editor
//! and answers on the serial port only; the commands are listed in
//! `radix_kdb::COMMANDS` and described in radix-debug(7). On a machine
//! without a serial port `kdb` says so in the log and stays out of the way.
//!
//! Only the CPU that entered `kdb` stops; the others keep running (they are
//! already stopped after a panic).

use core::fmt::{self, Write};
use core::sync::atomic::{AtomicBool, Ordering};

use radix_hal::{Fault, InterruptController, PageTable, Platform, Smp, Timer};
use radix_kdb::{Command, LineEditor, SymbolQuery, TraceAction};
use radix_klog::crashlog::Ending;
use radix_klog::{info, trace, warn};

use crate::symbols::{self, Backtrace};
use crate::{Arch, caps, crashlog, domains, oom, report, sched};

/// Set while a session runs, so a breakpoint inside `kdb` does not nest.
static ACTIVE: AtomicBool = AtomicBool::new(false);

/// Longest command line.
const LINE: usize = 120;

/// Writes to the serial port.
struct Serial;

impl Write for Serial {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        Arch::console_write(s);
        Ok(())
    }
}

/// Writes one line to the serial port.
fn say(args: fmt::Arguments<'_>) {
    let _ = Serial.write_fmt(args);
    Arch::console_write("\n");
}

/// Why `kdb` was entered, and what it can show about that.
struct Session {
    /// Registers saved by the breakpoint or exception, if any.
    fault: Option<Fault>,
    /// The backtrace of the panic, if `kdb` runs after one.
    panic_backtrace: Option<Backtrace>,
}

impl Session {
    fn after_panic(&self) -> bool {
        self.panic_backtrace.is_some()
    }
}

/// Runs `kdb` at the end of boot.
pub fn run_at_boot() {
    if !Arch::has_debug_console() {
        warn!("kdb: no serial port, so the debugger cannot run; see radix-debug(7)");
        return;
    }
    info!("kdb: boot complete; waiting for commands on the serial port");
    enter(Session { fault: None, panic_backtrace: None }, "boot complete");
}

/// The breakpoint handler: runs `kdb` with the registers the breakpoint
/// saved, then lets the code carry on.
pub fn on_breakpoint(fault: &Fault) {
    if !Arch::has_debug_console() {
        warn!("kdb: breakpoint at {:#x} skipped: no serial port", fault.pc);
        return;
    }
    if ACTIVE.load(Ordering::Acquire) {
        say(format_args!("kdb: breakpoint at {:#x} inside kdb skipped", fault.pc));
        return;
    }
    let mut why = radix_klog::TextBuf::<160>::new();
    let _ = match symbols::lookup(fault.pc.wrapping_sub(1)) {
        Some(location) => write!(why, "breakpoint at {:#x} in {location}", fault.pc),
        None => write!(why, "breakpoint at {:#x}", fault.pc),
    };
    enter(Session { fault: Some(*fault), panic_backtrace: None }, why.as_str());
}

/// Runs `kdb` after a panic, with the other CPUs stopped. Returns when the
/// user leaves, and the panic handler goes on to restart.
pub fn run_after_panic(fault: Option<Fault>, backtrace: Backtrace) {
    if !Arch::has_debug_console() {
        return;
    }
    // A panic inside a session (the `panic` command) starts a new one.
    ACTIVE.store(false, Ordering::Release);
    enter(Session { fault, panic_backtrace: Some(backtrace) }, "after the panic above");
}

/// The command loop.
fn enter(session: Session, why: &str) {
    if ACTIVE.swap(true, Ordering::AcqRel) {
        return;
    }
    say(format_args!(""));
    say(format_args!(
        "kdb on CPU {}, {why}. Type `help` for commands, `continue` to leave.",
        <Arch as radix_hal::CpuLocal>::current_cpu()
    ));
    let mut editor = LineEditor::<LINE>::new();
    Arch::console_write("kdb> ");
    loop {
        let Some(byte) = Arch::console_read() else {
            Arch::delay_us(100);
            continue;
        };
        let Some(line) = editor.feed(byte, &mut |echo| Arch::console_write(echo)) else {
            continue;
        };
        match radix_kdb::parse(line) {
            Ok(Command::Continue) if session.after_panic() => {
                say(format_args!("kdb: leaving; the machine restarts as the panic report says"));
                break;
            }
            Ok(Command::Continue) => {
                say(format_args!("kdb: continuing"));
                break;
            }
            Ok(command) => run(&session, command),
            Err(error) => say(format_args!("{error}")),
        }
        Arch::console_write("kdb> ");
    }
    ACTIVE.store(false, Ordering::Release);
}

/// Carries out one command.
fn run(session: &Session, command: Command<'_>) {
    match command {
        Command::Nothing | Command::Continue => {}
        Command::Help => {
            let _ = radix_kdb::write_help(&mut Serial);
        }
        Command::Log { count } => show_log(count),
        Command::Trace(action) => show_trace(action),
        Command::Cpus => show_cpus(),
        Command::Memory => show_memory(),
        Command::Heap => report::write_heap(&mut |args| say(args)),
        Command::Capabilities => caps::write_census(&mut |args| say(args)),
        Command::Threads => sched::write_threads(&mut |args| say(args)),
        Command::Scheduler => sched::write_queues(&mut |args| say(args)),
        Command::Domains => {
            domains::write_table(&mut |args| say(args));
            let (stops, restarts, gave_up) = oom::stats();
            say(format_args!(
                "Out of memory: {stops} domains stopped, {restarts} restarted, {gave_up} times \
                 nothing could be stopped"
            ));
        }
        Command::Locks => sched::write_lockdep(&mut |args| say(args)),
        Command::Acpi => Arch::describe_firmware(&mut |args| say(args)),
        Command::Backtrace => {
            let backtrace = match (&session.panic_backtrace, &session.fault) {
                (Some(backtrace), _) => *backtrace,
                (None, Some(fault)) => Backtrace::from_fault(fault.pc, fault.frame_pointer),
                (None, None) => Backtrace::here(),
            };
            backtrace.write(&mut |args| say(args));
        }
        Command::Registers => match &session.fault {
            Some(fault) => {
                for row in fault.registers().chunks(4) {
                    for (name, value) in row {
                        let _ = write!(Serial, "  {name:>6} {value:016x}");
                    }
                    say(format_args!(""));
                }
            }
            None => say(format_args!(
                "no saved registers: kdb was not entered from a breakpoint or an exception"
            )),
        },
        Command::Examine { address, len } => examine(address, len),
        Command::Symbol(query) => symbol(query),
        Command::Panic if session.after_panic() => {
            say(format_args!("the kernel has already panicked"));
        }
        Command::Panic => panic!("kdb: panic requested with the `panic` command"),
        Command::Reboot => {
            say(format_args!("kdb: restarting"));
            if !session.after_panic() {
                crashlog::set_ending(Ending::Restarted);
            }
            Arch::reset();
        }
    }
}

fn show_log(count: usize) {
    let range = radix_klog::record_range();
    let first = range.end.saturating_sub(count as u64).max(range.start);
    for seq in first..range.end {
        if let Some(record) = radix_klog::record(seq) {
            let mut line = radix_klog::TextBuf::<300>::new();
            let _ = record.format(&mut line);
            say(format_args!("{line}"));
        }
    }
}

fn show_trace(action: TraceAction) {
    match action {
        TraceAction::On => {
            trace::set_enabled(true);
            say(format_args!("tracing on"));
        }
        TraceAction::Off => {
            trace::set_enabled(false);
            say(format_args!("tracing off"));
        }
        TraceAction::Show => {
            let mut shown = 0;
            trace::for_each(|event| {
                let (secs, micros) =
                    (event.time_ns / 1_000_000_000, event.time_ns % 1_000_000_000 / 1_000);
                say(format_args!(
                    "  #{:<6} {secs:>5}.{micros:06} cpu{:<3} {:<24} {:#x} {:#x} {:#x}",
                    event.seq, event.cpu, event.name, event.args[0], event.args[1], event.args[2]
                ));
                shown += 1;
            });
            let state = if trace::enabled() { "on" } else { "off" };
            say(format_args!(
                "{shown} events shown of {} recorded; tracing is {state} (`trace on` starts it, \
                 or boot with the trace flag)",
                trace::recorded()
            ));
        }
    }
}

fn show_cpus() {
    say(format_args!(
        "{} CPUs, interrupts by {}, timer at {} Hz:",
        Arch::cpu_count(),
        Arch::name(),
        Arch::tick_hz()
    ));
    for index in 0..Arch::cpu_count() {
        let Some(cpu) = Arch::cpu_info(index) else { continue };
        say(format_args!(
            "  cpu{index}: APIC ID {}{}, {}, {} timer interrupts, {} pings",
            cpu.hardware_id,
            if cpu.boot_cpu { ", boot CPU" } else { "" },
            if cpu.online { "online" } else { "offline" },
            Arch::ticks(index),
            Arch::pings_received(index)
        ));
    }
}

fn show_memory() {
    let Some(boot) = crate::boot_summary() else {
        say(format_args!("the memory map was not recorded"));
        return;
    };
    say(format_args!("Memory map:"));
    report::write_memory_map(boot.memory_map, &mut |args| say(args));
    say(format_args!("Reserved by the kernel:"));
    report::write_reservations(boot.reservations, &mut |args| say(args));
    say(format_args!("Frame allocator zones:"));
    report::write_zones(&mut |args| say(args));
    let (dropped, failed) = crate::fs::writeback::stats();
    say(format_args!(
        "Page cache: {} pages held, {dropped} dropped to reclaim memory, {failed} failed \
         write-backs",
        radix_block::mem::pages()
    ));
    Arch::describe(&mut |args| say(args));
}

fn examine(address: u64, len: usize) {
    const CHUNK: usize = 256;
    let mut buf = [0u8; CHUNK];
    let mut done = 0;
    while done < len {
        let n = CHUNK.min(len - done);
        let at = address.wrapping_add(done as u64);
        if !Arch::read(at as usize, &mut buf[..n]) {
            say(format_args!("{at:#x}: not mapped, or not RAM (kdb reads only RAM)"));
            return;
        }
        let _ = radix_kdb::write_hex_dump(&mut Serial, at, &buf[..n]);
        done += n;
    }
}

fn symbol(query: SymbolQuery<'_>) {
    let Some(table) = symbols::table() else {
        say(format_args!("no symbol table in this image"));
        return;
    };
    match query {
        SymbolQuery::Address(address) => match table.lookup(symbols::link_address(address)) {
            Some(location) => say(format_args!("{address:#x} is {location}")),
            None => say(format_args!("{address:#x} is not in any kernel function")),
        },
        SymbolQuery::Name(name) => match table.address_of(name).map(symbols::run_address) {
            Some(address) => say(format_args!("{name} is at {address:#x}")),
            None => say(format_args!("no function named `{name}`")),
        },
    }
}
