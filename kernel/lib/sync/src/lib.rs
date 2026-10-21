// SPDX-FileCopyrightText: 2026 The OrthrOS contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Spin locks and one-time initialisation for the Radix kernel.
//!
//! Phase 1 needs three primitives: [`SpinLock`], for state shared between
//! CPUs; [`Once`], for values set up once during boot and read for the rest
//! of the kernel's life; and [`TakeOnce`], which hands out exclusive access
//! to a static buffer (a console's text grid, a CPU's stack) exactly once.
//! All are built on atomics and work before any other part of the kernel is
//! initialised.
//!
//! This crate is part of the trusted computing base: it uses `unsafe` to hand
//! out references to data guarded by an atomic flag. Everything above it uses
//! the safe API. It compiles for the host, where its unit tests run with real
//! threads.
//!
//! # Panic mode
//!
//! When the kernel panics it stops the other CPUs, which may be holding locks
//! the panic report needs (the console, for example). After
//! [`enter_panic_mode`], [`SpinLock::lock`] gives up waiting after a bounded
//! number of spins and takes the lock anyway, so the report always gets out.

#![cfg_attr(not(test), no_std)]

use core::cell::UnsafeCell;
use core::fmt;
use core::mem::MaybeUninit;
use core::ops::{Deref, DerefMut};
use core::sync::atomic::{AtomicBool, AtomicU8, Ordering};

/// Set once the kernel has panicked and the other CPUs have been stopped.
static PANIC_MODE: AtomicBool = AtomicBool::new(false);

/// How long [`SpinLock::lock`] waits in panic mode before taking the lock.
const PANIC_SPINS: u32 = 1 << 22;

/// Switches every [`SpinLock`] into panic mode: a lock that stays held for
/// too long is taken anyway.
///
/// # Safety
///
/// Call this only from the panic path, after the other CPUs have been asked
/// to stop. A lock still held by a stopped CPU (or by an interrupted frame
/// on this CPU) is then shared with the panic path, so the guarded data may
/// be seen half-updated; code that runs in panic mode must tolerate that.
pub unsafe fn enter_panic_mode() {
    PANIC_MODE.store(true, Ordering::SeqCst);
}

/// Whether [`enter_panic_mode`] has been called.
pub fn in_panic_mode() -> bool {
    PANIC_MODE.load(Ordering::Relaxed)
}

/// A test-and-test-and-set spin lock.
///
/// It does not disable interrupts. Code that takes a lock which an
/// interrupt handler also takes must disable interrupts around it.
pub struct SpinLock<T: ?Sized> {
    locked: AtomicBool,
    data: UnsafeCell<T>,
}

// SAFETY: the lock hands out at most one guard at a time (outside panic
// mode, see `enter_panic_mode`), so sending the lock to or sharing it with
// another CPU is as safe as sending `T` itself.
unsafe impl<T: ?Sized + Send> Sync for SpinLock<T> {}
// SAFETY: as above.
unsafe impl<T: ?Sized + Send> Send for SpinLock<T> {}

impl<T> SpinLock<T> {
    /// Creates an unlocked lock around `value`.
    pub const fn new(value: T) -> Self {
        Self { locked: AtomicBool::new(false), data: UnsafeCell::new(value) }
    }
}

impl<T: ?Sized> SpinLock<T> {
    /// Spins until the lock is free, then takes it. In panic mode, takes it
    /// after a bounded wait even if it is still held.
    pub fn lock(&self) -> SpinLockGuard<'_, T> {
        // Kani proofs are single-threaded: a lock that is already held can
        // only mean a deadlock, which the proof should report rather than
        // unwind the spin loop to its bound.
        if cfg!(kani) {
            return self.try_lock().expect("deadlock: the lock is already held");
        }
        let mut spins: u32 = 0;
        loop {
            if let Some(guard) = self.try_lock() {
                return guard;
            }
            while self.locked.load(Ordering::Relaxed) {
                core::hint::spin_loop();
                spins = spins.saturating_add(1);
                if spins >= PANIC_SPINS && in_panic_mode() {
                    return SpinLockGuard { lock: self };
                }
            }
        }
    }

    /// Takes the lock if it is free.
    pub fn try_lock(&self) -> Option<SpinLockGuard<'_, T>> {
        self.locked
            .compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed)
            .ok()
            .map(|_| SpinLockGuard { lock: self })
    }

    /// Whether some CPU holds the lock right now.
    pub fn is_locked(&self) -> bool {
        self.locked.load(Ordering::Relaxed)
    }
}

