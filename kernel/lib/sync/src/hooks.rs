// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! What the locks need from the kernel once it runs threads.
//!
//! Before the scheduler starts there is one thread per CPU and nothing to
//! preempt, so the locks work on their own. The kernel then registers its
//! [`Hooks`] with [`set_hooks`], once, on the boot CPU, while it holds no
//! lock: from then on every lock tells the kernel when it is taken and
//! let go, which keeps spin-lock holders on their CPU, feeds the
//! lock-order checker, and lets a contended [`crate::Mutex`] sleep.

use core::panic::Location;
use core::sync::atomic::{AtomicBool, AtomicU32, Ordering};

use crate::Once;
use crate::mutex::RawMutex;

/// Where a lock was created, which names its class for the lock-order
/// checker, and the class number the checker gave it.
pub struct LockSite {
    location: &'static Location<'static>,
    /// The checker's class number plus one, or 0 before it has one.
    class: AtomicU32,
}

/// The location of the code that called the `#[track_caller]` function
/// this is called from. Kani cannot model the `caller_location` intrinsic,
/// so its proofs see one location fixed at compile time instead, which
/// needs no intrinsic at run time.
#[track_caller]
#[inline(always)]
pub(crate) const fn caller() -> &'static Location<'static> {
    #[cfg(kani)]
    {
        const FIXED: &Location<'static> = Location::caller();
        FIXED
    }
    #[cfg(not(kani))]
    Location::caller()
}

impl LockSite {
    /// The site of the caller's caller: the code that creates the lock.
    #[track_caller]
    pub const fn here() -> LockSite {
        LockSite { location: caller(), class: AtomicU32::new(0) }
    }

    /// The file, line and column of the code that created the lock.
    pub fn location(&self) -> &'static Location<'static> {
        self.location
    }

    /// The class the checker recorded for this lock (subclass 0), if any.
    pub fn cached_class(&self) -> Option<u32> {
        match self.class.load(Ordering::Relaxed) {
            0 => None,
            n => Some(n - 1),
        }
    }

    /// Records the checker's class for this lock.
    pub fn cache_class(&self, class: u32) {
        self.class.store(class.saturating_add(1), Ordering::Relaxed);
    }
}

/// Whether a lock spins or sleeps.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LockKind {
    /// A [`crate::SpinLock`]: its holder cannot sleep or be preempted.
    Spin,
    /// A [`crate::Mutex`]: its holder may sleep, and so may its waiters.
    Mutex,
}

/// A lock being taken, as the hooks see it.
#[derive(Clone, Copy)]
pub struct Acquire<'a> {
    /// The lock's site and class.
    pub site: &'a LockSite,
    /// The lock's address, which tells two locks of one class apart.
    pub addr: usize,
    /// Spin lock or mutex.
    pub kind: LockKind,
    /// The nesting level for taking two locks of one class at once (0
    /// unless the code says otherwise with `lock_nested`).
    pub subclass: u8,
    /// The code taking it.
    pub at: &'static Location<'static>,
    /// Whether this is a successful `try_lock`, which cannot deadlock.
    pub trylock: bool,
}

/// The kernel services the locks use. Every method has a default that does
/// nothing (or spins), which is how the locks behave before the kernel
/// registers its hooks.
pub trait Hooks: Sync {
    /// Masks interrupts on this CPU and returns whether they were enabled.
    fn disable_interrupts(&self) -> bool {
        false
    }

    /// Undoes [`Hooks::disable_interrupts`].
    fn restore_interrupts(&self, _were_enabled: bool) {}

    /// Keeps the running thread on this CPU, unpreempted, until the
    /// matching [`Hooks::preempt_enable`]. Calls nest.
    fn preempt_disable(&self) {}

    /// Undoes one [`Hooks::preempt_disable`]; may switch threads if that
    /// was the last one and a more urgent thread is waiting.
    fn preempt_enable(&self) {}

    /// A lock is about to be waited for, or a `try_lock` got it. The
    /// lock-order checker hooks in here.
    fn lock_acquire(&self, _lock: Acquire<'_>) {}

    /// A lock was let go.
    fn lock_release(&self, _site: &LockSite, _addr: usize) {}

    /// A token for the running thread: non-zero and even once threads run
    /// (bit 0 of a mutex's word marks waiters), 0 before.
    fn current_thread(&self) -> usize {
        0
    }

    /// Waits until the caller owns `mutex`, whose owner word is held by
    /// someone else. The word holds the owner's token, with
    /// [`RawMutex::WAITERS`] set while anyone waits; the implementation
    /// sets it, sleeps, and returns once the lock is the caller's.
    fn mutex_wait(&self, mutex: &RawMutex) {
        let me = self.current_thread();
        while mutex.word().compare_exchange(0, me, Ordering::Acquire, Ordering::Relaxed).is_err() {
            core::hint::spin_loop();
        }
    }

    /// The owner lets go of `mutex`, which has waiters: hand it over (or
    /// clear the word).
    fn mutex_wake(&self, mutex: &RawMutex) {
        mutex.word().store(0, Ordering::Release);
    }

    /// Returns once every RCU read section that was running when it was
    /// called has ended.
    fn synchronize_rcu(&self) {}
}

static HOOKS: Once<&'static dyn Hooks> = Once::new();

/// Registers the kernel's hooks. Call once, on the boot CPU, before other
/// CPUs start and while holding no lock; later calls are ignored.
pub fn set_hooks(hooks: &'static dyn Hooks) {
    let _ = HOOKS.set(hooks);
}

/// The hooks the locks should call now: none before they are registered,
/// in panic mode, or under Kani.
pub(crate) fn active() -> Option<&'static dyn Hooks> {
    if cfg!(kani) || crate::in_panic_mode() {
        return None;
    }
    HOOKS.get().copied()
}

/// Whether the locks tell the hooks about each acquisition and release.
static CHECKING: AtomicBool = AtomicBool::new(true);

/// Tells the locks whether anything listens to [`Hooks::lock_acquire`] and
/// [`Hooks::lock_release`]: the kernel turns this off while its lock-order
/// checker is off, so a lock costs a load instead of two calls that return
/// at once. Change it only while no CPU holds a lock, as for turning the
/// checker on or off.
pub fn set_checking(on: bool) {
    CHECKING.store(on, Ordering::Relaxed);
}

/// The hooks to tell about an acquisition or release, if they listen.
pub(crate) fn checking(hooks: Option<&'static dyn Hooks>) -> Option<&'static dyn Hooks> {
    hooks.filter(|_| CHECKING.load(Ordering::Relaxed))
}
