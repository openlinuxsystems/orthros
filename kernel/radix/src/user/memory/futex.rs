// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Futexes (Phase 8, radix-address-space(2)): threads waiting on 32-bit
//! words of an address space, what a C library's locks and condition
//! variables sleep on.
//!
//! A word is named by a [`Key`]: in memory only its address space maps, by
//! the space and its address; in memory several spaces may map, each at
//! its own address (a shared mapping, a frame, a ring), by what they map
//! and the word's place in it, so that a wake in one process reaches the
//! waiters of another (`SpaceObject::futex_key`). Its waiters are kept in
//! one of [`BUCKETS`] lists, chosen by hashing the two, in the order they
//! came. `futex_wait` checks the word with the bucket locked,
//! so a `futex_wake` that comes after the word changed (it takes the same
//! lock) always finds the waiter: no wake-up is lost between the check and
//! the sleep. A woken waiter is taken off the list by its waker and told
//! so through its own flag, both with the list locked; the waker wakes
//! its thread once the list is unlocked, so that the thread, which may
//! run at once on the waker's CPU, does not find the list still locked.
//! One that stops waiting for another reason (a timeout, a signal) takes
//! itself off, unless a waker did first, which then counts as its
//! wake-up. A waiter without a deadline sleeps on its own, outside the
//! scheduler's wait table, as nothing but its waker, a signal or a stop
//! wakes it ([`sched::wake`]). The TLA+ model `kernel/radix/tla/Futex.tla`
//! checks that no wake-up is lost this way.
//!
//! A waiter on shared memory holds what it waits on, so that its key
//! names nothing else while it waits, even if every space unmaps it.

use alloc::vec::Vec;
use core::sync::atomic::Ordering;

use radix_abi::errors::Kernel;
use radix_hal::UserMode;
use radix_mm::Kmalloc;
use radix_sync::Mutex;

use super::anon::SharedRef;
use super::{FrameRef, RingRef, SpaceObject, kmalloc};
use crate::Arch;
use crate::sched::{self, ThreadRef, wait};
use crate::user::posix::signal::{Restart, interrupted};
use crate::user::{Result, UserRef, error};

/// How many lists waiters are spread over.
const BUCKETS: usize = 64;

/// How many waiters a wake takes off their list at a time, before it
/// wakes them with the list unlocked.
const WAKE_BATCH: usize = 8;

/// What names a word.
pub struct Key {
    /// What it is in, by address: its address space, or the memory spaces
    /// share.
    pub(super) owner: usize,
    /// Its address in the space, or its place in the shared memory.
    pub(super) offset: u64,
    /// The shared memory, which a waiter holds.
    pub(super) hold: Option<Held>,
}

/// Memory several spaces may map, held by a waiter on a word of it: a
/// pointer each, so that a key stays small enough to move cheaply.
#[expect(dead_code, reason = "held only to keep the memory alive")]
pub enum Held {
    /// A shared mapping's pages.
    Pages(SharedRef),
    /// A frame.
    Frame(FrameRef),
    /// A ring's pages.
    Ring(RingRef),
}

/// A thread waiting on a word.
struct Waiter {
    owner: usize,
    offset: u64,
    /// Held while it waits ([`Key::hold`]).
    #[expect(dead_code, reason = "held only to keep the memory alive")]
    hold: Option<Held>,
    thread: ThreadRef,
    /// Its program side, whose flag the waker sets.
    user: UserRef,
}

/// The lists of waiters, each made when first needed.
static LISTS: [Mutex<Option<Vec<Waiter, Kmalloc>>>; BUCKETS] =
    [const { Mutex::new(None) }; BUCKETS];

/// The list for the word `key` names.
fn list(key: &Key) -> &'static Mutex<Option<Vec<Waiter, Kmalloc>>> {
    let hash = (key.owner as u64 ^ key.offset.rotate_left(17)).wrapping_mul(0x9e37_79b9_7f4a_7c15);
    &LISTS[(hash >> 58) as usize % BUCKETS]
}

/// What a waiter with a deadline waits on in the scheduler's table: its
/// own flag.
fn key_of(user: &UserRef) -> usize {
    &raw const user.futex_woken as usize
}