impl<T: ?Sized + fmt::Debug> fmt::Debug for SpinLock<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.try_lock() {
            Some(guard) => f.debug_struct("SpinLock").field("data", &&*guard).finish(),
            None => f.write_str("SpinLock { <locked> }"),
        }
    }
}

/// Access to the data of a locked [`SpinLock`]; unlocks when dropped.
pub struct SpinLockGuard<'a, T: ?Sized> {
    lock: &'a SpinLock<T>,
}

impl<T: ?Sized> Deref for SpinLockGuard<'_, T> {
    type Target = T;

    fn deref(&self) -> &T {
        // SAFETY: the guard exists only while this CPU holds the lock (or,
        // in panic mode, after the caller of `enter_panic_mode` accepted
        // sharing), so no other reference to the data is live.
        unsafe { &*self.lock.data.get() }
    }
}

impl<T: ?Sized> DerefMut for SpinLockGuard<'_, T> {
    fn deref_mut(&mut self) -> &mut T {
        // SAFETY: as for `deref`; `&mut self` makes this the only access
        // through this guard.
        unsafe { &mut *self.lock.data.get() }
    }
}

impl<T: ?Sized> Drop for SpinLockGuard<'_, T> {
    fn drop(&mut self) {
        self.lock.locked.store(false, Ordering::Release);
    }
}

const UNINIT: u8 = 0;
const RUNNING: u8 = 1;
const READY: u8 = 2;

/// A value that is set once and then only read.
pub struct Once<T> {
    state: AtomicU8,
    value: UnsafeCell<MaybeUninit<T>>,
}

// SAFETY: the value is written once, by the CPU that moves the state from
// UNINIT to RUNNING, and published with a release store of READY; readers
// only hand out shared references after an acquire load sees READY.
unsafe impl<T: Send + Sync> Sync for Once<T> {}
// SAFETY: as above.
unsafe impl<T: Send> Send for Once<T> {}

impl<T> Once<T> {
    /// Creates an empty cell.
    pub const fn new() -> Self {
        Self { state: AtomicU8::new(UNINIT), value: UnsafeCell::new(MaybeUninit::uninit()) }
    }

    /// The value, if it has been set.
    pub fn get(&self) -> Option<&T> {
        if self.state.load(Ordering::Acquire) == READY {
            // SAFETY: READY is stored only after the value was written, and
            // the value is never written again.
            Some(unsafe { (*self.value.get()).assume_init_ref() })
        } else {
            None
        }
    }

    /// Sets the value. Fails, returning `value`, if it was already set or
    /// is being set by another CPU.
    pub fn set(&self, value: T) -> Result<&T, T> {
        if self
            .state
            .compare_exchange(UNINIT, RUNNING, Ordering::Acquire, Ordering::Acquire)
            .is_err()
        {
            return Err(value);
        }
        // SAFETY: winning the UNINIT -> RUNNING exchange gives this CPU sole
        // access to the slot until READY is published.
        let stored = unsafe { (*self.value.get()).write(value) };
        self.state.store(READY, Ordering::Release);
        Ok(stored)
    }

    /// Returns the value, setting it with `init` first if needed. Spins
    /// while another CPU is running its own `init`.
    pub fn call_once(&self, init: impl FnOnce() -> T) -> &T {
        match self.state.compare_exchange(UNINIT, RUNNING, Ordering::Acquire, Ordering::Acquire) {
            Ok(_) => {
                // SAFETY: as in `set`.
                let stored = unsafe { (*self.value.get()).write(init()) };
                self.state.store(READY, Ordering::Release);
                stored
            }
            Err(_) => loop {
                if let Some(value) = self.get() {
                    return value;
                }
                core::hint::spin_loop();
            },
        }
    }
}

