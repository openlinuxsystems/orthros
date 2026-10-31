// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! `klog`, the kernel log.
//!
//! Every kernel message goes through [`log`] (usually through the [`error!`],
//! [`warn!`], [`info!`] and [`debug!`] macros). A message is stored in a
//! fixed ring of recent records and written to every registered console
//! ([`Sink`]): the serial port, the screen and the crash log. The ring is a
//! static, so logging works from the first instruction of Rust code, before
//! any console exists; a console registered later can replay what it missed.
//!
//! The architecture registers a [`Platform`] early, which supplies
//! timestamps, the current CPU and interrupt masking. Until then messages
//! carry time 0 and CPU 0.
//!
//! Also here: [`trace`] points, a lock-free event recorder cheap enough for
//! interrupt handlers, and [`crashlog`], the format of the log kept in memory
//! across a warm reboot.

#![cfg_attr(not(test), no_std)]
#![forbid(unsafe_code)]

pub mod crashlog;
mod text;
pub mod trace;

pub use text::TextBuf;

use core::fmt::{self, Write};
use core::sync::atomic::{AtomicU8, Ordering};

use radix_sync::{Once, SpinLock};

/// How important a message is.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
#[repr(u8)]
pub enum Level {
    /// Something failed.
    Error = 0,
    /// Something is wrong but the kernel carries on.
    Warn = 1,
    /// Normal progress.
    Info = 2,
    /// Detail for developers.
    Debug = 3,
}

impl Level {
    /// Parses `error`, `warn`, `info` or `debug`.
    pub fn parse(name: &str) -> Option<Level> {
        match name {
            "error" => Some(Level::Error),
            "warn" => Some(Level::Warn),
            "info" => Some(Level::Info),
            "debug" => Some(Level::Debug),
            _ => None,
        }
    }

    /// Lower-case name.
    pub fn name(self) -> &'static str {
        match self {
            Level::Error => "error",
            Level::Warn => "warn",
            Level::Info => "info",
            Level::Debug => "debug",
        }
    }

    fn from_u8(value: u8) -> Level {
        match value {
            0 => Level::Error,
            1 => Level::Warn,
            2 => Level::Info,
            _ => Level::Debug,
        }
    }
}

/// How a console should present a piece of text.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Style {
    /// Ordinary text.
    Plain,
    /// An error message.
    Error,
    /// A warning.
    Warning,
    /// Developer detail, shown dimmed.
    Debug,
    /// A heading or anything that must stand out.
    Emphasis,
    /// The title bar of the panic report: light text on a red band.
    Banner,
}

impl From<Level> for Style {
    fn from(level: Level) -> Style {
        match level {
            Level::Error => Style::Error,
            Level::Warn => Style::Warning,
            Level::Info => Style::Plain,
            Level::Debug => Style::Debug,
        }
    }
}

/// Services the kernel log needs from the architecture.
pub trait Platform: Sync {
    /// Nanoseconds since boot, or 0 before the clock is calibrated.
    fn now_ns(&self) -> u64;
    /// Index of the current CPU.
    fn cpu(&self) -> usize;
    /// Masks interrupts and returns whether they were enabled.
    fn disable_interrupts(&self) -> bool;
    /// Restores the state returned by `disable_interrupts`.
    fn restore_interrupts(&self, were_enabled: bool);
}

/// A console the log writes to.
pub trait Sink: Sync {
    /// Writes `text`, which ends with a newline and may hold several lines.
    fn write(&self, text: &str, style: Style);

    /// Whether the sink takes messages below the console level too. The
    /// crash log does; screens and serial ports do not.
    fn wants_all_levels(&self) -> bool {
        false
    }
}

/// Number of records the ring keeps.
pub const RECORDS: usize = 512;
/// Longest message text kept per record, in bytes.
pub const RECORD_TEXT: usize = 200;
/// Most consoles at once.
const MAX_SINKS: usize = 8;

/// One message in the ring.
#[derive(Clone, Copy)]
pub struct Record {
    /// Position in the sequence of all messages since boot.
    pub seq: u64,
    /// Time the message was logged.
    pub time_ns: u64,
    /// Its level.
    pub level: Level,
    /// CPU that logged it.
    pub cpu: u16,
    /// The number of the process whose program wrote it, through the
    /// kernel log capability or _/devices/kmsg_; 0 for the kernel's own.
    pub process: u32,
    /// Its syslog(3) facility (see [`Origin`]).
    pub facility: u8,
    len: u8,
    text: [u8; RECORD_TEXT],
}

impl Record {
    const EMPTY: Record = Record {
        seq: 0,
        time_ns: 0,
        level: Level::Info,
        cpu: 0,
        process: 0,
        facility: KERN,
        len: 0,
        text: [0; RECORD_TEXT],
    };

    /// The message text (one line).
    pub fn text(&self) -> &str {
        let bytes = &self.text[..usize::from(self.len)];
        // The text was copied from a `&str` and cut at a character boundary.
        core::str::from_utf8(bytes).unwrap_or("<invalid>")
    }

