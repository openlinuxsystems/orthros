// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! The crash log: everything the consoles show, also kept in RAM that
//! survives a warm restart, so the next boot can show how this one ended.

use radix_hal::{Cpu, Platform};
use radix_klog::crashlog::{CrashLog, Ending};
use radix_klog::{Level, Sink, Style, info};
use radix_sync::SpinLock;

use crate::Arch;

static LOG: SpinLock<Option<CrashLog>> = SpinLock::new(None);

/// The log's console sink. It takes every message, debug ones included.
struct CrashLogSink;

impl Sink for CrashLogSink {
    fn write(&self, text: &str, _style: Style) {
        if let Some(log) = LOG.lock().as_mut() {
            log.write(text, &mut |bytes| Arch::flush_to_memory(bytes));
        }
    }

    fn wants_all_levels(&self) -> bool {
        true
    }
}

static SINK: CrashLogSink = CrashLogSink;

/// Opens the crash log region, if the architecture reserved one, and
/// starts copying the console output into it (from the start of boot).
pub fn init() {
    let Some(region) = Arch::take_crash_log_region() else { return };
    let len = region.len();
    match CrashLog::open(region) {
        Some(log) => {
            *LOG.lock() = Some(log);
            radix_klog::add_sink(&SINK, true);
        }
        None => radix_klog::warn!("Crash log: region of {len} bytes is too small"),
    }
}

/// Whether this boot has a crash log.
pub fn active() -> bool {
    LOG.lock().is_some()
}

/// This boot's number, counted by the crash log (1 if there is none).
pub fn boot_number() -> u32 {
    LOG.lock().as_ref().map_or(1, CrashLog::boot)
}

/// Records how this boot ends.
pub fn set_ending(ending: Ending) {
    if let Some(log) = LOG.lock().as_mut() {
        log.set_ending(ending, &mut |bytes| Arch::flush_to_memory(bytes));
    }
}

/// How the previous boot ended, and whether its log contains `needle`.
pub fn previous(needle: &str) -> Option<(Ending, bool)> {
    let log = LOG.lock();
    let previous = log.as_ref()?.previous()?;
    Some((previous.ending, previous.contains(needle)))
}

/// Lines of the previous boot's log shown at boot.
const SHOWN_AFTER_PANIC: usize = 40;
const SHOWN_OTHERWISE: usize = 12;

/// Reports the previous boot's log, if the region held one: how that boot
/// ended and its last lines.
pub fn report_previous() {
    let guard = LOG.lock();
    let Some(current) = guard.as_ref() else {
        drop(guard);
        info!("Crash log: none (no region reserved, or disabled with crashlog=off)");
        return;
    };
    let boot = current.boot();
    let Some(previous) = current.previous() else {
        // Logging writes to the crash log, so its lock must be free.
        drop(guard);
        info!("Crash log: boot {boot}; no log from a previous boot in memory");
        return;
    };
    let shown =
        if previous.ending == Ending::Panicked { SHOWN_AFTER_PANIC } else { SHOWN_OTHERWISE };
    let total = previous.line_count();
    let skip = total.saturating_sub(shown);
    let level = if previous.ending == Ending::Panicked { Level::Warn } else { Level::Info };
    // Copy the lines out before logging: logging writes to this region.
    let mut lines = [const { None }; SHOWN_AFTER_PANIC];
    for (slot, line) in lines.iter_mut().zip(previous.lines().skip(skip)) {
        *slot = Some(line);
    }
    let (previous_boot, ending) = (previous.boot, previous.ending);
    drop(guard);
    radix_klog::log(
        level,
        format_args!(
            "Crash log: boot {boot}; the previous boot ({previous_boot}) {ending}. Its last {} \
             lines:",
            total - skip
        ),
    );
    for line in lines.iter().flatten() {
        radix_klog::log(level, format_args!("  | {line}"));
    }
}