impl<T> Default for Once<T> {
    fn default() -> Self {
        Self::new()
    }
}

impl<T> Drop for Once<T> {
    fn drop(&mut self) {
        if *self.state.get_mut() == READY {
            // SAFETY: READY means the value was initialised, and `&mut self`
            // means nothing else can observe it any more.
            unsafe { self.value.get_mut().assume_init_drop() }
        }
    }
}

/// A static value that one caller can claim for the rest of the kernel's
/// life, as a `&'static mut`.
///
/// Large buffers live in statics because the kernel has no heap yet;
/// `TakeOnce` lets safe code own one without `static mut`.
pub struct TakeOnce<T> {
    taken: AtomicBool,
    value: UnsafeCell<T>,
}

// SAFETY: the value is reachable only through the single `&'static mut`
// handed out by `take`, so sharing the cell between CPUs is as safe as
// sending `T` to the CPU that took it.
unsafe impl<T: Send> Sync for TakeOnce<T> {}

impl<T> TakeOnce<T> {
    /// Creates an untaken cell holding `value`.
    pub const fn new(value: T) -> Self {
        Self { taken: AtomicBool::new(false), value: UnsafeCell::new(value) }
    }

    /// Claims the value. Returns `None` if it was claimed before.
    // Handing out `&mut` from `&self` is the point of the type; the flag
    // makes it happen at most once.
    #[allow(clippy::mut_from_ref)]
    pub fn take(&'static self) -> Option<&'static mut T> {
        if self.taken.swap(true, Ordering::AcqRel) {
            return None;
        }
        // SAFETY: only the first caller sees `taken` go from false to true,
        // so this is the only reference to the value ever created.
        Some(unsafe { &mut *self.value.get() })
    }

    /// Whether the value has been claimed.
    pub fn is_taken(&self) -> bool {
        self.taken.load(Ordering::Relaxed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    #[test]
    fn lock_excludes_other_threads() {
        let lock = Arc::new(SpinLock::new(0u64));
        let threads: Vec<_> = (0..4)
            .map(|_| {
                let lock = Arc::clone(&lock);
                std::thread::spawn(move || {
                    for _ in 0..10_000 {
                        *lock.lock() += 1;
                    }
                })
            })
            .collect();
        for thread in threads {
            thread.join().unwrap();
        }
        assert_eq!(*lock.lock(), 40_000);
    }

    #[test]
    fn try_lock_fails_while_held() {
        let lock = SpinLock::new(());
        let guard = lock.lock();
        assert!(lock.is_locked());
        assert!(lock.try_lock().is_none());
        drop(guard);
        assert!(lock.try_lock().is_some());
    }

    #[test]
    fn once_sets_exactly_once() {
        let cell = Once::new();
        assert!(cell.get().is_none());
        assert_eq!(*cell.call_once(|| 5), 5);
        assert_eq!(cell.set(6), Err(6));
        assert_eq!(*cell.call_once(|| 7), 5);
        assert_eq!(cell.get(), Some(&5));
    }

    #[test]
    fn once_drops_its_value() {
        let marker = Arc::new(());
        {
            let cell = Once::new();
            cell.set(Arc::clone(&marker)).unwrap();
            assert_eq!(Arc::strong_count(&marker), 2);
        }
        assert_eq!(Arc::strong_count(&marker), 1);
    }

    #[test]
    fn take_once_hands_out_one_reference() {
        static CELL: TakeOnce<[u8; 4]> = TakeOnce::new([1, 2, 3, 4]);
        assert!(!CELL.is_taken());
        let first = CELL.take().unwrap();
        first[0] = 9;
        assert!(CELL.take().is_none());
        assert!(CELL.is_taken());
        assert_eq!(first, &[9, 2, 3, 4]);
    }
}