    /// Formats the record as a console line, without the newline.
    pub fn format(&self, out: &mut impl Write) -> fmt::Result {
        write_prefix(out, self.time_ns, usize::from(self.cpu))?;
        out.write_str(self.text())
    }
}

impl fmt::Debug for Record {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Record")
            .field("seq", &self.seq)
            .field("time_ns", &self.time_ns)
            .field("level", &self.level)
            .field("cpu", &self.cpu)
            .field("process", &self.process)
            .field("facility", &self.facility)
            .field("text", &self.text())
            .finish_non_exhaustive()
    }
}

struct State {
    records: [Record; RECORDS],
    next_seq: u64,
    sinks: [Option<&'static dyn Sink>; MAX_SINKS],
}

static STATE: SpinLock<State> = SpinLock::new(State {
    records: [Record::EMPTY; RECORDS],
    next_seq: 0,
    sinks: [None; MAX_SINKS],
});

static PLATFORM: Once<&'static dyn Platform> = Once::new();
static CONSOLE_LEVEL: AtomicU8 = AtomicU8::new(Level::Info as u8);

/// Registers the architecture's services. Only the first call counts.
pub fn set_platform(platform: &'static dyn Platform) {
    let _ = PLATFORM.set(platform);
}

/// Nanoseconds since boot, from the registered platform.
pub fn now_ns() -> u64 {
    PLATFORM.get().map_or(0, |p| p.now_ns())
}

/// The current CPU, from the registered platform.
pub fn current_cpu() -> usize {
    PLATFORM.get().map_or(0, |p| p.cpu())
}

/// Sets the lowest level written to consoles. Every level is kept in the
/// ring regardless.
pub fn set_console_level(level: Level) {
    CONSOLE_LEVEL.store(level as u8, Ordering::Relaxed);
}

/// The lowest level written to consoles.
pub fn console_level() -> Level {
    Level::from_u8(CONSOLE_LEVEL.load(Ordering::Relaxed))
}

/// Runs `f` with interrupts masked and the log locked.
fn with_state<R>(f: impl FnOnce(&mut State) -> R) -> R {
    let platform = PLATFORM.get();
    let were_enabled = platform.is_some_and(|p| p.disable_interrupts());
    let result = f(&mut STATE.lock());
    if let Some(p) = platform {
        p.restore_interrupts(were_enabled);
    }
    result
}

/// Adds a console. With `replay`, the messages already in the ring are
/// written to it first, so a screen that comes up late still shows the
/// whole boot.
pub fn add_sink(sink: &'static dyn Sink, replay: bool) -> bool {
    with_state(|state| {
        let Some(slot) = state.sinks.iter_mut().find(|slot| slot.is_none()) else {
            return false;
        };
        *slot = Some(sink);
        if replay {
            let level = console_level();
            let first = state.next_seq.saturating_sub(RECORDS as u64);
            for seq in first..state.next_seq {
                let record = &state.records[(seq % RECORDS as u64) as usize];
                if record.level <= level || sink.wants_all_levels() {
                    let mut line = TextBuf::<{ RECORD_TEXT + 40 }>::new();
                    let _ = record.format(&mut line);
                    line.push_str("\n");
                    sink.write(line.as_str(), record.level.into());
                }
            }
        }
        true
    })
}

/// The kernel's facility, of its own records.
pub const KERN: u8 = 0;
/// The facility of a program's records unless it names another.
pub const USER: u8 = 1;
/// The facility of security events (LOG_AUTHPRIV), which logd(8) keeps
/// in the audit log when the kernel or Spark logs them (ADR 0211).
pub const AUTHPRIV: u8 = 10;
/// The facilities there are: 0 to 23, local7 the last.
pub const FACILITIES: u8 = 24;

/// Who a record is from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Origin {
    /// The number of the process whose program wrote it; 0 for the
    /// kernel's own.
    pub process: u32,
    /// Its syslog(3) facility: [`KERN`] for the kernel's own, [`USER`] for
    /// a program's unless it names another, and [`AUTHPRIV`] for a
    /// security event.
    pub facility: u8,
}

impl Origin {
    /// The kernel's own records.
    pub const KERNEL: Origin = Origin { process: 0, facility: KERN };
}

/// Logs a message. Multi-line messages become one record per line.
pub fn log(level: Level, args: fmt::Arguments<'_>) {
    log_from(level, Origin::KERNEL, args);
}

/// Logs a security event of the kernel's, for the audit log (ADR 0211).
pub fn audit(args: fmt::Arguments<'_>) {
    log_from(Level::Info, Origin { process: 0, facility: AUTHPRIV }, args);
}

