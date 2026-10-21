//! Per-CPU data: one value per CPU, each on its own cache line, so that
//! CPUs updating their own values never contend for a line.
//!
//! A CPU's own value is only meaningfully "its own" while the code using
//! it stays on that CPU: [`PerCpu::with`] holds off preemption for as long
//! as it runs. Values are reached by index from anywhere, which the
//! scheduler uses to look at, and lock, other CPUs' run queues.

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
