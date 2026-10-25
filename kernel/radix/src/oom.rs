// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! The out-of-memory policy (spec 4.1).
//!
//! When the frame allocator cannot find a block of up to 2^3 pages for an
//! allocation that may sleep, even after the heap has given back what it
//! caches, it asks this module (`Frames::set_oom`). The policy stops whole
//! resource domains (`domains`), never single threads, and says so in the
//! log every time:
//!
//! 0. It asks the writeback thread to drop clean pages of the page cache,
//!    which their disks hold (`fs::writeback`), and has the allocation
//!    tried again if any went.
//! 1. It picks the domain to stop among those with threads of their own:
//!    the lowest out-of-memory class first (apps, then services), and
//!    within a class the one using the most memory, with the domains below
//!    it, which stop with it. A domain whose threads are all in the
//!    domains below it, such as `services`, is not picked itself: its
//!    children are, one at a time. Domains of the `system` class, which
//!    hold the trusted computing base, are never stopped; if only they are
//!    left, the allocation fails and its caller gets an error. The kernel
//!    never panics only because memory ran out.
//! 2. It asks every thread of that domain and the domains below it to stop
//!    (`Thread::request_stop`), wakes them so they notice, and logs a
//!    warning naming the domain, its class and its usage.
//! 3. If the thread that asked for memory is one of them, its allocation
//!    fails at once, so it can unwind. Otherwise the policy waits up to a
//!    second for the domain's threads to exit, then the allocator tries
//!    again.
//! 4. A domain with a supervisor in the kernel is restarted once its
//!    threads are gone: the supervisor function runs on the `supervisor`
//!    thread. The domains of the services Spark supervises have none
//!    (Phase 9): their processes end as if killed by SIGKILL, the domain
//!    takes threads again once its last is gone, and Spark, seeing its
//!    service end, starts it again.
//!
//! One thread runs the policy at a time. A thread that comes in while
//! another stop is under way, or whose own domain is being stopped, does
//! not stop a second domain for the same shortage.

use core::sync::atomic::{AtomicU64, Ordering};

use radix_klog::{error, info, warn};
use radix_mm::{FRAMES, OOM_MAX_ORDER};
use radix_sync::{Mutex, SpinLock};

use crate::domains::{self, DomainRef, OomClass};
use crate::sched::{self, Options, Policy, wait};

/// How long the policy waits for the page cache to give back pages.
const RECLAIM_WAIT_NS: u64 = 200_000_000;
/// How long the policy waits for a stopped domain's threads to exit.
const STOP_WAIT_NS: u64 = 1_000_000_000;
/// Restarts that can wait for the supervisor thread.
const PENDING: usize = 8;

/// Held while the policy runs.
static POLICY: Mutex<()> = Mutex::new(());
/// Domains stopped so far; a waiter that sees it change retries first.
static STOPS: AtomicU64 = AtomicU64::new(0);
/// Times the policy found nothing it may stop.
static GAVE_UP: AtomicU64 = AtomicU64::new(0);
static RESTARTS: AtomicU64 = AtomicU64::new(0);
/// Stopped domains waiting to be restarted.
static PENDING_RESTARTS: SpinLock<[Option<DomainRef>; PENDING]> =
    SpinLock::new([const { None }; PENDING]);

fn restart_key() -> usize {
    &raw const RESTARTS as usize
}

/// Registers the policy with the frame allocator.
pub fn init() {
    FRAMES.set_oom(on_out_of_memory);
}

/// Starts the thread that restarts supervised domains.
pub fn start() {
    let made = sched::spawn(
        "supervisor",
        supervisor_main,
        0,
        Options::new().policy(Policy::Fair { nice: -5 }),
    );
    if let Err(err) = made {
        error!("Out of memory: could not start the supervisor thread: {err}");
    }
}

/// How the policy has acted so far: domains stopped, domains restarted,
/// and times it found nothing to stop.
pub fn stats() -> (u64, u64, u64) {
    (
        STOPS.load(Ordering::Relaxed),
        RESTARTS.load(Ordering::Relaxed),
        GAVE_UP.load(Ordering::Relaxed),
    )
}

