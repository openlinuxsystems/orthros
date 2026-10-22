// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Interval timers (Phase 8, radix-process(2)'s `set_timer`): the three
//! of POSIX's `setitimer` each process has, real time (SIGALRM), user time
//! (SIGVTALRM) and profiling time (SIGPROF).
//!
//! A timer holds when it expires, on the kernel's clock for the real one
//! and on its process's CPU time for the others, and its interval. One
//! kernel thread runs them all: it sleeps until the next real timer is
//! due, or for [`CPU_TICK_NS`] while a CPU-time timer is armed, since CPU
//! time is only counted at ticks; then it sends the signal of each timer
//! that has expired and sets it again by its interval (an expiry missed
//! while the signal was pending counts once). The processes with an armed
//! timer are listed in [`ARMED`], by weak references, so a process that
//! has gone drops out of the list.

use alloc::sync::{Arc, Weak};
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use radix_abi::errors::Kernel;
use radix_abi::process::TimerKind;
use radix_abi::signal::{SIGALRM, SIGPROF, SIGVTALRM};
use radix_klog::error;
use radix_mm::Kmalloc;
use radix_posix::signal::{Info, SI_TIMER};
use radix_sync::Mutex;

use super::{cpu_ns, kmalloc, signal};
use crate::sched::{self, Options, Policy, wait};
use crate::user::Result;
use crate::user::process::{ProcessObject, ProcessRef};

/// How often CPU-time timers are looked at while one is armed.
const CPU_TICK_NS: u64 = 10_000_000;

/// A timer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Timer {
    /// When it expires (0: never): on the kernel's clock for the real
    /// timer, on its process's CPU time for the others.
    pub expires: u64,
    /// How long after expiring it expires again (0: it does not).
    pub interval: u64,
}

impl Timer {
    /// A timer that is not set.
    pub const OFF: Timer = Timer { expires: 0, interval: 0 };
}

/// The processes with a timer set, which the thread looks at.
static ARMED: Mutex<Option<Vec<Weak<ProcessObject, Kmalloc>, Kmalloc>>> = Mutex::new(None);

/// Moves whenever a timer is set, so that the thread looks again.
static CHANGES: AtomicU64 = AtomicU64::new(0);

/// Whether the thread has been started.
static STARTED: AtomicBool = AtomicBool::new(false);

fn changes_key() -> usize {
    &CHANGES as *const AtomicU64 as usize
}

/// The signal timer `index` sends.
const SIGNALS: [u32; 3] = [SIGALRM, SIGVTALRM, SIGPROF];

/// What timer `index` of `process` counts now: the kernel's clock, or the
/// process's CPU time.
fn now_for(process: &ProcessObject, index: usize) -> u64 {
    if index == 0 { sched::now_ns() } else { cpu_ns(process) }
}

/// Sets `process`'s timer `kind` to expire in `value_ns` (0: never), then
/// every `interval_ns`, and returns what was left of it and its interval.
pub fn set(
    process: &ProcessRef,
    kind: TimerKind,
    value_ns: u64,
    interval_ns: u64,
) -> Result<(u64, u64)> {
    let index = kind as usize;
    if index >= SIGNALS.len() {
        return Err(Kernel::InvalidArgument.status(0));
    }
    if value_ns != 0 {
        start()?;
        register(process)?;
    }
    let now = now_for(process, index);
    let old = {
        let mut posix = process.posix.lock();
        let timer = &mut posix.timers[index];
        let old = *timer;
        *timer = if value_ns == 0 {
            Timer::OFF
        } else {
            Timer { expires: now.saturating_add(value_ns).max(1), interval: interval_ns }
        };
        old
    };
    CHANGES.fetch_add(1, Ordering::AcqRel);
    wait::wake_all(changes_key());
    // A timer due now is still a little way off.
    let left = if old.expires == 0 { 0 } else { old.expires.saturating_sub(now).max(1) };
    Ok((left, old.interval))
}

