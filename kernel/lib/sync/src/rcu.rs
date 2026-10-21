//! Read-copy-update: readers that never wait or write shared memory.
//!
//! A read section ([`read_lock`], or [`RcuCell::read`]) holds off
//! preemption and nothing else, so it costs two per-CPU increments. A
//! writer publishes a new version of the data with one atomic swap and
//! frees the old one only after a *grace period*: once every read section
//! that might still see it has ended ([`synchronize`]). Radix's RCU is
//! quiescent-state based (QSBR): a CPU that has switched threads, or gone
//! idle, since the grace period started cannot be inside an older read
//! section, because read sections cannot be preempted. The kernel's
//! scheduler drives grace periods ([`Hooks::synchronize_rcu`]); see
//! `docs/design/scheduler.md`.
//!
//! Read sections must not sleep: they hold off preemption, and the
//! scheduler reports a thread that sleeps inside one.

use alloc::boxed::Box;
use core::alloc::Allocator;
use core::marker::PhantomData;
use core::ptr::NonNull;
use core::sync::atomic::{AtomicPtr, Ordering};

use crate::hooks::{self, Hooks};

/// An RCU read section; it ends when the guard is dropped. It stays on the
/// CPU it started on.
pub struct ReadGuard {
    hooks: Option<&'static dyn Hooks>,
    _not_send: PhantomData<*const ()>,
}

/// Starts a read section.
pub fn read_lock() -> ReadGuard {
    let hooks = hooks::active();
    if let Some(hooks) = hooks {
        hooks.preempt_disable();
    }
    ReadGuard { hooks, _not_send: PhantomData }
}

impl Drop for ReadGuard {
    fn drop(&mut self) {
        if let Some(hooks) = self.hooks {
            hooks.preempt_enable();
        }
    }
}

/// Waits until every read section that was running when it was called has
/// ended. It sleeps, so it may not be called inside a read section or with
/// a spin lock held. Before the kernel registers its hooks only the boot
/// CPU runs, so there is nothing to wait for.
pub fn synchronize() {
    if let Some(hooks) = hooks::active() {
        hooks.synchronize_rcu();
    }
}

/// A heap-allocated value that readers reach without locks and writers
/// replace whole.
pub struct RcuCell<T, A: Allocator + Clone> {
    ptr: AtomicPtr<T>,
    alloc: A,
}

// SAFETY: readers on any CPU get `&T` (so `T: Sync`), and the value is
// dropped by whichever CPU replaces or drops the cell (so `T: Send`).
unsafe impl<T: Send + Sync, A: Allocator + Clone + Send + Sync> Sync for RcuCell<T, A> {}
// SAFETY: as above.
unsafe impl<T: Send + Sync, A: Allocator + Clone + Send> Send for RcuCell<T, A> {}

impl<T, A: Allocator + Clone> RcuCell<T, A> {
    /// A cell holding `value`.
    pub fn new(value: Box<T, A>) -> Self {
        let (ptr, alloc) = Box::into_raw_with_allocator(value);
        Self { ptr: AtomicPtr::new(ptr), alloc }
    }

    /// Runs `f` on the current value inside a read section. `f` must not
    /// sleep.
    pub fn read<R>(&self, f: impl FnOnce(&T) -> R) -> R {
        let _section = read_lock();
        let ptr =
            NonNull::new(self.ptr.load(Ordering::Acquire)).expect("an RCU cell holds a value");
        // SAFETY: the pointer came from `Box::into_raw_with_allocator` and
        // is freed only after it has been swapped out and a grace period
        // has passed (`replace`) or the cell is gone (`drop`, which `&self`
        // rules out). This read section started before any such swap could
        // complete its grace period, so the value outlives `f`, and the
        // reference cannot escape `f`.
        f(unsafe { ptr.as_ref() })
    }

    /// Publishes `value` and returns the old value once no reader can see
    /// it any more. It waits for a grace period, so it sleeps: not inside a
    /// read section, and not with a spin lock held.
    pub fn replace(&self, value: Box<T, A>) -> Box<T, A> {
        let (new, _) = Box::into_raw_with_allocator(value);
        let old = self.ptr.swap(new, Ordering::AcqRel);
        synchronize();
        // SAFETY: `old` came from `Box::into_raw_with_allocator` with this
        // cell's allocator (every value is boxed with a clone of it, see
        // `new`), the swap made this the only caller holding it, and the
        // grace period means no reader still has a reference to it.
        unsafe { Box::from_raw_in(old, self.alloc.clone()) }
    }

    /// The current value, through `&mut`, which proves no reader or writer
    /// is active.
    pub fn get_mut(&mut self) -> &mut T {
        // SAFETY: the pointer always holds a live box (see `read`), and
        // `&mut self` rules out every other access.
        unsafe { &mut *self.ptr.load(Ordering::Relaxed) }
    }
}

impl<T, A: Allocator + Clone> Drop for RcuCell<T, A> {
    fn drop(&mut self) {
        let ptr = *self.ptr.get_mut();
        // SAFETY: `&mut self` means no reader can hold a reference (they
        // borrow the cell), and the pointer is a live box from this cell's
        // allocator.
        drop(unsafe { Box::from_raw_in(ptr, self.alloc.clone()) });
    }
}

#[cfg(test)]
mod tests {
    use std::alloc::Global;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, AtomicU64};

    use super::*;

    /// A value whose two halves must always agree; dropping it poisons
    /// them, so a reader that sees a freed value notices.
    struct Pair {
        a: AtomicU64,
        b: AtomicU64,
    }

    impl Drop for Pair {
        fn drop(&mut self) {
            self.a.store(u64::MAX, Ordering::SeqCst);
            self.b.store(0, Ordering::SeqCst);
        }
    }

    fn pair(n: u64) -> Box<Pair, Global> {
        Box::new_in(Pair { a: AtomicU64::new(n), b: AtomicU64::new(n) }, Global)
    }

    #[test]
    fn readers_never_see_freed_values() {
        crate::testing::install();
        let cell = Arc::new(RcuCell::new(pair(0)));
        let stop = Arc::new(AtomicBool::new(false));
        let started = Arc::new(AtomicU64::new(0));
        let readers: Vec<_> = (0..3)
            .map(|_| {
                let (cell, stop) = (Arc::clone(&cell), Arc::clone(&stop));
                let started = Arc::clone(&started);
                std::thread::spawn(move || {
                    let mut reads = 0u64;
                    started.fetch_add(1, Ordering::SeqCst);
                    loop {
                        cell.read(|p| {
                            let a = p.a.load(Ordering::SeqCst);
                            std::thread::yield_now();
                            let b = p.b.load(Ordering::SeqCst);
                            assert_eq!(a, b, "a reader saw a value freed under it");
                        });
                        reads += 1;
                        if stop.load(Ordering::Relaxed) {
                            break reads;
                        }
                    }
                })
            })
            .collect();
        while started.load(Ordering::SeqCst) < 3 {
            std::thread::yield_now();
        }
        for n in 1..200 {
            let old = cell.replace(pair(n));
            assert_eq!(old.a.load(Ordering::SeqCst), n - 1);
        }
        stop.store(true, Ordering::Relaxed);
        let reads: u64 = readers.into_iter().map(|r| r.join().unwrap()).sum();
        assert!(reads >= 3);
        cell.read(|p| assert_eq!(p.a.load(Ordering::SeqCst), 199));
    }

    #[test]
    fn get_mut_and_drop() {
        let mut cell = RcuCell::new(pair(5));
        *cell.get_mut().a.get_mut() = 6;
        cell.read(|p| assert_eq!(p.a.load(Ordering::Relaxed), 6));
    }
}
