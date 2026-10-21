// SPDX-FileCopyrightText: 2026 The OrthrOS contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Waiting: the wait table, mutexes, and priority inheritance.
//!
//! One table holds every thread that waits for something, keyed by the
//! address of what it waits for. A thread waits at most once at a time,
//! so the table has room for every thread ([`super::make_room`]) and never
//! allocates. Two kinds of wait use it:
//!
//! - [`wait_until`] sleeps until a condition holds; whoever makes it true
//!   calls [`wake_all`] with the same key. The condition is checked with
//!   the table locked, and the waker takes the lock after changing it, so
//!   no wake-up is lost.
//! - A contended [`radix_sync::Mutex`] queues its waiters here, most
//!   urgent first, through the hooks ([`mutex_wait`], [`mutex_wake`]). The
//!   owner lets go by handing the lock straight to the first waiter.
//!
//! # Priority inheritance
//!
//! A thread waiting for a mutex lends its real-time priority to the owner,
//! and through it along the chain of mutexes the owner itself waits for:
//! a thread's lent priority (`boost`) is the highest among the waiters on
//! the mutexes it holds. When it lets a mutex go, it keeps only what the
//! waiters on its other mutexes lend it, and the new owner inherits the
//! remaining waiters. So a low-priority thread holding a lock that a
//! real-time thread needs runs at that thread's priority until it lets go,
//! and a thread of middle priority cannot keep both waiting (the classic
//! priority inversion). Fair waiters lend nothing: among fair threads
//! the fair class already bounds how long anyone waits for the CPU.
//!
//! Lock order: this table's lock, then the registry's or a thread's, then a
//! run queue's.

use alloc::vec::Vec;
use core::sync::atomic::{AtomicU64, Ordering};

use radix_mm::Kmalloc;
use radix_sync::{RawMutex, SpinLock};

use super::thread::ThreadRef;
use super::{current, registry_find, set_boost};

/// The longest chain of mutex owners a priority is lent along.
const MAX_CHAIN: usize = 16;

/// A waiting thread.
struct Waiter {
    thread: ThreadRef,
    key: usize,
    /// The real-time priority it lends (mutex waits only).
    prio: u8,
    /// Arrival order, for first come first served among equals.
    seq: u64,
    /// The owner's token, for mutex waits; 0 otherwise.
    owner: usize,
}

struct Table {
    waiters: Vec<Waiter, Kmalloc>,
    seq: u64,
}

impl Table {
    fn push(&mut self, waiter: Waiter) {
        if self.waiters.push_within_capacity(waiter).is_err() {
            // Room for every thread is made before the thread exists.
            panic!("Scheduler: the wait table had no room");
        }
    }

    fn remove(&mut self, thread: &ThreadRef, key: usize) -> bool {
        match self.waiters.iter().position(|w| w.key == key && ThreadRef::ptr_eq(&w.thread, thread))
        {
            Some(index) => {
                self.waiters.swap_remove(index);
                true
            }
            None => false,
        }
    }

    /// The most urgent waiter on `key`: highest priority, then the first
    /// to come.
    fn best(&self, key: usize) -> Option<usize> {
        self.waiters
            .iter()
            .enumerate()
            .filter(|(_, w)| w.key == key)
            .max_by_key(|(_, w)| (w.prio, core::cmp::Reverse(w.seq)))
            .map(|(index, _)| index)
    }

    fn has(&self, key: usize) -> bool {
        self.waiters.iter().any(|w| w.key == key)
    }

    /// The priority the waiters on mutexes owned by `owner` lend it.
    fn lent_to(&self, owner: usize) -> u8 {
        self.waiters.iter().filter(|w| w.owner == owner).map(|w| w.prio).max().unwrap_or(0)
    }
}

static TABLE: SpinLock<Option<Table>> = SpinLock::new(None);
static WAKEUPS: AtomicU64 = AtomicU64::new(0);
static HANDOFFS: AtomicU64 = AtomicU64::new(0);
static BOOSTS: AtomicU64 = AtomicU64::new(0);

/// Sets the table up with room for `room` waiters.
pub(super) fn init(room: usize, alloc: Kmalloc) -> bool {
    let mut waiters = Vec::new_in(alloc);
    if waiters.try_reserve_exact(room).is_err() {
        return false;
    }
    *TABLE.lock_irq() = Some(Table { waiters, seq: 0 });
    true
}

