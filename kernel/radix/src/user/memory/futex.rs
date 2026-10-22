// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Futexes (Phase 8, radix-address-space(2)): threads waiting on 32-bit
//! words of an address space, what a C library's locks and condition
//! variables sleep on.
//!
//! A word is named by its address space and its address. Its waiters are
//! kept in one of [`BUCKETS`] lists, chosen by hashing the two, in the
//! order they came. `futex_wait` checks the word with the bucket locked,
//! so a `futex_wake` that comes after the word changed (it takes the same
//! lock) always finds the waiter: no wake-up is lost between the check and
//! the sleep. A woken waiter is taken off the list by its waker and told
//! so through its own flag; one that stops waiting for another reason (a
//! timeout, a signal) takes itself off, unless a waker did first, which
//! then counts as its wake-up.
//!
//! Words of memory several processes share (`MAP_SHARED`) are named by
//! each one's own space, so one process's wake does not reach another's
//! waiters on the same page: shared futexes are not supported yet
//! (docs/design/posix.md lists it).

use alloc::vec::Vec;
use core::sync::atomic::Ordering;

use radix_abi::errors::Kernel;
use radix_hal::UserMode;
use radix_mm::Kmalloc;
use radix_sync::Mutex;

use super::{SpaceObject, kmalloc};
use crate::Arch;
use crate::sched::{self, ThreadRef, wait};
use crate::user::{Result, UserRef, error};

/// How many lists waiters are spread over.
const BUCKETS: usize = 64;

/// A thread waiting on a word.
struct Waiter {
    /// The address space, by address.
    space: usize,
    address: u64,
    thread: ThreadRef,
    /// Its program side, whose flag the waker sets.
    user: UserRef,
}

/// The lists of waiters, each made when first needed.
static LISTS: [Mutex<Option<Vec<Waiter, Kmalloc>>>; BUCKETS] =
    [const { Mutex::new(None) }; BUCKETS];

/// The list for the word at `address` of the space at `space`.
fn list(space: usize, address: u64) -> &'static Mutex<Option<Vec<Waiter, Kmalloc>>> {
    let hash = (space as u64 ^ address.rotate_left(17)).wrapping_mul(0x9e37_79b9_7f4a_7c15);
    &LISTS[(hash >> 58) as usize % BUCKETS]
}

/// What a waiter waits on: its own flag.
fn key_of(user: &UserRef) -> usize {
    &user.futex_woken as *const _ as usize
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
    let key = space as *const SpaceObject as usize;
    let bucket = list(key, address);
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
        let waiter = Waiter { space: key, address, thread: me.clone(), user: user.clone() };
        let _ = waiters.push_within_capacity(waiter);
    }
    let done = || user.futex_woken.load(Ordering::Acquire) || sched::interrupted();
    let in_time = match deadline_ns {
        None => {
            wait::wait_until(key_of(user), "futex", done);
            true
        }
        Some(deadline) => wait::wait_until_or_timeout(key_of(user), "futex", deadline, done),
    };
    if user.futex_woken.load(Ordering::Acquire) {
        return Ok(());
    }
    // Off the list, unless a waker took it off just now: then the wake-up
    // is this wait's.
    let mut waiters = bucket.lock();
    let Some(waiters) = waiters.as_mut() else { return Ok(()) };
    let before = waiters.len();
    waiters.retain(|w| !ThreadRef::ptr_eq(&w.thread, &me));
    if waiters.len() == before {
        return Ok(());
    }
    Err(error(if in_time { Kernel::Interrupted } else { Kernel::TimedOut }))
}

/// Wakes up to `count` of the threads waiting on the word at `address` of
/// `space`, first come first woken; returns how many.
pub fn wake(space: &SpaceObject, address: u64, count: u32) -> Result<u32> {
    if !address.is_multiple_of(4) {
        return Err(Kernel::InvalidArgument.status(0));
    }
    let key = space as *const SpaceObject as usize;
    let mut woken = 0;
    let mut waiters = list(key, address).lock();
    let Some(waiters) = waiters.as_mut() else { return Ok(0) };
    let mut index = 0;
    while index < waiters.len() && woken < count {
        let waiter = &waiters[index];
        if waiter.space != key || waiter.address != address {
            index += 1;
            continue;
        }
        let waiter = waiters.remove(index);
        waiter.user.futex_woken.store(true, Ordering::Release);
        wait::wake_all(key_of(&waiter.user));
        woken += 1;
    }
    Ok(woken)
}
