// SPDX-FileCopyrightText: 2026 The OrthrOS contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Per-CPU data: one value per CPU, each on its own cache line, so that
//! CPUs updating their own values never contend for a line.
//!
//! A CPU's own value is only meaningfully "its own" while the code using
//! it stays on that CPU: [`PerCpu::with`] holds off preemption for as long
//! as it runs. Values are reached by index from anywhere, which the
//! scheduler uses to look at, and lock, other CPUs' run queues.
//!
//! A value only its own CPU ever uses needs no lock: an [`Own`] value is
//! reached only through [`PerCpu::with_own`], on the CPU it belongs to.

use core::cell::UnsafeCell;
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
    /// inside [`PerCpu::with`].
    pub fn this(&self) -> &T {
        self.get(Arch::current_cpu())
    }

    /// Runs `f` on this CPU's value with preemption held off, so the code
    /// stays on this CPU until `f` returns.
    pub fn with<R>(&self, f: impl FnOnce(&T) -> R) -> R {
        Arch::preempt_disable();
        let result = f(self.this());
        crate::sched::preempt_enable();
        result
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

// SAFETY: the value is only reached through `PerCpu::with_own`, on the CPU
// it belongs to, with preemption held off and `busy` keeping out an
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
    /// Runs `f` on this CPU's value with preemption held off, or returns
    /// `None` without running it if this CPU is using the value already
    /// (an interrupt handler that came in during a use).
    pub fn with_own<R>(&self, f: impl FnOnce(&mut T) -> R) -> Option<R> {
        Arch::preempt_disable();
        let own = self.this();
        let result = if own.busy.load(Ordering::Relaxed) {
            None
        } else {
            own.busy.store(true, Ordering::Relaxed);
            // Only this CPU looks at `busy`, so keeping the compiler from
            // moving the value's accesses out from between the two stores
            // is all the ordering needed.
            compiler_fence(Ordering::SeqCst);
            // SAFETY: preemption is off, so this code stays on this CPU and
            // no other thread runs on it until `f` returns; no other CPU
            // reaches this CPU's value; and `busy` was clear, so no use on
            // this CPU is under way that this one interrupted.
            #[allow(unsafe_code)]
            let value = unsafe { &mut *own.value.get() };
            let result = f(value);
            compiler_fence(Ordering::SeqCst);
            own.busy.store(false, Ordering::Relaxed);
            Some(result)
        };
        crate::sched::preempt_enable();
        result
    }
}
