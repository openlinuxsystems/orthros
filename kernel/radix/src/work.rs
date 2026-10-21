// SPDX-FileCopyrightText: 2026 The OrthrOS contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Running a function on the other CPUs.
//!
//! Until Phase 4 brings threads, the other CPUs only sleep in [`idle`].
//! Self-tests that need them post a function here: every other online CPU
//! runs it once from its idle loop, woken by a ping IPI, and the poster
//! waits until all of them are done. One job runs at a time.
//!
//! [`idle`]: crate::idle

use core::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

use radix_hal::{InterruptController, Ipi, MAX_CPUS, Smp};
use radix_sync::SpinLock;

use crate::Arch;

/// A posted function and its number.
#[derive(Clone, Copy)]
struct Job {
    number: u64,
    function: Option<fn()>,
}

/// The current job.
static JOB: SpinLock<Job> = SpinLock::new(Job { number: 0, function: None });
/// Held by the CPU that posts a job until every CPU has run it.
static POSTING: SpinLock<()> = SpinLock::new(());
/// CPUs that have finished the current job.
static DONE: AtomicUsize = AtomicUsize::new(0);
/// The last job each CPU ran.
static SEEN: [AtomicU64; MAX_CPUS] = [const { AtomicU64::new(0) }; MAX_CPUS];

/// The other online CPUs.
fn others(me: usize) -> impl Iterator<Item = usize> {
    (0..Arch::cpu_count())
        .filter(move |&cpu| cpu != me)
        .filter(|&cpu| Arch::cpu_info(cpu).is_some_and(|info| info.online))
}

/// Runs `job` once on every other online CPU and returns when all have
/// finished it.
pub fn on_others(me: usize, job: fn()) {
    let _posting = POSTING.lock();
    let count = others(me).count();
    if count == 0 {
        return;
    }
    DONE.store(0, Ordering::Relaxed);
    {
        let mut slot = JOB.lock();
        slot.number += 1;
        slot.function = Some(job);
    }
    for cpu in others(me) {
        Arch::send_ipi(cpu, Ipi::Ping);
    }
    while DONE.load(Ordering::Acquire) < count {
        core::hint::spin_loop();
    }
}

/// Runs `job` on every online CPU, this one included, and returns when all
/// have finished it.
pub fn on_all(me: usize, job: fn()) {
    let _posting = POSTING.lock();
    let count = others(me).count();
    DONE.store(0, Ordering::Relaxed);
    {
        let mut slot = JOB.lock();
        slot.number += 1;
        slot.function = Some(job);
    }
    for cpu in others(me) {
        Arch::send_ipi(cpu, Ipi::Ping);
    }
    job();
    while DONE.load(Ordering::Acquire) < count {
        core::hint::spin_loop();
    }
}

/// Runs the posted job if CPU `cpu` has not run it yet. The idle loop of
/// the other CPUs calls this after every interrupt.
pub fn run_pending(cpu: usize) {
    let Job { number, function } = *JOB.lock();
    let Some(seen) = SEEN.get(cpu) else { return };
    if seen.swap(number, Ordering::Relaxed) != number
        && let Some(function) = function
    {
        function();
        DONE.fetch_add(1, Ordering::Release);
    }
}
