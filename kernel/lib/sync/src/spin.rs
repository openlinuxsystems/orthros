// SPDX-FileCopyrightText: 2026 The OrthrOS contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Spin locks: [`SpinLock`], which the kernel's hooks see, and
//! [`RawSpinLock`], which they do not.

use core::cell::UnsafeCell;
use core::fmt;
use core::marker::PhantomData;
use core::ops::{Deref, DerefMut};
use core::panic::Location;
use core::sync::atomic::{AtomicBool, Ordering};

use crate::hooks::{self, Acquire, Hooks, LockKind, LockSite};
use crate::{PANIC_SPINS, in_panic_mode};

/// The test-and-test-and-set flag both lock types are built on.
struct Flag(AtomicBool);

impl Flag {
    const fn new() -> Flag {
        Flag(AtomicBool::new(false))
    }

    fn try_acquire(&self) -> bool {
        self.0.compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed).is_ok()
    }

    /// Spins until the flag is free and takes it. In panic mode, takes it
    /// after a bounded wait even if it is still held.
    fn acquire(&self) {
        // Kani proofs are single-threaded: a lock that is already held can
        // only mean a deadlock, which the proof should report rather than
        // unwind the spin loop to its bound.
        if cfg!(kani) {
            assert!(self.try_acquire(), "deadlock: the lock is already held");
            return;
        }
        let mut spins: u32 = 0;
        loop {
            if self.try_acquire() {
                return;
            }
            while self.0.load(Ordering::Relaxed) {
                core::hint::spin_loop();
                spins = spins.saturating_add(1);
                if spins >= PANIC_SPINS && in_panic_mode() {
                    return;
                }
            }
        }
    }

    fn release(&self) {
        self.0.store(false, Ordering::Release);
    }

    fn is_set(&self) -> bool {
        self.0.load(Ordering::Relaxed)
    }
}

/// A test-and-test-and-set spin lock.
///
/// Once the kernel has registered its [`Hooks`], holding a `SpinLock`
/// keeps the holder from being preempted (it must not sleep), and every
/// acquisition is checked by the lock-order checker under the class named
/// by where the lock was created. `lock` does not mask interrupts: a lock
/// that an interrupt handler also takes must be taken with
/// [`SpinLock::lock_irq`] everywhere else.
pub struct SpinLock<T: ?Sized> {
    flag: Flag,
    site: LockSite,
    data: UnsafeCell<T>,
}

// SAFETY: the lock hands out at most one guard at a time (outside panic
// mode, see `enter_panic_mode`), so sending the lock to or sharing it with
// another CPU is as safe as sending `T` itself.
unsafe impl<T: ?Sized + Send> Sync for SpinLock<T> {}
// SAFETY: as above.
unsafe impl<T: ?Sized + Send> Send for SpinLock<T> {}

impl<T> SpinLock<T> {
    /// Creates an unlocked lock around `value`. Every lock created by the
    /// same line of code is one class for the lock-order checker.
    #[track_caller]
    pub const fn new(value: T) -> Self {
        Self { flag: Flag::new(), site: LockSite::here(), data: UnsafeCell::new(value) }
    }

    /// The value, now that nothing else can hold the lock.
    pub fn into_inner(self) -> T {
        self.data.into_inner()
    }
}

