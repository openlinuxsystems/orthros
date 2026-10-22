// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Running a function on the other CPUs.
//!
//! Every CPU has a worker thread, `work/N`, pinned to it in the fair class
//! at nice -20, so a posted job runs there promptly but shares the CPU
//! with what already runs. Self-tests that need the other CPUs post a
//! function here: every other CPU's worker runs it once, and the poster
//! sleeps until all of them are done. One job runs at a time.
//!
//! The workers are not real-time: jobs that take a mutex in a loop would
//! otherwise starve a fair thread waiting for it, since real-time waiters
//! are handed a mutex first.

use core::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

use radix_hal::{MAX_CPUS, Smp};
use radix_klog::error;
use radix_sched::CpuSet;
use radix_sync::{Mutex, SpinLock};

use crate::Arch;
use crate::sched::{self, Options, Policy, wait};

/// The workers' nice level: the largest share of the fair class.
const NICE: i8 = -20;

/// A posted function, its number and the CPUs that run it.
#[derive(Clone, Copy)]
struct Job {
    number: u64,
    function: Option<fn()>,
    cpus: CpuSet,
}

/// The current job.
static JOB: SpinLock<Job> = SpinLock::new(Job { number: 0, function: None, cpus: CpuSet::EMPTY });
/// Mirrors the job's number, for the workers' wait condition.
static NUMBER: AtomicU64 = AtomicU64::new(0);
/// Held by the thread that posts a job until every CPU has run it.
static POSTING: Mutex<()> = Mutex::new(());
/// CPUs that have finished the current job.
static DONE: AtomicUsize = AtomicUsize::new(0);
/// CPUs that have a worker.
static WORKERS: SpinLock<CpuSet> = SpinLock::new(CpuSet::EMPTY);

fn job_key() -> usize {
    &NUMBER as *const AtomicU64 as usize
}

fn done_key() -> usize {
    &DONE as *const AtomicUsize as usize
}

/// Starts a worker on every CPU that runs threads.
pub fn start() {
    for cpu in sched::online().iter() {
        let options = Options::new().pinned(cpu).policy(Policy::Fair { nice: NICE });
        let mut name = radix_klog::TextBuf::<16>::new();
        let _ = core::fmt::Write::write_fmt(&mut name, format_args!("work/{cpu}"));
        match sched::spawn(name.as_str(), worker_main, cpu, options) {
            Ok(_) => WORKERS.lock_irq().insert(cpu),
            Err(err) => error!("Work: no worker for CPU {cpu}: {err}"),
        }
    }
}

fn worker_main(cpu: usize) {
    let mut seen = 0;
    loop {
        wait::wait_until(job_key(), "work", || NUMBER.load(Ordering::Acquire) != seen);
        let job = *JOB.lock_irq();
        seen = job.number;
        if let (true, Some(function)) = (job.cpus.contains(cpu), job.function) {
            function();
            DONE.fetch_add(1, Ordering::AcqRel);
            wait::wake_all(done_key());
        }
    }
}

/// The CPUs with a worker, but `me`.
fn others(me: usize) -> CpuSet {
    let mut cpus = WORKERS.lock_irq().and(&CpuSet::first(Arch::cpu_count().min(MAX_CPUS)));
    cpus.remove(me);
    cpus
}

/// Posts `job` for `cpus` and returns how many will run it.
fn post(job: fn(), cpus: CpuSet) -> usize {
    DONE.store(0, Ordering::Relaxed);
    {
        let mut slot = JOB.lock_irq();
        slot.number += 1;
        slot.function = Some(job);
        slot.cpus = cpus;
        NUMBER.store(slot.number, Ordering::Release);
    }
    wait::wake_all(job_key());
    cpus.len()
}

fn wait_done(count: usize) {
    wait::wait_until(done_key(), "work done", || DONE.load(Ordering::Acquire) >= count);
}

/// Runs `job` once on every other CPU and returns when all have finished
/// it. `me` is the CPU of the caller, which should be pinned there.
pub fn on_others(me: usize, job: fn()) {
    let _posting = POSTING.lock();
    let count = post(job, others(me));
    wait_done(count);
}

/// Runs `job` on every CPU, the caller's (`me`) included, and returns when
/// all have finished it.
pub fn on_all(me: usize, job: fn()) {
    let _posting = POSTING.lock();
    let count = post(job, others(me));
    job();
    wait_done(count);
}
