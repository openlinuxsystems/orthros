// SPDX-FileCopyrightText: 2026 The OrthrOS contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Locks, one-time initialisation and RCU for the Radix kernel.
//!
//! Phase 1 needed three primitives: [`SpinLock`], for state shared between
//! CPUs; [`Once`], for values set up once during boot and read for the rest
//! of the kernel's life; and [`TakeOnce`], which hands out exclusive access
//! to a static buffer (a console's text grid, a CPU's stack) exactly once.
//! Phase 4 adds what a kernel with threads needs:
//!
//! - [`Mutex`], a lock that puts its waiters to sleep and hands itself to
//!   the most urgent of them, with priority inheritance (the scheduler does
//!   the sleeping, see [`Hooks::mutex_wait`]);
//! - [`RawSpinLock`], a spin lock the lock checker does not see, for the
//!   checker's own state and the scheduler's innermost bookkeeping;
//! - [`rcu`], read-copy-update: readers that never wait, for data that is
//!   read far more often than it changes.
//!
//! All of them work before any other part of the kernel is initialised.
//! Once the kernel runs threads it registers [`Hooks`]: from then on a held
//! [`SpinLock`] keeps its holder from being preempted, [`SpinLock::lock_irq`]
//! masks interrupts, every lock reports to the lock-order checker (in
//! `radix-lockdep`), and a contended [`Mutex`] sleeps instead of spinning.
//! Each lock records where it was created (with `#[track_caller]`), which
//! names its *class* for the checker: every lock made by the same line of
//! code is one class, as with Linux's lockdep.
//!
//! This crate is part of the trusted computing base: it uses `unsafe` to hand
//! out references to data guarded by an atomic flag, and RCU's pointer swaps.
//! Everything above it uses the safe API. It compiles for the host, where its
//! unit tests run with real threads.
//!
//! # Panic mode
//!
//! When the kernel panics it stops the other CPUs, which may be holding locks
//! the panic report needs (the console, for example). After
//! [`enter_panic_mode`], [`SpinLock::lock`] gives up waiting after a bounded
//! number of spins and takes the lock anyway, so the report always gets out;
//! the hooks are no longer called.

#![cfg_attr(not(test), no_std)]
// Kani's own, older nightly has the allocator API unstable still.
#![cfg_attr(kani, feature(allocator_api))]

extern crate alloc;

mod hooks;
mod mutex;
mod once;
pub mod rcu;
mod spin;

pub use hooks::{Acquire, Hooks, LockKind, LockSite, set_hooks};
pub use mutex::{Mutex, MutexGuard, RawMutex};
pub use once::{Once, TakeOnce};
pub use spin::{RawSpinLock, RawSpinLockGuard, SpinLock, SpinLockGuard};

use core::sync::atomic::{AtomicBool, Ordering};

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

#[cfg(test)]
pub(crate) mod testing {
    //! Hooks for the host tests: a "kernel" whose threads are host threads.
    //! Contended mutexes spin with `yield_now` instead of sleeping. Each
    //! thread's outermost section that holds off preemption (an RCU read
    //! section or a spin lock) records the epoch it started in, and a grace
    //! period starts a new epoch and waits for every section from an older
    //! one, which is what a grace period guarantees and a little more.

    use std::cell::Cell;
    use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

    use super::*;

    const SLOTS: usize = 64;

    static EPOCH: AtomicU64 = AtomicU64::new(0);
    /// Per thread: 0 outside a section, else the epoch it started in + 1.
    static SECTIONS: [AtomicU64; SLOTS] = [const { AtomicU64::new(0) }; SLOTS];
    static NEXT_THREAD: AtomicUsize = AtomicUsize::new(1);

    thread_local! {
        static TOKEN: Cell<usize> = const { Cell::new(0) };
        static DEPTH: Cell<usize> = const { Cell::new(0) };
    }

    pub struct HostHooks;

    impl HostHooks {
        fn slot(&self) -> &'static AtomicU64 {
            &SECTIONS[(self.current_thread() >> 1) % SLOTS]
        }
    }

    impl Hooks for HostHooks {
        fn disable_interrupts(&self) -> bool {
            false
        }

        fn restore_interrupts(&self, _were_enabled: bool) {}

        fn preempt_disable(&self) {
            let outermost = DEPTH.with(|depth| {
                depth.set(depth.get() + 1);
                depth.get() == 1
            });
            if outermost {
                self.slot().store(EPOCH.load(Ordering::SeqCst) + 1, Ordering::SeqCst);
            }
        }

        fn preempt_enable(&self) {
            let outermost = DEPTH.with(|depth| {
                depth.set(depth.get() - 1);
                depth.get() == 0
            });
            if outermost {
                self.slot().store(0, Ordering::SeqCst);
            }
        }

        fn current_thread(&self) -> usize {
            TOKEN.with(|token| {
                if token.get() == 0 {
                    token.set(NEXT_THREAD.fetch_add(1, Ordering::Relaxed) << 1);
                }
                token.get()
            })
        }

        fn mutex_wait(&self, mutex: &RawMutex) {
            let me = self.current_thread();
            loop {
                let word = mutex.word().load(Ordering::Relaxed);
                if word == 0 {
                    if mutex
                        .word()
                        .compare_exchange(0, me, Ordering::Acquire, Ordering::Relaxed)
                        .is_ok()
                    {
                        return;
                    }
                } else if word & RawMutex::WAITERS == 0 {
                    let _ = mutex.word().compare_exchange(
                        word,
                        word | RawMutex::WAITERS,
                        Ordering::Relaxed,
                        Ordering::Relaxed,
                    );
                }
                std::thread::yield_now();
            }
        }

        fn mutex_wake(&self, mutex: &RawMutex) {
            // No hand-off: the waiters race for the free lock.
            mutex.word().store(0, Ordering::Release);
        }

        fn synchronize_rcu(&self) {
            let epoch = EPOCH.fetch_add(1, Ordering::SeqCst) + 1;
            for slot in &SECTIONS {
                loop {
                    let started = slot.load(Ordering::SeqCst);
                    if started == 0 || started > epoch {
                        break;
                    }
                    std::thread::yield_now();
                }
            }
        }
    }

    static HOOKS: HostHooks = HostHooks;

    /// Registers the host hooks (once per test binary).
    pub fn install() {
        set_hooks(&HOOKS);
    }
}
