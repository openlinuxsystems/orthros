//! One-time initialisation: [`Once`] and [`TakeOnce`].

use core::cell::UnsafeCell;
use core::mem::MaybeUninit;
use core::sync::atomic::{AtomicBool, AtomicU8, Ordering};

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
    use std::sync::Arc;

    use super::*;

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
