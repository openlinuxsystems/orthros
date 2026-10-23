// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Per-CPU data: one value per CPU, each on its own cache line, so that
//! CPUs updating their own values never contend for a line.
//!
//! A CPU's own value is only meaningfully "its own" while the code using
//! it stays on that CPU: with interrupts off, a spin lock held or
//! preemption held off. Values are reached by index from anywhere, which
//! the scheduler uses to look at, and lock, other CPUs' run queues.
//!
//! A value only its own CPU ever uses needs no lock: an [`Own`] value is
//! reached only through [`PerCpu::own`] and [`PerCpu::with_own`], on the
//! CPU it belongs to.

use core::cell::UnsafeCell;
use core::marker::PhantomData;
use core::ops::{Deref, DerefMut};
use core::sync::atomic::{AtomicBool, Ordering, compiler_fence};

use radix_hal::{CpuLocal, MAX_CPUS};

use crate::Arch;

/// One CPU's value, alone on its cache line.
#[repr(align(64))]
pub struct Slot<T>(T);

impl<T> Slot<T> {
    /// Wraps `value`.
    pub const fn new(value: T) -> Slot<T> {
        Slot(value)
    }
}

/// A value of `T` for every possible CPU.
pub struct PerCpu<T> {
    slots: [Slot<T>; MAX_CPUS],
}

impl<T> PerCpu<T> {
    /// Per-CPU data from its slots, built with
    /// `[const { Slot::new(..) }; MAX_CPUS]`.
    pub const fn new(slots: [Slot<T>; MAX_CPUS]) -> PerCpu<T> {
        PerCpu { slots }
    }

    /// The value of CPU `cpu`. Panics if `cpu` is not below `MAX_CPUS`.
    pub fn get(&self, cpu: usize) -> &T {
        &self.slots[cpu].0
    }

    /// The value of the CPU running this code. The caller must not move to
    /// another CPU while it uses it: interrupts off, a spin lock held, or
    /// preemption held off.
    pub fn this(&self) -> &T {
        self.get(Arch::current_cpu())
    }
}

/// A value that only its own CPU uses, so it needs no lock and no atomic
/// read-modify-write: see [`PerCpu::with_own`].
pub struct Own<T> {
    /// Set while this CPU uses the value. Only this CPU reads or writes it,
    /// so plain loads and stores do: an interrupt that comes in while it is
    /// set finds it set, and one that comes in before it is set is over
    /// before the code it interrupted goes on.
    busy: AtomicBool,
    value: UnsafeCell<T>,
}

// SAFETY: the value is only reached through `PerCpu::own`, on the CPU it
// belongs to, with preemption held off and `busy` keeping out an
// interrupt handler on the same CPU, so it is never used from two places
// at once. It may be used by any thread that runs on that CPU, hence
// `T: Send`.
#[allow(unsafe_code)]
unsafe impl<T: Send> Sync for Own<T> {}

impl<T> Own<T> {
    /// Wraps `value`.
    pub const fn new(value: T) -> Own<T> {
        Own { busy: AtomicBool::new(false), value: UnsafeCell::new(value) }
    }
}

impl<T> PerCpu<Own<T>> {
    /// This CPU's value, held with preemption off until the guard goes, or
    /// `None` if this CPU is using the value already (an interrupt handler
    /// that came in during a use). The holder must not sleep.
    #[inline(always)]
    pub fn own(&self) -> Option<OwnGuard<'_, T>> {
        Arch::preempt_disable();
        let own = self.this();
        if own.busy.load(Ordering::Relaxed) {
            crate::sched::preempt_enable();
            return None;
        }
        own.busy.store(true, Ordering::Relaxed);
        // Only this CPU looks at `busy`, so keeping the compiler from
        // moving the value's accesses out from between the two stores is
        // all the ordering needed.
        compiler_fence(Ordering::SeqCst);
        Some(OwnGuard { own, _here: PhantomData })
    }

    /// Runs `f` on this CPU's value with preemption held off, or returns
    /// `None` without running it if this CPU is using the value already,
    /// as [`PerCpu::own`].
    #[inline(always)]
    pub fn with_own<R>(&self, f: impl FnOnce(&mut T) -> R) -> Option<R> {
        let mut guard = self.own()?;
        Some(f(&mut guard))
    }
}

/// This CPU's value of a [`PerCpu<Own<T>>`], from [`PerCpu::own`].
pub struct OwnGuard<'a, T> {
    own: &'a Own<T>,
    /// Not `Send`: the value is this CPU's, and preemption is off.
    _here: PhantomData<*const ()>,
}

impl<T> Deref for OwnGuard<'_, T> {
    type Target = T;

    #[inline(always)]
    fn deref(&self) -> &T {
        // SAFETY: as in `deref_mut`, and `&self` is shared only with other
        // shared uses of this guard.
        #[allow(unsafe_code)]
        unsafe {
            &*self.own.value.get()
        }
    }
}

impl<T> DerefMut for OwnGuard<'_, T> {
    #[inline(always)]
    fn deref_mut(&mut self) -> &mut T {
        // SAFETY: preemption is off while the guard lives, so this code
        // stays on this CPU and no other thread runs on it; no other CPU
        // reaches this CPU's value; and `busy` was clear when the guard was
        // made and is set until it goes, so no other use on this CPU, the
        // one this guard interrupted or one interrupting it, is under way.
        // The guard is not `Send`, and `&mut self` borrows it alone.
        #[allow(unsafe_code)]
        unsafe {
            &mut *self.own.value.get()
        }
    }
}

impl<T> Drop for OwnGuard<'_, T> {
    #[inline(always)]
    fn drop(&mut self) {
        compiler_fence(Ordering::SeqCst);
        self.own.busy.store(false, Ordering::Relaxed);
        crate::sched::preempt_enable();
    }
}
