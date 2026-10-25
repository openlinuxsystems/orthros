// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! RCU grace periods, quiescent-state based.
//!
//! An RCU read section (`radix_sync::rcu`) only holds off preemption, so a
//! CPU that switches threads, goes idle, or takes a timer tick in code that
//! could have been preempted is not inside any read section that began
//! before: it has passed a *quiescent state*. A grace period starts with
//! the set of CPUs that run threads and ends once each has passed one;
//! then every read section that was running when it started has ended.
//!
//! [`synchronize`] waits for the end of a grace period that starts after
//! it is called: the next one, or the one after if one is already under
//! way (a reader that started after the running one began might still be
//! inside). Grace periods run one at a time and are shared, so many
//! callers waiting at once cost one grace period, not one each.

use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use radix_sched::CpuSet;
use radix_sync::SpinLock;

use super::wait;

struct GracePeriods {
    /// Grace periods completed.
    completed: u64,
    /// Whether one is under way.
    running: bool,
    /// CPUs it still waits for.
    waiting: CpuSet,
    /// The highest grace period someone waits for.
    wanted: u64,
    started_ns: u64,
}

static STATE: SpinLock<GracePeriods> = SpinLock::new(GracePeriods {
    completed: 0,
    running: false,
    waiting: CpuSet::EMPTY,
    wanted: 0,
    started_ns: 0,
});
/// Mirrors of the state, read without the lock.
static COMPLETED: AtomicU64 = AtomicU64::new(0);
static RUNNING: AtomicBool = AtomicBool::new(false);
/// Total time grace periods took, for the self-test's report.
static TOTAL_NS: AtomicU64 = AtomicU64::new(0);

fn key() -> usize {
    &raw const COMPLETED as usize
}

fn start(state: &mut GracePeriods) {
    state.running = true;
    state.waiting = super::online();
    state.started_ns = super::now_ns();
    RUNNING.store(true, Ordering::Release);
}

/// Waits until every RCU read section running when it was called has
/// ended. Sleeps; call it where sleeping is allowed.
pub fn synchronize() {
    if !super::can_sleep() || super::online().len() <= 1 {
        // With one CPU, a thread that can call this is not in a read
        // section, and no other CPU could be. Where sleeping is not
        // allowed there is nothing safe to do but return; the lock
        // checker reports such callers.
        return;
    }
    let target = {
        let mut state = STATE.lock_irq();
        let target = state.completed + if state.running { 2 } else { 1 };
        state.wanted = state.wanted.max(target);
        if !state.running {
            start(&mut state);
        }
        target
    };
    wait::wait_until(key(), "rcu", || COMPLETED.load(Ordering::Acquire) >= target);
}

/// CPU `cpu` is in a quiescent state. With interrupts off, on that CPU.
pub(super) fn quiescent(cpu: usize) {
    if !RUNNING.load(Ordering::Acquire) {
        return;
    }
    let done = {
        let mut state = STATE.lock();
        if !state.running || !state.waiting.contains(cpu) {
            return;
        }
        state.waiting.remove(cpu);
        // CPUs that stopped running threads meanwhile do not count.
        state.waiting = state.waiting.and(&super::online());
        if !state.waiting.is_empty() {
            return;
        }
        state.completed += 1;
        state.running = false;
        RUNNING.store(false, Ordering::Release);
        TOTAL_NS.fetch_add(super::now_ns().saturating_sub(state.started_ns), Ordering::Relaxed);
        COMPLETED.store(state.completed, Ordering::Release);
        if state.wanted > state.completed {
            start(&mut state);
        }
        true
    };
    if done {
        wait::wake_all(key());
    }
}

/// Grace periods completed.
pub fn completed() -> u64 {
    COMPLETED.load(Ordering::Relaxed)
}

/// The average length of a grace period so far, in nanoseconds.
pub fn average_ns() -> u64 {
    TOTAL_NS.load(Ordering::Relaxed) / completed().max(1)
}
