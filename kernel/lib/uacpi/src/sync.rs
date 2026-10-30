// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! uACPI's semaphores and spin locks, made from atomics and the host's
//! waits.
//!
//! uACPI blocks only on counting semaphores: created with one unit, a
//! semaphore is its locks (the namespace's, the tables', AML's `Mutex`);
//! with none, its events (AML's `Event`, waits for the global lock). A
//! semaphore here is an atomic count of units; a thread that finds none
//! sleeps in the kernel's wait table under the semaphore's address, and
//! giving a unit wakes them all, the first of which takes it. Giving never
//! sleeps, so it works in interrupt handlers and under spin locks, as
//! uACPI requires.
//!
//! uACPI's spin locks, which its interrupt handler takes, are an atomic
//! flag held with interrupts masked; they hold only a few of its counters
//! at a time. They are outside the kernel's lock checker, which sees only
//! its own locks, and uACPI takes no kernel lock while holding one.

use core::hint::spin_loop;
use core::ptr;
use core::sync::atomic::{AtomicBool, AtomicU32, Ordering};

use crate::Host;

/// The timeout, in milliseconds, with which uACPI waits for good.
pub const FOREVER_MS: u16 = 0xffff;

/// A counting semaphore.
pub struct Semaphore {
    units: AtomicU32,
}

impl Semaphore {
    pub const fn new(units: u32) -> Self {
        Self { units: AtomicU32::new(units) }
    }

    /// The key its waiters sleep under: its address.
    fn key(&self) -> usize {
        ptr::from_ref(self).addr()
    }

    /// Takes a unit if there is one.
    fn try_take(&self) -> bool {
        self.units.try_update(Ordering::Acquire, Ordering::Relaxed, |u| u.checked_sub(1)).is_ok()
    }

    /// Takes a unit, waiting up to `timeout_ms` for one: not at all for 0,
    /// for good for [`FOREVER_MS`]. Returns whether it took one.
    pub fn take(&self, host: &dyn Host, timeout_ms: u16) -> bool {
        if self.try_take() {
            return true;
        }
        if timeout_ms == 0 {
            return false;
        }
        let deadline = (timeout_ms != FOREVER_MS)
            .then(|| host.now_ns().saturating_add(u64::from(timeout_ms) * 1_000_000));
        loop {
            let ready = || self.units.load(Ordering::Relaxed) > 0;
            if !host.wait(self.key(), deadline, &ready) {
                // Out of time; a unit given at the last moment still counts.
                return self.try_take();
            }
            if self.try_take() {
                return true;
            }
        }
    }

    /// Gives a unit, waking the threads that wait for one.
    pub fn give(&self, host: &dyn Host) {
        self.units.fetch_add(1, Ordering::Release);
        host.wake_all(self.key());
    }
}

/// A spin lock, held with interrupts masked.
pub struct Spin {
    held: AtomicBool,
}

impl Spin {
    pub const fn new() -> Self {
        Self { held: AtomicBool::new(false) }
    }

    /// Masks interrupts, then takes the lock. Returns whether interrupts
    /// were on, for [`Spin::unlock`].
    pub fn lock(&self, host: &dyn Host) -> bool {
        let were_enabled = host.disable_interrupts();
        while self
            .held
            .compare_exchange_weak(false, true, Ordering::Acquire, Ordering::Relaxed)
            .is_err()
        {
            while self.held.load(Ordering::Relaxed) {
                spin_loop();
            }
        }
        were_enabled
    }

    /// Lets the lock go, then unmasks interrupts if they were on.
    pub fn unlock(&self, host: &dyn Host, were_enabled: bool) {
        self.held.store(false, Ordering::Release);
        host.restore_interrupts(were_enabled);
    }
}
