// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! The time-stamp counter, for benchmarks. The kernel measured its
//! frequency and put it in the start-up page.

use core::fmt;

/// The time-stamp counter, after every earlier instruction has finished.
#[inline(always)]
pub fn cycles() -> u64 {
    let (low, high): (u32, u32);
    // SAFETY: `lfence; rdtsc` reads the counter into edx:eax and touches
    // nothing else; user mode may run it (the kernel leaves CR4.TSD clear).
    unsafe {
        core::arch::asm!(
            "lfence",
            "rdtsc",
            out("eax") low,
            out("edx") high,
            options(nomem, nostack, preserves_flags),
        );
    }
    (u64::from(high) << 32) | u64::from(low)
}

/// A time in nanoseconds with two decimals, as `ob bench` reads it.
#[derive(Clone, Copy)]
pub struct Nanos {
    /// Hundredths of a nanosecond.
    pub hundredths: u64,
}

impl Nanos {
    /// The time `cycles` take, spread over `count` repetitions.
    pub fn per(cycles: u64, count: u64) -> Nanos {
        let hz = u128::from(crate::startup().tsc_hz.max(1));
        let hundredths = u128::from(cycles) * 100_000_000_000 / hz / u128::from(count.max(1));
        Nanos { hundredths: hundredths.min(u128::from(u64::MAX)) as u64 }
    }
}

impl fmt::Display for Nanos {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}.{:02}", self.hundredths / 100, self.hundredths % 100)
    }
}
