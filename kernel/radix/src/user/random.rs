// SPDX-FileCopyrightText: 2026 The OrthrOS contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Random bytes for programs, from a generator per CPU
//! (`radix_random::Local`), so that CPUs asking at once never wait for
//! each other (docs/design/random.md).

use radix_hal::MAX_CPUS;
use radix_random::Local;

use crate::percpu::{Own, PerCpu, Slot};

/// Each CPU's generator, which only its own CPU uses.
static LOCAL: PerCpu<Own<Local>> =
    PerCpu::new([const { Slot::new(Own::new(Local::new())) }; MAX_CPUS]);

/// Fills `out` with random bytes from this CPU's generator, or from the
/// kernel's in the unlikely case that this CPU is using its own already
/// (an interrupt handler that came in during a request).
pub fn fill(out: &mut [u8]) {
    if LOCAL.with_own(|local| local.fill(out)).is_none() {
        radix_random::fill(out);
    }
}

/// Eight random words, from where [`fill`] takes them.
pub fn words() -> [u64; 8] {
    LOCAL.with_own(Local::words).unwrap_or_else(|| core::array::from_fn(|_| radix_random::u64()))
}

/// 64 random bits, from where [`fill`] takes them.
pub fn u64() -> u64 {
    LOCAL.with_own(Local::u64).unwrap_or_else(radix_random::u64)
}