/// Gives the table room for `room` waiters; allocates outside the lock.
pub(super) fn grow(room: usize, alloc: Kmalloc) -> bool {
    let mut fresh = Vec::new_in(alloc);
    if fresh.try_reserve_exact(room).is_err() {
        return false;
    }
    let old = {
        let mut table = TABLE.lock_irq();
        let Some(table) = table.as_mut() else { return false };
        if table.waiters.capacity() >= room {
            return true;
        }
        while let Some(waiter) = table.waiters.pop() {
            let _ = fresh.push_within_capacity(waiter);
        }
        core::mem::replace(&mut table.waiters, fresh)
    };
    drop(old);
    true
}

/// Sleeps until `condition` holds; someone who makes it true calls
/// [`wake_all`] with `key`. `condition` runs with the table locked and
/// interrupts off, so it must only read (atomics, typically).
pub fn wait_until(key: usize, reason: &'static str, condition: impl Fn() -> bool) {
    let Some(me) = current() else {
        // Before the scheduler runs there is only the boot thread.
        while !condition() {
            core::hint::spin_loop();
        }
        return;
    };
    loop {
        {
            let mut table = TABLE.lock_irq();
            let Some(table) = table.as_mut() else { return };
            if condition() {
                return;
            }
            table.seq += 1;
            let seq = table.seq;
            table.push(Waiter { thread: me.clone(), key, prio: 0, seq, owner: 0 });
        }
        super::block(reason);
        // Still queued after a spurious wake-up: leave, check, queue again.
        if let Some(table) = TABLE.lock_irq().as_mut() {
            table.remove(&me, key);
        }
    }
}

/// Like [`wait_until`], but gives up at `deadline_ns` (on the kernel's
/// clock). Returns whether the condition held.
pub fn wait_until_or_timeout(
    key: usize,
    reason: &'static str,
    deadline_ns: u64,
    condition: impl Fn() -> bool,
) -> bool {
    let Some(me) = current() else { return condition() };
    loop {
        {
            let mut table = TABLE.lock_irq();
            let Some(table) = table.as_mut() else { return condition() };
            if condition() {
                return true;
            }
            if super::now_ns() >= deadline_ns {
                return false;
            }
            table.seq += 1;
            let seq = table.seq;
            table.push(Waiter { thread: me.clone(), key, prio: 0, seq, owner: 0 });
        }
        super::sleep_until_inner(&me, deadline_ns, reason);
        if let Some(table) = TABLE.lock_irq().as_mut() {
            table.remove(&me, key);
        }
    }
}

/// Wakes every thread waiting on `key`.
pub fn wake_all(key: usize) {
    let mut table = TABLE.lock_irq();
    let Some(table) = table.as_mut() else { return };
    let mut index = 0;
    while index < table.waiters.len() {
        if table.waiters[index].key == key && table.waiters[index].owner == 0 {
            let waiter = table.waiters.swap_remove(index);
            WAKEUPS.fetch_add(1, Ordering::Relaxed);
            super::wake(&waiter.thread);
        } else {
            index += 1;
        }
    }
}

/// The hook behind a contended [`radix_sync::Mutex`]: queues the caller,
/// lends its priority along the chain of owners, and sleeps until the lock
/// is handed to it.
pub(super) fn mutex_wait(mutex: &RawMutex) {
    let Some(me) = current() else { return spin(mutex, super::NO_TOKEN) };
    let token = me.token();
    let key = mutex.key();
    let word = mutex.word();
    {
        let mut table = TABLE.lock_irq();
        let Some(table) = table.as_mut() else {
            drop(table);
            return spin(mutex, token);
        };
        let mut seen = word.load(Ordering::Relaxed);
        let owner = loop {
            let owner = seen & !RawMutex::WAITERS;
            if owner == 0 {
                // Let go meanwhile: take it, keeping the waiters bit for
                // whoever else is queued.
                let bit = if table.has(key) { RawMutex::WAITERS } else { 0 };
                match word.compare_exchange(seen, token | bit, Ordering::Acquire, Ordering::Relaxed)
                {
                    Ok(_) => return,
                    Err(now) => seen = now,
                }
                continue;
            }
            match word.compare_exchange(
                seen,
                seen | RawMutex::WAITERS,
                Ordering::Relaxed,
                Ordering::Relaxed,
            ) {
                Ok(_) => break owner,
                Err(now) => seen = now,
            }
        };
        // With the waiters bit set, the owner can only let go through
        // `mutex_wake`, which needs the table: it is still the owner.
        let prio = super::rt_prio_of(&me);
        table.seq += 1;
        let seq = table.seq;
        table.push(Waiter { thread: me.clone(), key, prio, seq, owner });
        me.blocked_on.store(key, Ordering::Relaxed);
        lend(table, owner, prio);
    }
    while mutex.owner() != token {
        super::block("mutex");
    }
}