/// Waits while the word at `address` of `space`, the running thread's,
/// holds `expected`, until a wake, `deadline_ns` (on the kernel's clock)
/// or a signal. `user` is the running thread's program side.
pub fn wait(
    space: &SpaceObject,
    address: u64,
    expected: u32,
    deadline_ns: Option<u64>,
    user: &UserRef,
) -> Result<()> {
    if !address.is_multiple_of(4) {
        return Err(Kernel::InvalidArgument.status(0));
    }
    let me = sched::current().ok_or(error(Kernel::Gone))?;
    let key = space.futex_key(address);
    let bucket = list(&key);
    {
        let mut waiters = bucket.lock();
        // Read with the list locked: a waker changes the word before it
        // takes the lock.
        let mut word = [0u8; 4];
        if !Arch::copy_from_user(&mut word, address) {
            return Err(error(Kernel::BadAddress));
        }
        if u32::from_le_bytes(word) != expected {
            return Err(error(Kernel::WouldBlock));
        }
        let waiters = waiters.get_or_insert_with(|| Vec::new_in(kmalloc()));
        waiters.try_reserve(1).map_err(|_| error(Kernel::NoMemory))?;
        user.futex_woken.store(false, Ordering::Relaxed);
        // Room was reserved above.
        let waiter = Waiter {
            owner: key.owner,
            offset: key.offset,
            hold: key.hold,
            thread: me.clone(),
            user: user.clone(),
        };
        let _ = waiters.push_within_capacity(waiter);
    }
    let done = || user.futex_woken.load(Ordering::Acquire) || sched::interrupted();
    let in_time = match deadline_ns {
        None => {
            // A wake that comes before the thread sleeps makes the sleep
            // return at once.
            while !done() {
                sched::block("futex");
            }
            true
        }
        Some(deadline) => wait::wait_until_or_timeout(key_of(user), "futex", deadline, done),
    };
    if user.futex_woken.load(Ordering::Acquire) {
        return Ok(());
    }
    // Off the list, unless a waker took it off just now: then the wake-up
    // is this wait's. What it held goes with the list unlocked.
    let mine = {
        let mut waiters = bucket.lock();
        let Some(waiters) = waiters.as_mut() else { return Ok(()) };
        let Some(at) = waiters.iter().position(|w| ThreadRef::ptr_eq(&w.thread, &me)) else {
            return Ok(());
        };
        waiters.remove(at)
    };
    drop(mine);
    if !in_time {
        return Err(error(Kernel::TimedOut));
    }
    // As Linux's: a wait with a deadline runs again only if no handler
    // ran, so that a sleep a handler cuts short says so; one without runs
    // again unless the handler lacks SA_RESTART.
    let restart = if deadline_ns.is_some() { Restart::NoHandler } else { Restart::Always };
    Err(interrupted(restart))
}

/// Wakes up to `count` of the threads waiting on the word at `address` of
/// `space`, first come first woken; returns how many.
pub fn wake(space: &SpaceObject, address: u64, count: u32) -> Result<u32> {
    if !address.is_multiple_of(4) {
        return Err(Kernel::InvalidArgument.status(0));
    }
    let key = space.futex_key(address);
    let bucket = list(&key);
    let mut woken = 0;
    loop {
        let mut batch: [Option<ThreadRef>; WAKE_BATCH] = [const { None }; WAKE_BATCH];
        let mut taken = 0;
        let more = {
            let mut waiters = bucket.lock();
            let Some(waiters) = waiters.as_mut() else { return Ok(0) };
            let mut index = 0;
            while index < waiters.len() && woken < count && taken < WAKE_BATCH {
                let waiter = &waiters[index];
                if waiter.owner != key.owner || waiter.offset != key.offset {
                    index += 1;
                    continue;
                }
                let waiter = waiters.remove(index);
                waiter.user.futex_woken.store(true, Ordering::Release);
                // What the wait held goes here, with the list locked, and
                // is never the last hold: a waiter on the same word holds
                // the same memory as `key`, which is held until this
                // returns, so no object is freed under the lock.
                batch[taken] = Some(waiter.thread);
                taken += 1;
                woken += 1;
            }
            taken == WAKE_BATCH && woken < count
        };
        for thread in batch.iter().flatten() {
            sched::wake(thread);
        }
        if !more {
            return Ok(woken);
        }
    }
}