/// The frame allocator's hook. Returns whether the allocation should be
/// tried again.
fn on_out_of_memory(order: u32) -> bool {
    debug_assert!(order <= OOM_MAX_ORDER);
    if !sched::can_sleep() {
        return false;
    }
    let Some(me) = sched::current() else { return false };
    if me.should_stop() || me.domain().is_stopping() {
        // Being stopped already: unwind instead of stopping another domain.
        return false;
    }
    // Clean cached pages can be read again from their disks: they go
    // before any domain is stopped.
    if crate::fs::writeback::reclaim(RECLAIM_WAIT_NS) {
        return true;
    }
    let seen = STOPS.load(Ordering::Acquire);
    let _policy = POLICY.lock();
    if STOPS.load(Ordering::Acquire) != seen {
        // Another thread stopped a domain while this one waited: try again.
        return true;
    }
    let Some(victim) = choose() else {
        GAVE_UP.fetch_add(1, Ordering::Relaxed);
        error!(
            "Out of memory: no app or service domain is left to stop; an allocation of {} pages \
             by `{}` fails",
            1u64 << order,
            me.name()
        );
        return false;
    };
    stop(&victim);
    if me.domain().is_within(&victim) {
        return false;
    }
    let deadline = sched::now_ns().saturating_add(STOP_WAIT_NS);
    let emptied =
        wait::wait_until_or_timeout(victim.key(), "oom", deadline, || victim.threads() == 0);
    if !emptied {
        warn!(
            "Out of memory: domain `{}` still has {} threads a second after it was stopped",
            victim.name(),
            victim.threads()
        );
    }
    true
}

/// The domain to stop: the lowest class below `system`, then the most
/// memory used. Domains being stopped already are left out, and so are
/// those without threads of their own, as stopping one frees nothing
/// itself: a parent's usage counts its children's, and a domain whose
/// threads have all gone, which takes threads again (`Domain::thread_left`),
/// would be picked again and again.
fn choose() -> Option<DomainRef> {
    let mut best: Option<DomainRef> = None;
    domains::for_each(|domain| {
        if domain.class() == OomClass::System || domain.is_stopping() {
            return;
        }
        if domain.threads() == 0 {
            return;
        }
        let better = match &best {
            None => true,
            Some(best) => {
                (domain.class(), core::cmp::Reverse(domain.usage_pages()))
                    < (best.class(), core::cmp::Reverse(best.usage_pages()))
            }
        };
        if better {
            best = Some(domain.clone());
        }
    });
    best
}

/// Stops the threads of `victim` and the domains below it: marks the
/// domains stopping (for the out-of-memory policy if `oom`), asks the
/// threads to stop and wakes them. Returns how many there were. Also for
/// radix-resource-domain(2)'s `stop`.
pub fn stop_threads(victim: &DomainRef, oom: bool) -> u32 {
    domains::for_each(|domain| {
        if domain.is_within(victim) {
            domain.set_stopping(true, oom);
        }
    });
    let mut stopped = 0;
    sched::for_each_thread(|thread| {
        if thread.domain().is_within(victim) {
            thread.request_stop();
            sched::wake(thread);
            stopped += 1;
        }
    });
    stopped
}

/// Stops `victim` and the domains below it, and queues a restart if it has
/// a supervisor in the kernel.
fn stop(victim: &DomainRef) {
    STOPS.fetch_add(1, Ordering::AcqRel);
    let stopped = stop_threads(victim, true);
    warn!(
        "Out of memory: stopped domain `{}` ({} class, {} pages in use, {} thread{}){}",
        victim.name(),
        victim.class().name(),
        victim.usage_pages(),
        stopped,
        if stopped == 1 { "" } else { "s" },
        if victim.supervisor().is_some() { "; its supervisor restarts it" } else { "" }
    );
    if victim.supervisor().is_some() {
        let queued = {
            let mut pending = PENDING_RESTARTS.lock_irq();
            match pending.iter_mut().find(|slot| slot.is_none()) {
                Some(slot) => {
                    *slot = Some(victim.clone());
                    true
                }
                None => false,
            }
        };
        if queued {
            wait::wake_all(restart_key());
        } else {
            error!("Out of memory: too many restarts waiting; `{}` stays stopped", victim.name());
        }
    }
}

/// Takes the next restart off the list.
fn next_restart() -> Option<DomainRef> {
    let mut pending = PENDING_RESTARTS.lock_irq();
    pending.iter_mut().find_map(Option::take)
}

fn has_restart() -> bool {
    PENDING_RESTARTS.lock_irq().iter().any(Option::is_some)
}

/// The supervisor thread: restarts stopped domains that have a supervisor,
/// once their threads are gone.
fn supervisor_main(_: usize) {
    loop {
        wait::wait_until(restart_key(), "restart", has_restart);
        while let Some(domain) = next_restart() {
            let deadline = sched::now_ns().saturating_add(5 * STOP_WAIT_NS);
            let emptied = wait::wait_until_or_timeout(domain.key(), "restart", deadline, || {
                domain.threads() == 0
            });
            if !emptied {
                error!(
                    "Out of memory: domain `{}` did not stop; it is not restarted",
                    domain.name()
                );
                continue;
            }
            domains::for_each(|below| {
                if below.is_within(&domain) {
                    below.set_stopping(false, false);
                }
            });
            if let Some(restart) = domain.supervisor() {
                RESTARTS.fetch_add(1, Ordering::Relaxed);
                info!("Out of memory: restarting domain `{}`", domain.name());
                restart(&domain);
            }
        }
    }
}
