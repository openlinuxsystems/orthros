// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Seeding the kernel's random number generator (`radix-random`) from the
//! CPU: RDSEED, RDRAND, and the timing jitter of the time-stamp counter.
//!
//! [`seed`] runs first thing when the boot trampoline calls
//! `radix_kaslr_choose`, so the generator is ready before KASLR picks the
//! kernel's address. That is before the kernel has moved, so this module
//! stores no pointers and cannot panic, like `kaslr.rs`.

use core::arch::asm;
use core::fmt;
use core::sync::atomic::{AtomicU8, AtomicU32, AtomicU64, Ordering};

use radix_random::health::{JitterTest, WordTest};
use radix_random::{Pool, Source};

use crate::cpu;

/// Words taken from each hardware generator: 256 bits.
const WORDS: u32 = 4;
/// Timing samples taken.
const SAMPLES: u32 = 4096;
/// Words of memory the jitter workload walks: 32 KiB, as large as a
/// typical first-level data cache, so its accesses hit and miss in ways
/// that vary.
const SCRATCH_WORDS: usize = 4096;

static SCRATCH: [AtomicU64; SCRATCH_WORDS] = [const { AtomicU64::new(0) }; SCRATCH_WORDS];

/// What became of one source at boot.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum Outcome {
    /// The CPU does not have it.
    Missing = 0,
    /// It failed its health test (it is still mixed in).
    Failed = 1,
    /// It passed its health test.
    Passed = 2,
}

impl Outcome {
    fn from_u8(value: u8) -> Outcome {
        match value {
            2 => Outcome::Passed,
            1 => Outcome::Failed,
            _ => Outcome::Missing,
        }
    }
}

static RDSEED: AtomicU8 = AtomicU8::new(Outcome::Missing as u8);
static RDRAND: AtomicU8 = AtomicU8::new(Outcome::Missing as u8);
static JITTER: AtomicU8 = AtomicU8::new(Outcome::Missing as u8);
static STUCK: AtomicU32 = AtomicU32::new(0);

/// Gathers entropy from every source and seeds the kernel's generator.
pub fn seed() {
    let mut pool = Pool::new();
    let rdseed = hardware(&mut pool, Source::HardwareSeed, cpu::feature::rdseed(), cpu::rdseed);
    let rdrand = hardware(&mut pool, Source::HardwareRandom, cpu::feature::rdrand(), cpu::rdrand);
    let jitter = jitter(&mut pool);
    RDSEED.store(rdseed as u8, Ordering::Relaxed);
    RDRAND.store(rdrand as u8, Ordering::Relaxed);
    let outcome = if jitter.passed() { Outcome::Passed } else { Outcome::Failed };
    JITTER.store(outcome as u8, Ordering::Relaxed);
    STUCK.store(jitter.stuck(), Ordering::Relaxed);
    radix_random::seed(pool);
}

/// Mixes [`WORDS`] words from a hardware generator into `pool`.
fn hardware(
    pool: &mut Pool,
    source: Source,
    present: bool,
    read: impl Fn() -> Option<u64>,
) -> Outcome {
    if !present {
        return Outcome::Missing;
    }
    let mut test = WordTest::new();
    for _ in 0..WORDS {
        match read() {
            Some(word) => {
                test.check(word);
                pool.mix_u64(source, word);
            }
            None => test.miss(),
        }
    }
    if test.passed(WORDS) {
        pool.passed(source);
        Outcome::Passed
    } else {
        Outcome::Failed
    }
}

/// Times [`SAMPLES`] runs of a small workload and mixes every reading of
/// the counter into `pool`. How long the work takes varies with the state
/// of the caches, the TLB and the pipeline, and on real hardware with the
/// memory bus and the clock domains; each run's address depends on the
/// previous timing, so the variation feeds on itself.
fn jitter(pool: &mut Pool) -> JitterTest {
    let mut test = JitterTest::new();
    let mut previous = timestamp();
    let mut index = 0;
    for _ in 0..SAMPLES {
        for step in 0..8 {
            index = (index + 1 + ((previous as usize >> step) & 0xff) * 8) % SCRATCH_WORDS;
            let word = SCRATCH[index].load(Ordering::Relaxed);
            SCRATCH[index].store(word.rotate_left(7) ^ previous, Ordering::Relaxed);
        }
        let now = timestamp();
        test.sample(now.wrapping_sub(previous));
        pool.mix_u64(Source::Jitter, now);
        previous = now;
    }
    if test.passed() {
        pool.passed(Source::Jitter);
    }
    test
}

/// The time-stamp counter, read after every earlier instruction has
/// finished (LFENCE), and a barrier the compiler does not move memory
/// accesses across.
fn timestamp() -> u64 {
    let (low, high): (u32, u32);
    // SAFETY: LFENCE and RDTSC only order execution and read a counter.
    unsafe { asm!("lfence", "rdtsc", out("eax") low, out("edx") high, options(nostack)) };
    (u64::from(high) << 32) | u64::from(low)
}

/// The boot log's account of the seed, as one line after "Random: ".
pub struct Summary;

impl fmt::Display for Summary {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let outcomes = [
            ("RDSEED", Outcome::from_u8(RDSEED.load(Ordering::Relaxed))),
            ("RDRAND", Outcome::from_u8(RDRAND.load(Ordering::Relaxed))),
            ("timing jitter", Outcome::from_u8(JITTER.load(Ordering::Relaxed))),
        ];
        let stuck = STUCK.load(Ordering::Relaxed);
        // The sources with outcome `wanted`, as "A", "A and B" or "A, B and
        // C" (with `last` in place of "and").
        let named = |f: &mut fmt::Formatter<'_>, wanted: Outcome, last: &str| -> fmt::Result {
            let names = outcomes.iter().filter(|(_, outcome)| *outcome == wanted);
            let count = names.clone().count();
            for (i, (name, _)) in names.enumerate() {
                let joint = match (i, count - i) {
                    (0, _) => "",
                    (_, 1) => last,
                    _ => ", ",
                };
                write!(f, "{joint}{name}")?;
                if *name == "timing jitter" {
                    write!(f, " ({SAMPLES} samples, {stuck} stuck)")?;
                }
            }
            Ok(())
        };
        if outcomes.iter().any(|(_, outcome)| *outcome == Outcome::Passed) {
            f.write_str("ChaCha20 seeded before KASLR from ")?;
            named(f, Outcome::Passed, " and ")?;
        } else {
            f.write_str("ChaCha20 seeded before KASLR, but no source passed its health test")?;
        }
        if outcomes.iter().any(|(_, outcome)| *outcome == Outcome::Failed) {
            f.write_str("; failed its health test: ")?;
            named(f, Outcome::Failed, " and ")?;
        }
        if outcomes.iter().any(|(_, outcome)| *outcome == Outcome::Missing) {
            f.write_str("; the CPU has no ")?;
            named(f, Outcome::Missing, " or ")?;
        }
        Ok(())
    }
}
