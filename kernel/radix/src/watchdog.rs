// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! The machine's hardware watchdog (Phase 11, ADR 0143), which restarts
//! the machine unless it is petted in time.
//!
//! [`find`] asks the platform for one at boot, after the PCI scan, and
//! logs whether it ended the boot before. Spark starts it through the
//! power capability and pets it from its loop, so a Spark that stops
//! responding, or a kernel that stops running it, restarts the machine.
//! Before the machine is powered off or restarted, [`ending`] gives it its
//! longest timeout, up to [`ENDING_SECONDS`], so that a long write-back is
//! not cut short but a hung one still ends; before it is halted, it stops.
//! `/kernel/class/watchdog/watchdog0` shows its state, as Linux's sysfs
//! does.

use radix_hal::{Devices, Watchdog};
use radix_klog::{info, warn};
use radix_sync::{Once, SpinLock};

use crate::Arch;

/// The longest timeout while the filesystems are written back at the end.
pub const ENDING_SECONDS: u32 = 600;

/// The watchdog the platform found.
static FOUND: Once<Watchdog> = Once::new();

/// Whether it runs, and its timeout in seconds.
static STATE: SpinLock<(bool, u32)> = SpinLock::new((false, 0));

/// Asks the platform for the watchdog, which it leaves stopped.
pub fn find() {
    let Some(found) = Arch::find_watchdog() else {
        info!("Watchdog: none the kernel drives");
        return;
    };
    if found.restarted {
        warn!("Watchdog: {} restarted the machine: the boot before this one hung", found.name);
    }
    info!(
        "Watchdog: {}, with timeouts from {} to {} s; Spark starts it",
        found.name, found.min_seconds, found.max_seconds
    );
    let _ = FOUND.set(found);
}

/// The watchdog, if the platform found one.
pub fn found() -> Option<&'static Watchdog> {
    FOUND.get()
}

/// Whether it runs, and its timeout.
pub fn state() -> (bool, u32) {
    *STATE.lock()
}

/// Starts it, or changes its timeout, for `who`; returns the timeout set,
/// or `None` without a watchdog.
pub fn start(seconds: u32, who: &str) -> Option<u32> {
    let found = found()?;
    let (set, changed) = {
        let mut state = STATE.lock();
        let set = Arch::watchdog_start(seconds);
        let changed = *state != (true, set);
        *state = (true, set);
        (set, changed)
    };
    // Not under the lock: a console the log writes to may take locks.
    if changed {
        info!(
            "Watchdog: `{who}` started {}: the machine restarts unless petted every {set} s",
            found.name
        );
    }
    Some(set)
}

/// Pets it; false if it is not running.
pub fn pet() -> bool {
    let state = STATE.lock();
    if state.0 {
        Arch::watchdog_pet();
    }
    state.0
}

/// Stops it, for `who`; false without a watchdog.
pub fn stop(who: &str) -> bool {
    let Some(found) = found() else { return false };
    let was = {
        let mut state = STATE.lock();
        if state.0 {
            Arch::watchdog_stop();
        }
        core::mem::replace(&mut state.0, false)
    };
    if was {
        info!("Watchdog: `{who}` stopped {}", found.name);
    }
    true
}

/// Before the machine ends: when it `halts`, stops the watchdog, so the
/// machine stays halted; otherwise gives a running one its longest
/// timeout, up to [`ENDING_SECONDS`].
pub fn ending(halts: bool) {
    let Some(found) = found() else { return };
    let stopped = {
        let mut state = STATE.lock();
        if !state.0 {
            return;
        }
        if halts {
            Arch::watchdog_stop();
            state.0 = false;
        } else {
            state.1 = Arch::watchdog_start(found.max_seconds.min(ENDING_SECONDS));
        }
        halts
    };
    if stopped {
        info!("Watchdog: stopped, so the halted machine stays halted");
    }
}
