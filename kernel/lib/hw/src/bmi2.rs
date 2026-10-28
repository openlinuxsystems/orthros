// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! BMI2's shifts, for the filesystems' decoders, which are compartments
//! and cannot run them themselves: `shlx` and `shrx` shift by a count in
//! any register, in one operation, where the shifts every x86-64
//! processor has take their count in `cl` and are two or three operations
//! on Intel's processors. The zstd decoder takes each value of a sequence
//! from its window of bits with them (ADR 0133), as Linux's does where
//! the processor has BMI2.

use core::sync::atomic::{AtomicU8, Ordering};

/// Proof that the processor has BMI2's instructions, which only
/// [`Bmi2::get`] makes, so that its shifts are safe to run.
#[derive(Clone, Copy, Debug)]
pub struct Bmi2(());

/// Whether the processor has BMI2: 0 not asked yet, 1 no, 2 yes.
static HAS: AtomicU8 = AtomicU8::new(0);

impl Bmi2 {
    /// The proof, where the processor has BMI2, asked once: CPUID can
    /// cost a trip to the hypervisor.
    pub fn get() -> Option<Bmi2> {
        let has = match HAS.load(Ordering::Relaxed) {
            1 => false,
            2 => true,
            _ => {
                let has = cpu_has();
                HAS.store(if has { 2 } else { 1 }, Ordering::Relaxed);
                has
            }
        };
        has.then_some(Bmi2(()))
    }

    /// `value` shifted left by `count` modulo 64 (`shlx`).
    #[inline(always)]
    pub fn shl(self, value: u64, count: u32) -> u64 {
        shlx(value, count)
    }

    /// `value` shifted right by `count` modulo 64 (`shrx`).
    #[inline(always)]
    pub fn shr(self, value: u64, count: u32) -> u64 {
        shrx(value, count)
    }
}

/// BMI2: CPUID leaf 7, subleaf 0, EBX bit 8, where the processor has
/// leaf 7.
#[cfg(target_arch = "x86_64")]
fn cpu_has() -> bool {
    use core::arch::x86_64::{__cpuid, __cpuid_count};
    __cpuid(0).eax >= 7 && __cpuid_count(7, 0).ebx & (1 << 8) != 0
}

#[cfg(not(target_arch = "x86_64"))]
fn cpu_has() -> bool {
    false
}

#[cfg(target_arch = "x86_64")]
#[inline(always)]
fn shlx(value: u64, count: u32) -> u64 {
    let out;
    // SAFETY: `shlx` reads and writes the three registers alone, and runs
    // only with a `Bmi2`, which `Bmi2::get` makes once CPUID found it.
    #[allow(unsafe_code)]
    unsafe {
        core::arch::asm!(
            "shlx {out}, {value}, {count}",
            out = lateout(reg) out,
            value = in(reg) value,
            count = in(reg) u64::from(count),
            options(pure, nomem, nostack, preserves_flags),
        );
    }
    out
}

#[cfg(target_arch = "x86_64")]
#[inline(always)]
fn shrx(value: u64, count: u32) -> u64 {
    let out;
    // SAFETY: as in `shlx`.
    #[allow(unsafe_code)]
    unsafe {
        core::arch::asm!(
            "shrx {out}, {value}, {count}",
            out = lateout(reg) out,
            value = in(reg) value,
            count = in(reg) u64::from(count),
            options(pure, nomem, nostack, preserves_flags),
        );
    }
    out
}

#[cfg(not(target_arch = "x86_64"))]
fn shlx(_: u64, _: u32) -> u64 {
    unreachable!("no BMI2")
}

#[cfg(not(target_arch = "x86_64"))]
fn shrx(_: u64, _: u32) -> u64 {
    unreachable!("no BMI2")
}