/// Lends `prio` to the thread owning `token` and along the chain of owners
/// it waits for.
fn lend(table: &mut Table, mut owner: usize, mut prio: u8) {
    if prio == 0 {
        return;
    }
    for _ in 0..MAX_CHAIN {
        let Some(thread) = registry_find(owner) else { return };
        let lent = table.lent_to(owner).max(prio);
        if !set_boost(&thread, lent) {
            return;
        }
        BOOSTS.fetch_add(1, Ordering::Relaxed);
        let key = thread.blocked_on.load(Ordering::Relaxed);
        if key == 0 {
            return;
        }
        // It waits for another mutex: what it lends there goes up too.
        let effective = super::rt_prio_of(&thread);
        let Some(entry) = table
            .waiters
            .iter_mut()
            .find(|w| w.key == key && ThreadRef::ptr_eq(&w.thread, &thread))
        else {
            return;
        };
        entry.prio = effective;
        owner = entry.owner;
        prio = effective;
    }
}

/// The hook behind letting go of a contended mutex: hands it to the most
/// urgent waiter, which inherits the remaining waiters, and drops what the
/// old owner was lent through it.
pub(super) fn mutex_wake(mutex: &RawMutex) {
    let me = current();
    let key = mutex.key();
    let mut guard = TABLE.lock_irq();
    let Some(table) = guard.as_mut() else {
        mutex.word().store(0, Ordering::Release);
        return;
    };
    match table.best(key) {
        None => mutex.word().store(0, Ordering::Release),
        Some(index) => {
            let next = table.waiters.swap_remove(index);
            let token = next.thread.token();
            let more = table.has(key);
            for waiter in table.waiters.iter_mut().filter(|w| w.key == key) {
                waiter.owner = token;
            }
            next.thread.blocked_on.store(0, Ordering::Relaxed);
            mutex.word().store(token | if more { RawMutex::WAITERS } else { 0 }, Ordering::Release);
            HANDOFFS.fetch_add(1, Ordering::Relaxed);
            let inherited = table.lent_to(token);
            set_boost(&next.thread, inherited);
            super::wake(&next.thread);
        }
    }
    if let Some(me) = me {
        let kept = table.lent_to(me.token());
        set_boost(&me, kept);
    }
}

/// A thread's own priority changed (`sched::set_policy`): what it lends
/// as a mutex waiter changes with it. Call with no locks held.
pub(super) fn priority_changed(thread: &ThreadRef) {
    let key = thread.blocked_on.load(Ordering::Relaxed);
    if key == 0 {
        return;
    }
    let mut table = TABLE.lock_irq();
    let Some(table) = table.as_mut() else { return };
    let prio = super::rt_prio_of(thread);
    let Some(entry) =
        table.waiters.iter_mut().find(|w| w.key == key && ThreadRef::ptr_eq(&w.thread, thread))
    else {
        return;
    };
    entry.prio = prio;
    let owner = entry.owner;
    // Recompute the owner's loan from scratch: it may have gone down.
    if let Some(owner_thread) = registry_find(owner) {
        set_boost(&owner_thread, table.lent_to(owner));
    }
    lend(table, owner, prio);
}

/// Waits for a mutex without the scheduler: before threads run.
fn spin(mutex: &RawMutex, token: usize) {
    while mutex.word().compare_exchange(0, token, Ordering::Acquire, Ordering::Relaxed).is_err() {
        core::hint::spin_loop();
    }
}

/// Counts for the debugger: wake-ups through the table, mutex handoffs,
/// and priority loans.
pub fn stats() -> (u64, u64, u64) {
    (
        WAKEUPS.load(Ordering::Relaxed),
        HANDOFFS.load(Ordering::Relaxed),
        BOOSTS.load(Ordering::Relaxed),
    )
}

/// How many threads wait in the table now, or `None` if it is locked.
pub fn waiting() -> Option<usize> {
    TABLE.try_lock_irq().map(|t| t.as_ref().map_or(0, |t| t.waiters.len()))
}
