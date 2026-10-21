// SPDX-FileCopyrightText: 2026 The OrthrOS contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! [`Mutex`], a lock whose waiters sleep.
//!
//! The lock is one word: 0 when free, otherwise the owner's thread token
//! (from [`Hooks::current_thread`]) with bit 0 ([`RawMutex::WAITERS`]) set
//! while threads wait. Taking a free lock and letting go of one nobody
//! waits for are a single compare-and-swap each. The rest is the kernel's:
//! [`Hooks::mutex_wait`] queues the caller by priority, lends its priority
//! to the owner (priority inheritance) and sleeps until the lock is handed
//! over; [`Hooks::mutex_wake`] hands it to the most urgent waiter.
//!
//! Before the kernel registers its hooks there are no threads, and a
//! contended mutex spins like a spin lock.

use core::cell::UnsafeCell;
use core::fmt;
use core::marker::PhantomData;
use core::ops::{Deref, DerefMut};
use core::panic::Location;
use core::sync::atomic::{AtomicUsize, Ordering};

use crate::hooks::{self, Acquire, Hooks, LockKind, LockSite};
use crate::{PANIC_SPINS, in_panic_mode};

/// The owner token used before threads exist.
const NO_THREAD: usize = 2;

/// The lock word and site of a [`Mutex`], which the kernel's hooks work
/// on.
pub struct RawMutex {
    word: AtomicUsize,
    site: LockSite,
}

impl RawMutex {
    /// Bit 0 of the word: threads are waiting, so letting go must go
    /// through [`Hooks::mutex_wake`].
    pub const WAITERS: usize = 1;

    #[track_caller]
    const fn new() -> RawMutex {
        RawMutex { word: AtomicUsize::new(0), site: LockSite::here() }
    }

    /// The lock word: the owner's token and [`RawMutex::WAITERS`].
    pub fn word(&self) -> &AtomicUsize {
        &self.word
    }

    /// The owner's token, or 0 when the lock is free.
    pub fn owner(&self) -> usize {
        self.word.load(Ordering::Relaxed) & !Self::WAITERS
    }

    /// The lock's address, which names it to the kernel's wait queues.
    pub fn key(&self) -> usize {
        self as *const RawMutex as usize
    }

    /// Where the lock was created.
    pub fn site(&self) -> &LockSite {
        &self.site
    }
}

/// A mutual-exclusion lock whose waiters sleep, in priority order, and
/// lend their priority to the holder. The holder may sleep too. Taking it
/// where sleeping is not allowed (holding a spin lock, in an interrupt
/// handler) is a bug the lock-order checker reports.
pub struct Mutex<T: ?Sized> {
    raw: RawMutex,
    data: UnsafeCell<T>,
}

// SAFETY: one guard at a time (outside panic mode) gives access to the
// data, so sharing the lock is as safe as sending `T` to its holder.
unsafe impl<T: ?Sized + Send> Sync for Mutex<T> {}
// SAFETY: as above.
unsafe impl<T: ?Sized + Send> Send for Mutex<T> {}

impl<T> Mutex<T> {
    /// Creates an unlocked mutex around `value`. Every mutex created by the
    /// same line of code is one class for the lock-order checker.
    #[track_caller]
    pub const fn new(value: T) -> Self {
        Self { raw: RawMutex::new(), data: UnsafeCell::new(value) }
    }
}

impl<T: ?Sized> Mutex<T> {
    /// Takes the lock, sleeping while another thread holds it.
    #[track_caller]
    pub fn lock(&self) -> MutexGuard<'_, T> {
        let hooks = hooks::active();
        if let Some(hooks) = hooks {
            hooks.lock_acquire(self.acquire_info(Location::caller(), false));
        }
        let me = hooks.map_or(0, |h| h.current_thread());
        let token = if me == 0 { NO_THREAD } else { me };
        if self.claim(token) {
            return self.guard(hooks, token);
        }
        match hooks {
            Some(hooks) if me != 0 => hooks.mutex_wait(&self.raw),
            _ => self.spin(token),
        }
        self.guard(hooks, token)
    }

    /// Takes the lock if it is free.
    #[track_caller]
    pub fn try_lock(&self) -> Option<MutexGuard<'_, T>> {
        let hooks = hooks::active();
        let me = hooks.map_or(0, |h| h.current_thread());
        let token = if me == 0 { NO_THREAD } else { me };
        if !self.claim(token) {
            return None;
        }
        if let Some(hooks) = hooks {
            hooks.lock_acquire(self.acquire_info(Location::caller(), true));
        }
        Some(self.guard(hooks, token))
    }

    /// Whether some thread holds the lock right now.
    pub fn is_locked(&self) -> bool {
        self.raw.owner() != 0
    }

    /// The lock word and site.
    pub fn raw(&self) -> &RawMutex {
        &self.raw
    }

    /// The value, through `&mut`, which proves no guard exists.
    pub fn get_mut(&mut self) -> &mut T {
        self.data.get_mut()
    }

    fn claim(&self, token: usize) -> bool {
        self.raw.word.compare_exchange(0, token, Ordering::Acquire, Ordering::Relaxed).is_ok()
    }

    /// Waits for the lock without the scheduler: before threads exist, and
    /// in panic mode, where it is taken anyway after a bounded wait.
    fn spin(&self, token: usize) {
        let mut spins: u32 = 0;
        while !self.claim(token) {
            core::hint::spin_loop();
            spins = spins.saturating_add(1);
            if spins >= PANIC_SPINS && in_panic_mode() {
                return;
            }
        }
    }

    fn guard(&self, hooks: Option<&'static dyn Hooks>, token: usize) -> MutexGuard<'_, T> {
        MutexGuard { mutex: self, hooks, token, _not_send: PhantomData }
    }

    fn acquire_info(&self, at: &'static Location<'static>, trylock: bool) -> Acquire<'_> {
        Acquire {
            site: &self.raw.site,
            addr: self.raw.key(),
            kind: LockKind::Mutex,
            subclass: 0,
            at,
            trylock,
        }
    }
}