impl<T: ?Sized> SpinLock<T> {
    /// Spins until the lock is free, then takes it. In panic mode, takes it
    /// after a bounded wait even if it is still held.
    #[track_caller]
    pub fn lock(&self) -> SpinLockGuard<'_, T> {
        self.acquire(0, false, Location::caller())
    }

    /// Like [`SpinLock::lock`], for code that holds another lock of the same
    /// class: `subclass` (1, 2 ...) tells the checker the nesting is
    /// deliberate and always in this order.
    #[track_caller]
    pub fn lock_nested(&self, subclass: u8) -> SpinLockGuard<'_, T> {
        self.acquire(subclass, false, Location::caller())
    }

    /// Masks interrupts on this CPU, then takes the lock; the guard unmasks
    /// them again (if they were on). For locks interrupt handlers take.
    #[track_caller]
    pub fn lock_irq(&self) -> SpinLockGuard<'_, T> {
        self.acquire(0, true, Location::caller())
    }

    /// [`SpinLock::lock_irq`] with a subclass, as for
    /// [`SpinLock::lock_nested`].
    #[track_caller]
    pub fn lock_irq_nested(&self, subclass: u8) -> SpinLockGuard<'_, T> {
        self.acquire(subclass, true, Location::caller())
    }

    fn acquire(
        &self,
        subclass: u8,
        irq: bool,
        at: &'static Location<'static>,
    ) -> SpinLockGuard<'_, T> {
        let hooks = hooks::active();
        let irq_state = enter(hooks, irq);
        if let Some(hooks) = hooks {
            hooks.lock_acquire(self.acquire_info(subclass, at, false));
        }
        self.flag.acquire();
        SpinLockGuard { lock: self, hooks, irq: irq_state, _not_send: PhantomData }
    }

    fn acquire_info(
        &self,
        subclass: u8,
        at: &'static Location<'static>,
        trylock: bool,
    ) -> Acquire<'_> {
        Acquire {
            site: &self.site,
            addr: self as *const Self as *const () as usize,
            kind: LockKind::Spin,
            subclass,
            at,
            trylock,
        }
    }

    /// Takes the lock if it is free.
    #[track_caller]
    pub fn try_lock(&self) -> Option<SpinLockGuard<'_, T>> {
        self.try_acquire(false, Location::caller())
    }

    /// Masks interrupts and takes the lock if it is free; unmasks them
    /// again if it is not.
    #[track_caller]
    pub fn try_lock_irq(&self) -> Option<SpinLockGuard<'_, T>> {
        self.try_acquire(true, Location::caller())
    }

    fn try_acquire(
        &self,
        irq: bool,
        at: &'static Location<'static>,
    ) -> Option<SpinLockGuard<'_, T>> {
        let hooks = hooks::active();
        let irq_state = enter(hooks, irq);
        if !self.flag.try_acquire() {
            leave(hooks, irq_state);
            return None;
        }
        if let Some(hooks) = hooks {
            hooks.lock_acquire(self.acquire_info(0, at, true));
        }
        Some(SpinLockGuard { lock: self, hooks, irq: irq_state, _not_send: PhantomData })
    }

    /// Whether some CPU holds the lock right now.
    pub fn is_locked(&self) -> bool {
        self.flag.is_set()
    }

    /// Where the lock was created.
    pub fn site(&self) -> &LockSite {
        &self.site
    }

    /// The value, through `&mut`, which proves no guard exists.
    pub fn get_mut(&mut self) -> &mut T {
        self.data.get_mut()
    }
}

/// Holds off preemption and, for `irq`, masks interrupts: what a spin-lock
/// holder needs before it takes the lock. Returns whether interrupts were
/// on, when they were masked.
fn enter(hooks: Option<&'static dyn Hooks>, irq: bool) -> Option<bool> {
    let hooks = hooks?;
    hooks.preempt_disable();
    irq.then(|| hooks.disable_interrupts())
}

/// Undoes [`enter`]: interrupts first, so that the last `preempt_enable`
/// can switch to a waiting thread.
fn leave(hooks: Option<&'static dyn Hooks>, irq: Option<bool>) {
    if let Some(hooks) = hooks {
        if let Some(were_enabled) = irq {
            hooks.restore_interrupts(were_enabled);
        }
        hooks.preempt_enable();
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

/// Access to the data of a locked [`SpinLock`]; unlocks when dropped. It
/// stays on the CPU that took it.
pub struct SpinLockGuard<'a, T: ?Sized> {
    lock: &'a SpinLock<T>,
    /// The hooks told about the acquisition, told again about the release.
    hooks: Option<&'static dyn Hooks>,
    /// Whether interrupts were on before `lock_irq` masked them.
    irq: Option<bool>,
    _not_send: PhantomData<*const ()>,
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
        self.lock.flag.release();
        if let Some(hooks) = self.hooks {
            hooks.lock_release(
                &self.lock.site,
                self.lock as *const SpinLock<T> as *const () as usize,
            );
        }
        leave(self.hooks, self.irq);
    }
}

