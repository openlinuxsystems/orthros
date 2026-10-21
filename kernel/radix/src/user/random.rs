// SPDX-FileCopyrightText: 2026 The OrthrOS contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Random bytes for programs, from a generator per CPU
//! (`radix_random::Local`), so that CPUs asking at once never wait for
//! each other (docs/design/random.md).

use radix_hal::MAX_CPUS;
use radix_random::Local;
use radix_sync::SpinLock;

use crate::percpu::{PerCpu, Slot};

/// Each CPU's generator. Only its own CPU takes the lock, with preemption
/// off, so it is never contended: it is there because every request
/// changes the generator. No interrupt handler asks for random bytes.
static LOCAL: PerCpu<SpinLock<Local>> =
    PerCpu::new([const { Slot::new(SpinLock::new(Local::new())) }; MAX_CPUS]);

/// Fills `out` with random bytes from this CPU's generator.
pub fn fill(out: &mut [u8]) {
    LOCAL.with(|local| local.lock().fill(out));
}

/// 64 random bits from this CPU's generator.
pub fn u64() -> u64 {
    LOCAL.with(|local| local.lock().u64())
}