/// Logs a message from `origin`, as [`log`] does: a program's, or one of
/// the kernel's of another facility than its own.
pub fn log_from(level: Level, origin: Origin, args: fmt::Arguments<'_>) {
    let time_ns = now_ns();
    let cpu = current_cpu();
    let mut message = TextBuf::<512>::new();
    let _ = message.write_fmt(args);
    let console = level <= console_level();
    with_state(|state| {
        for line in message.as_str().trim_end_matches('\n').split('\n') {
            let seq = state.next_seq;
            state.next_seq += 1;
            let record = &mut state.records[(seq % RECORDS as u64) as usize];
            record.seq = seq;
            record.time_ns = time_ns;
            record.level = level;
            record.cpu = u16::try_from(cpu).unwrap_or(u16::MAX);
            record.process = origin.process;
            record.facility = origin.facility;
            let kept = text::floor_char_boundary(line, RECORD_TEXT);
            record.text[..kept].copy_from_slice(&line.as_bytes()[..kept]);
            record.len = kept as u8;

            let mut out = TextBuf::<600>::new();
            let _ = write_prefix(&mut out, time_ns, cpu);
            out.push_str(line);
            out.push_str("\n");
            for sink in state.sinks.iter().flatten() {
                if console || sink.wants_all_levels() {
                    sink.write(out.as_str(), level.into());
                }
            }
        }
    });
}

/// Writes text to every console without making it a record: used for the
/// panic report and for the kernel debugger's output.
pub fn write_raw(text: &str, style: Style) {
    with_state(|state| {
        for sink in state.sinks.iter().flatten() {
            sink.write(text, style);
        }
    });
}

/// Formats and writes text with [`write_raw`].
pub fn print(style: Style, args: fmt::Arguments<'_>) {
    let mut text = TextBuf::<1024>::new();
    let _ = text.write_fmt(args);
    write_raw(text.as_str(), style);
}

/// Sequence numbers of the records still in the ring: `first..next`.
pub fn record_range() -> core::ops::Range<u64> {
    with_state(|state| state.next_seq.saturating_sub(RECORDS as u64)..state.next_seq)
}

/// A copy of record `seq`, if it is still in the ring.
pub fn record(seq: u64) -> Option<Record> {
    with_state(|state| {
        let record = state.records[(seq % RECORDS as u64) as usize];
        (record.seq == seq && seq < state.next_seq).then_some(record)
    })
}

/// Writes `[    1.234567] ` and, for CPUs other than the boot CPU,
/// `cpuN: `.
fn write_prefix(out: &mut impl Write, time_ns: u64, cpu: usize) -> fmt::Result {
    let secs = time_ns / 1_000_000_000;
    let micros = (time_ns % 1_000_000_000) / 1_000;
    write!(out, "[{secs:5}.{micros:06}] ")?;
    if cpu != 0 {
        write!(out, "cpu{cpu}: ")?;
    }
    Ok(())
}

/// Logs an error.
#[macro_export]
macro_rules! error {
    ($($arg:tt)*) => { $crate::log($crate::Level::Error, format_args!($($arg)*)) };
}

/// Logs a warning.
#[macro_export]
macro_rules! warn {
    ($($arg:tt)*) => { $crate::log($crate::Level::Warn, format_args!($($arg)*)) };
}

/// Logs normal progress.
#[macro_export]
macro_rules! info {
    ($($arg:tt)*) => { $crate::log($crate::Level::Info, format_args!($($arg)*)) };
}

/// Logs developer detail.
#[macro_export]
macro_rules! debug {
    ($($arg:tt)*) => { $crate::log($crate::Level::Debug, format_args!($($arg)*)) };
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    /// The log is a global, so tests that use it run one at a time.
    pub(crate) static SERIAL: Mutex<()> = Mutex::new(());

    struct Capture(Mutex<String>);

    impl Sink for Capture {
        fn write(&self, text: &str, _style: Style) {
            self.0.lock().unwrap().push_str(text);
        }
    }

    #[test]
    fn records_and_replays() {
        let _serial = SERIAL.lock().unwrap();
        info!("first {}", 1);
        debug!("hidden detail");
        warn!("two\nlines");
        let range = record_range();
        let texts: Vec<String> =
            range.clone().filter_map(record).map(|r| r.text().to_string()).collect();
        assert!(texts.ends_with(&[
            "first 1".to_string(),
            "hidden detail".to_string(),
            "two".to_string(),
            "lines".to_string()
        ]));

        let sink: &'static Capture = Box::leak(Box::new(Capture(Mutex::new(String::new()))));
        assert!(add_sink(sink, true));
        let replayed = sink.0.lock().unwrap().clone();
        assert!(replayed.contains("[    0.000000] first 1\n"));
        assert!(!replayed.contains("hidden detail"), "debug is below the console level");
        info!("after");
        assert!(sink.0.lock().unwrap().ends_with("after\n"));
    }

    #[test]
    fn long_messages_are_cut_at_a_character_boundary() {
        let _serial = SERIAL.lock().unwrap();
        let long = "é".repeat(150);
        info!("{long}");
        let last = record(record_range().end - 1).unwrap();
        assert!(last.text().len() <= RECORD_TEXT);
        assert!(last.text().chars().all(|c| c == 'é'));
    }

    #[test]
    fn prefixes_show_time_and_cpu() {
        let mut out = String::new();
        write_prefix(&mut out, 1_234_567_891, 3).unwrap();
        assert_eq!(out, "[    1.234567] cpu3: ");
        assert_eq!(Level::parse("warn"), Some(Level::Warn));
        assert_eq!(Level::parse("loud"), None);
    }
}