/// Puts `process` in [`ARMED`], unless it is there.
fn register(process: &ProcessRef) -> Result<()> {
    let mut armed = ARMED.lock();
    let list = armed.get_or_insert_with(|| Vec::new_in(kmalloc()));
    if list.iter().any(|weak| core::ptr::eq(weak.as_ptr(), Arc::as_ptr(process))) {
        return Ok(());
    }
    list.retain(|weak| weak.strong_count() > 0);
    list.try_reserve(1).map_err(|_| Kernel::NoMemory.status(0))?;
    // Room was reserved above.
    let _ = list.push_within_capacity(Arc::downgrade(process));
    Ok(())
}

/// Starts the timer thread, once.
fn start() -> Result<()> {
    if STARTED.swap(true, Ordering::AcqRel) {
        return Ok(());
    }
    match sched::spawn("timers", run, 0, Options::new().policy(Policy::Fair { nice: -5 })) {
        Ok(_) => Ok(()),
        Err(err) => {
            STARTED.store(false, Ordering::Release);
            error!("User: could not start the timer thread: {err}");
            Err(Kernel::NoMemory.status(0))
        }
    }
}

/// The timer thread.
fn run(_: usize) {
    loop {
        let seen = CHANGES.load(Ordering::Acquire);
        let deadline = expire_due();
        let changed = || CHANGES.load(Ordering::Acquire) != seen;
        match deadline {
            Some(deadline) => {
                wait::wait_until_or_timeout(changes_key(), "timers", deadline, changed);
            }
            None => wait::wait_until(changes_key(), "timers", changed),
        }
    }
}

/// Sends the signal of every timer that has expired, sets it again, and
/// returns when the thread must look next (`None`: when a timer is set).
fn expire_due() -> Option<u64> {
    let mut processes = Vec::new_in(kmalloc());
    {
        let mut armed = ARMED.lock();
        let list = armed.as_mut()?;
        list.retain(|weak| weak.strong_count() > 0);
        if processes.try_reserve(list.len()).is_err() {
            // Look again soon.
            return Some(sched::now_ns().saturating_add(CPU_TICK_NS));
        }
        for process in list.iter().filter_map(Weak::upgrade) {
            // Room was reserved above.
            let _ = processes.push_within_capacity(process);
        }
    }
    let mut next = u64::MAX;
    let mut keep_looking = false;
    for process in &processes {
        let real = sched::now_ns();
        let cpu = cpu_ns(process);
        let mut fired = [false; 3];
        let mut armed = false;
        {
            let mut posix = process.posix.lock();
            for (index, timer) in posix.timers.iter_mut().enumerate() {
                if timer.expires == 0 {
                    continue;
                }
                let now = if index == 0 { real } else { cpu };
                if now >= timer.expires {
                    fired[index] = true;
                    *timer = match (now - timer.expires).checked_div(timer.interval) {
                        None => Timer::OFF,
                        Some(missed) => {
                            // Missed expiries count once.
                            let late = missed + 1;
                            let expires =
                                timer.expires.saturating_add(late.saturating_mul(timer.interval));
                            Timer { expires, interval: timer.interval }
                        }
                    };
                }
                if timer.expires != 0 {
                    armed = true;
                    if index == 0 {
                        next = next.min(timer.expires);
                    } else {
                        keep_looking = true;
                    }
                }
            }
        }
        for (index, _) in fired.iter().enumerate().filter(|(_, fired)| **fired) {
            let info = Info { code: SI_TIMER, pid: 0, status: 0 };
            signal::send_process(process, SIGNALS[index], info);
        }
        if !armed {
            unregister(process);
        }
    }
    if keep_looking {
        next = next.min(sched::now_ns().saturating_add(CPU_TICK_NS));
    }
    (next != u64::MAX).then_some(next)
}

/// Takes `process` out of [`ARMED`], unless a timer of its was set again
/// meanwhile.
fn unregister(process: &ProcessRef) {
    let mut armed = ARMED.lock();
    let Some(list) = armed.as_mut() else { return };
    if process.posix.lock().timers.iter().any(|timer| timer.expires != 0) {
        return;
    }
    list.retain(|weak| !core::ptr::eq(weak.as_ptr(), Arc::as_ptr(process)));
}