/// A spin lock that never calls the kernel's hooks: taking it neither holds
/// off preemption nor reaches the lock-order checker. It is for the
/// checker's own state and the few places that must not recurse into it;
/// the caller keeps interrupts masked while it holds one if an interrupt
/// handler could take it too.
pub struct RawSpinLock<T: ?Sized> {
    flag: Flag,
    data: UnsafeCell<T>,
}

// SAFETY: as for `SpinLock`.
unsafe impl<T: ?Sized + Send> Sync for RawSpinLock<T> {}
// SAFETY: as for `SpinLock`.
unsafe impl<T: ?Sized + Send> Send for RawSpinLock<T> {}

impl<T> RawSpinLock<T> {
    /// Creates an unlocked lock around `value`.
    pub const fn new(value: T) -> Self {
        Self { flag: Flag::new(), data: UnsafeCell::new(value) }
    }
}

impl<T: ?Sized> RawSpinLock<T> {
    /// Spins until the lock is free, then takes it (in panic mode, after a
    /// bounded wait in any case).
    pub fn lock(&self) -> RawSpinLockGuard<'_, T> {
        self.flag.acquire();
        RawSpinLockGuard { lock: self }
    }

    /// Takes the lock if it is free.
    pub fn try_lock(&self) -> Option<RawSpinLockGuard<'_, T>> {
        self.flag.try_acquire().then_some(RawSpinLockGuard { lock: self })
    }

    /// Whether some CPU holds the lock right now.
    pub fn is_locked(&self) -> bool {
        self.flag.is_set()
    }
}

/// Access to the data of a locked [`RawSpinLock`]; unlocks when dropped.
pub struct RawSpinLockGuard<'a, T: ?Sized> {
    lock: &'a RawSpinLock<T>,
}

impl<T: ?Sized> Deref for RawSpinLockGuard<'_, T> {
    type Target = T;

    fn deref(&self) -> &T {
        // SAFETY: as for `SpinLockGuard`.
        unsafe { &*self.lock.data.get() }
    }
}

impl<T: ?Sized> DerefMut for RawSpinLockGuard<'_, T> {
    fn deref_mut(&mut self) -> &mut T {
        // SAFETY: as for `SpinLockGuard`.
        unsafe { &mut *self.lock.data.get() }
    }
}

impl<T: ?Sized> Drop for RawSpinLockGuard<'_, T> {
    fn drop(&mut self) {
        self.lock.flag.release();
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;

    #[test]
    fn lock_excludes_other_threads() {
        crate::testing::install();
        let lock = Arc::new(SpinLock::new(0u64));
        let threads: Vec<_> = (0..4)
            .map(|_| {
                let lock = Arc::clone(&lock);
                std::thread::spawn(move || {
                    for i in 0..10_000 {
                        if i % 2 == 0 {
                            *lock.lock() += 1;
                        } else {
                            *lock.lock_irq() += 1;
                        }
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
        assert!(lock.try_lock_irq().is_none());
        drop(guard);
        assert!(lock.try_lock().is_some());
    }

    #[test]
    fn locks_from_one_line_share_a_site() {
        let make = || SpinLock::new(0);
        let (a, b) = (make(), make());
        let c = SpinLock::new(0);
        assert_eq!(a.site().location(), b.site().location());
        assert_ne!(a.site().location(), c.site().location());
        assert!(a.site().location().file().ends_with("spin.rs"));
        a.site().cache_class(7);
        assert_eq!(a.site().cached_class(), Some(7));
        assert_eq!(c.site().cached_class(), None);
    }

    #[test]
    fn raw_lock_excludes_other_threads() {
        let lock = Arc::new(RawSpinLock::new(0u64));
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
        let guard = lock.lock();
        assert!(lock.try_lock().is_none());
        drop(guard);
        assert!(!lock.is_locked());
    }
}
