// SPDX-FileCopyrightText: 2026 The OrthrOS contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! A spin lock, for the runtime's few shared tables. It is held for a
//! few instructions at a time; a thread that finds it held gives up its
//! CPU (`yield`) while it waits, so the holder can finish.

use core::cell::UnsafeCell;
use core::ops::{Deref, DerefMut};
use core::sync::atomic::{AtomicBool, Ordering};

use libradix::SYS;
use radix_abi::syscalls;

/// A value behind a spin lock.
pub struct Lock<T> {
    held: AtomicBool,
    value: UnsafeCell<T>,
}

// SAFETY: the value is only reached through a guard, and one guard at a
// time exists: `lock` takes `held` with acquire ordering before making
// one, and the guard's drop lets go of it with release ordering.
unsafe impl<T: Send> Sync for Lock<T> {}

impl<T> Lock<T> {
    /// A lock around `value`.
    pub const fn new(value: T) -> Lock<T> {
        Lock { held: AtomicBool::new(false), value: UnsafeCell::new(value) }
    }

    /// Takes the lock, waiting while another thread holds it.
    pub fn lock(&self) -> Guard<'_, T> {
        while self.held.compare_exchange_weak(false, true, Ordering::Acquire, Ordering::Relaxed).is_err()
        {
            while self.held.load(Ordering::Relaxed) {
                let _ = syscalls::r#yield(&SYS);
            }
        }
        Guard { lock: self }
    }
}

/// The lock, held.
pub struct Guard<'a, T> {
    lock: &'a Lock<T>,
}

impl<T> Deref for Guard<'_, T> {
    type Target = T;

    fn deref(&self) -> &T {
        // SAFETY: the guard holds the lock, so nothing else reaches the
        // value until it is dropped.
        unsafe { &*self.lock.value.get() }
    }
}

impl<T> DerefMut for Guard<'_, T> {
    fn deref_mut(&mut self) -> &mut T {
        // SAFETY: as for `deref`, and the guard is borrowed mutably.
        unsafe { &mut *self.lock.value.get() }
    }
}

impl<T> Drop for Guard<'_, T> {
    fn drop(&mut self) {
        self.lock.held.store(false, Ordering::Release);
    }
}