impl<T: ?Sized + fmt::Debug> fmt::Debug for Mutex<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.try_lock() {
            Some(guard) => f.debug_struct("Mutex").field("data", &&*guard).finish(),
            None => f.write_str("Mutex { <locked> }"),
        }
    }
}

/// Access to the data of a locked [`Mutex`]; unlocks when dropped. The
/// thread that took it lets go of it.
pub struct MutexGuard<'a, T: ?Sized> {
    mutex: &'a Mutex<T>,
    hooks: Option<&'static dyn Hooks>,
    token: usize,
    _not_send: PhantomData<*const ()>,
}

impl<T: ?Sized> Deref for MutexGuard<'_, T> {
    type Target = T;

    fn deref(&self) -> &T {
        // SAFETY: the guard exists only while its thread owns the lock (or
        // in panic mode, see `enter_panic_mode`), so no other reference to
        // the data is live.
        unsafe { &*self.mutex.data.get() }
    }
}

impl<T: ?Sized> DerefMut for MutexGuard<'_, T> {
    fn deref_mut(&mut self) -> &mut T {
        // SAFETY: as for `deref`.
        unsafe { &mut *self.mutex.data.get() }
    }
}

impl<T: ?Sized> Drop for MutexGuard<'_, T> {
    fn drop(&mut self) {
        let raw = &self.mutex.raw;
        let unlocked =
            raw.word.compare_exchange(self.token, 0, Ordering::Release, Ordering::Relaxed).is_ok();
        if !unlocked {
            match self.hooks {
                // Waiters are queued: hand the lock to the first of them.
                Some(hooks) if raw.word.load(Ordering::Relaxed) & RawMutex::WAITERS != 0 => {
                    hooks.mutex_wake(raw);
                }
                // Taken over in panic mode, or waited for without hooks.
                _ => raw.word.store(0, Ordering::Release),
            }
        }
        if let Some(hooks) = self.hooks {
            hooks.lock_release(&raw.site, raw.key());
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;

    #[test]
    fn mutex_excludes_other_threads() {
        crate::testing::install();
        let mutex = Arc::new(Mutex::new(0u64));
        let threads: Vec<_> = (0..4)
            .map(|_| {
                let mutex = Arc::clone(&mutex);
                std::thread::spawn(move || {
                    for _ in 0..5_000 {
                        let mut guard = mutex.lock();
                        let value = *guard;
                        std::hint::black_box(&mut *guard);
                        *guard = value + 1;
                    }
                })
            })
            .collect();
        for thread in threads {
            thread.join().unwrap();
        }
        assert_eq!(*mutex.lock(), 20_000);
        assert!(!mutex.is_locked());
    }

    #[test]
    fn try_lock_fails_while_held() {
        crate::testing::install();
        let mutex = Mutex::new(1);
        let guard = mutex.lock();
        assert!(mutex.is_locked());
        assert_ne!(mutex.raw().owner(), 0);
        let other = std::thread::scope(|s| s.spawn(|| mutex.try_lock().is_none()).join().unwrap());
        assert!(other);
        drop(guard);
        assert!(mutex.try_lock().is_some());
        assert_eq!(mutex.raw().word().load(Ordering::Relaxed), 0);
    }

    #[test]
    fn mutexes_name_their_site() {
        let a = Mutex::new(());
        assert!(a.raw().site().location().file().ends_with("mutex.rs"));
        assert_eq!(a.raw().key(), a.raw() as *const RawMutex as usize);
    }
}
