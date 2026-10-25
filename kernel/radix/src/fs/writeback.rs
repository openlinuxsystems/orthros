// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! The writeback thread.
//!
//! Every [`INTERVAL_NS`] it writes each filesystem with changes in memory
//! to its disk (`Vfs::sync_all`), so at most that much work is lost when
//! the power goes, as with Linux's flusher threads. It also drops clean
//! cached pages, which the disk has, when memory runs short: when the
//! frame allocator's free pages fall below 1/64 of memory at a wake-up,
//! and whenever the out-of-memory policy asks ([`reclaim`]) before it
//! stops a resource domain. Dropping pages takes the caches' locks, so it
//! runs here, in a thread that holds none, never inside an allocation.

use core::sync::atomic::{AtomicU64, Ordering};

use radix_klog::{error, warn};
use radix_mm::FRAMES;
use radix_sync::Once;

use crate::sched::{self, Options, Policy, ThreadRef, wait};

/// How often dirty filesystems are written back.
pub const INTERVAL_NS: u64 = 5_000_000_000;

/// The fewest pages a reclaim drops, when the caches hold that many.
const RECLAIM_MIN: usize = 256;

/// Reclaims asked for, and done: the thread works while they differ.
static ASKED: AtomicU64 = AtomicU64::new(0);
static DONE: AtomicU64 = AtomicU64::new(0);
/// Pages dropped so far.
static DROPPED: AtomicU64 = AtomicU64::new(0);
/// Write-backs that failed, for the log.
static FAILED: AtomicU64 = AtomicU64::new(0);

static THREAD: Once<ThreadRef> = Once::new();

fn asked_key() -> usize {
    &raw const ASKED as usize
}

fn done_key() -> usize {
    &raw const DONE as usize
}

/// Starts the thread.
pub fn start() {
    match sched::spawn("writeback", run, 0, Options::new().policy(Policy::Fair { nice: 0 })) {
        Ok(thread) => {
            let _ = THREAD.set(thread);
        }
        Err(err) => error!("Files: could not start the writeback thread: {err}"),
    }
}

fn run(_: usize) {
    let mut next_sync = sched::now_ns().saturating_add(INTERVAL_NS);
    loop {
        wait::wait_until_or_timeout(asked_key(), "writeback", next_sync, || {
            ASKED.load(Ordering::Acquire) != DONE.load(Ordering::Acquire)
        });
        let asked = ASKED.load(Ordering::Acquire);
        let Some(vfs) = super::vfs() else {
            // No files, for now or for good: an ask is answered with
            // nothing dropped, and the next look waits its turn.
            DONE.store(asked, Ordering::Release);
            wait::wake_all(done_key());
            if sched::now_ns() >= next_sync {
                next_sync = sched::now_ns().saturating_add(INTERVAL_NS);
            }
            continue;
        };
        let short = FRAMES.free_pages() < FRAMES.managed_pages() / 64;
        if asked != DONE.load(Ordering::Acquire) || short {
            let cached = radix_block::mem::pages() as usize;
            let dropped = vfs.shrink((cached / 4).max(RECLAIM_MIN));
            DROPPED.fetch_add(dropped as u64, Ordering::AcqRel);
            DONE.store(asked, Ordering::Release);
            wait::wake_all(done_key());
        }
        if sched::now_ns() >= next_sync {
            if let Err(err) = vfs.sync_all() {
                // Said once, then counted: a failing disk would fill the
                // log every five seconds.
                if FAILED.fetch_add(1, Ordering::Relaxed) == 0 {
                    warn!("Files: writing back to a disk failed: {err}; changes stay in memory");
                }
            }
            next_sync = sched::now_ns().saturating_add(INTERVAL_NS);
        }
    }
}

/// Asks the thread to drop clean cached pages and waits for it, up to
/// `timeout_ns`. Returns whether any went. The writeback thread itself
/// gets `false` at once: it cannot wait for itself.
pub fn reclaim(timeout_ns: u64) -> bool {
    let Some(thread) = THREAD.get() else { return false };
    if sched::current().is_some_and(|me| ThreadRef::ptr_eq(&me, thread)) {
        return false;
    }
    let before = DROPPED.load(Ordering::Acquire);
    let ticket = ASKED.fetch_add(1, Ordering::AcqRel) + 1;
    wait::wake_all(asked_key());
    let deadline = sched::now_ns().saturating_add(timeout_ns);
    wait::wait_until_or_timeout(done_key(), "reclaim", deadline, || {
        DONE.load(Ordering::Acquire) >= ticket
    });
    DROPPED.load(Ordering::Acquire) > before
}

/// Pages dropped and failed write-backs so far, for `kdb`.
pub fn stats() -> (u64, u64) {
    (DROPPED.load(Ordering::Relaxed), FAILED.load(Ordering::Relaxed))
}
