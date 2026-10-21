//! The Radix kernel's random number generator.
//!
//! One ChaCha20 generator with fast key erasure ([`Generator`]) serves the
//! whole kernel: KASLR, and from Phase 5 the `getrandom` capability that
//! every process receives. The architecture layer seeds it at the very
//! start of boot, before KASLR picks the kernel's address: it gathers
//! input from every entropy source the machine has (on x86-64, RDSEED,
//! RDRAND and the timing jitter of the time-stamp counter) into a
//! [`Pool`], which hashes all of it with BLAKE2s, and hands the pool to
//! [`seed`]. Because every source goes through the hash, the seed is
//! unpredictable as long as any one source is, so no single source has to
//! be trusted: not the CPU's generator, which cannot be audited, and not
//! the jitter, which is hard to measure.
//!
//! The [`health`] tests decide which sources the boot log says the seed
//! rests on; they never keep a source out of the pool.
//!
//! The lock around the generator does not mask interrupts, so interrupt
//! handlers must not use it (`docs/design/random.md`).

#![cfg_attr(not(test), no_std)]
#![forbid(unsafe_code)]

mod blake2s;
mod chacha;
mod generator;
pub mod health;

use core::sync::atomic::{AtomicU8, Ordering};

use radix_sync::SpinLock;

pub use blake2s::{Blake2s, hash};
pub use generator::{CHUNK, Generator};

/// The kernel's generator.
static GENERATOR: SpinLock<Generator> = SpinLock::new(Generator::new());
/// The sources the last seed rested on, as [`Sources`] bits.
static SOURCES: AtomicU8 = AtomicU8::new(0);

/// A kind of entropy source.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum Source {
    /// The CPU's entropy source read directly (x86-64 RDSEED).
    HardwareSeed = 1,
    /// The CPU's own deterministic generator (x86-64 RDRAND).
    HardwareRandom = 2,
    /// Variation in how long the same work takes, read from a cycle
    /// counter.
    Jitter = 4,
}

/// A set of [`Source`]s.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Sources(u8);

impl Sources {
    /// No sources.
    pub const NONE: Sources = Sources(0);

    /// Whether `source` is in the set.
    pub fn contains(self, source: Source) -> bool {
        self.0 & source as u8 != 0
    }

    /// Adds `source` to the set.
    pub fn insert(&mut self, source: Source) {
        self.0 |= source as u8;
    }

    /// Whether the set is empty.
    pub fn is_empty(self) -> bool {
        self.0 == 0
    }
}

/// Input gathered for a seed. Everything mixed in is hashed with BLAKE2s,
/// each input tagged with its source and length.
#[derive(Default)]
pub struct Pool {
    hash: Blake2s,
    passed: Sources,
}

impl Pool {
    /// An empty pool.
    pub const fn new() -> Pool {
        Pool { hash: Blake2s::new(), passed: Sources::NONE }
    }

    /// Mixes `bytes` from `source` into the pool.
    pub fn mix(&mut self, source: Source, bytes: &[u8]) {
        self.hash.update(&[source as u8]);
        self.hash.update(&(bytes.len() as u32).to_le_bytes());
        self.hash.update(bytes);
    }

    /// Mixes one 64-bit value from `source` into the pool.
    pub fn mix_u64(&mut self, source: Source, value: u64) {
        self.mix(source, &value.to_le_bytes());
    }

    /// Records that `source` passed its health test.
    pub fn passed(&mut self, source: Source) {
        self.passed.insert(source);
    }

    /// The sources that passed their health tests.
    pub fn sources(&self) -> Sources {
        self.passed
    }

    /// The seed: a hash of everything mixed in.
    fn finish(self) -> ([u8; 32], Sources) {
        let passed = self.passed;
        let mut hash = self.hash;
        hash.update(b"radix-random pool v1");
        (hash.finalize(), passed)
    }
}

/// Seeds the kernel's generator with `pool`, mixing it into the current
/// key, and returns the sources that passed their health tests.
pub fn seed(pool: Pool) -> Sources {
    let (mut seed, sources) = pool.finish();
    GENERATOR.lock().reseed(&seed);
    wipe(&mut seed);
    SOURCES.store(sources.0, Ordering::Relaxed);
    sources
}

/// Whether the kernel's generator has been seeded.
pub fn is_seeded() -> bool {
    GENERATOR.lock().is_seeded()
}

/// The sources the last seed rested on.
pub fn sources() -> Sources {
    Sources(SOURCES.load(Ordering::Relaxed))
}

/// Fills `out` with random bytes from the kernel's generator.
pub fn fill(out: &mut [u8]) {
    GENERATOR.lock().fill(out);
}

/// 64 random bits from the kernel's generator.
pub fn u64() -> u64 {
    GENERATOR.lock().next_u64()
}

/// A random number below `bound`, each equally likely; 0 if `bound` is 0.
pub fn below(bound: u64) -> u64 {
    if bound == 0 {
        return 0;
    }
    // Numbers below 2^64 mod bound would make the low results more likely;
    // what is left is a whole number of runs of `bound`.
    let threshold = bound.wrapping_neg() % bound;
    loop {
        let x = u64();
        if x >= threshold {
            return x % bound;
        }
    }
}

/// Overwrites a secret with zeros. The read through `black_box` keeps the
/// compiler from dropping the stores as dead.
pub(crate) fn wipe<T: Copy + Default>(secret: &mut [T]) {
    secret.fill(T::default());
    core::hint::black_box(&*secret);
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// Bytes as lowercase hexadecimal.
    pub fn hex(bytes: &[u8]) -> String {
        bytes.iter().map(|b| format!("{b:02x}")).collect()
    }

    /// A key given as bytes, as ChaCha20's words.
    pub fn key_words(bytes: &[u8; 32]) -> [u32; 8] {
        core::array::from_fn(|i| {
            u32::from_le_bytes([bytes[4 * i], bytes[4 * i + 1], bytes[4 * i + 2], bytes[4 * i + 3]])
        })
    }

    #[test]
    fn pools_depend_on_source_and_content() {
        let seed_of = |inputs: &[(Source, &[u8])]| {
            let mut pool = Pool::new();
            for &(source, bytes) in inputs {
                pool.mix(source, bytes);
            }
            pool.finish().0
        };
        let a = seed_of(&[(Source::Jitter, b"abc")]);
        assert_eq!(a, seed_of(&[(Source::Jitter, b"abc")]));
        assert_ne!(a, seed_of(&[(Source::HardwareSeed, b"abc")]));
        assert_ne!(a, seed_of(&[(Source::Jitter, b"abd")]));
        // Lengths are hashed, so inputs cannot be regrouped.
        assert_ne!(
            seed_of(&[(Source::Jitter, b"ab"), (Source::Jitter, b"c")]),
            seed_of(&[(Source::Jitter, b"a"), (Source::Jitter, b"bc")])
        );
    }

    #[test]
    fn the_kernel_generator_is_seeded_and_serves_requests() {
        let mut pool = Pool::new();
        pool.mix_u64(Source::Jitter, 0x1234);
        pool.passed(Source::Jitter);
        let sources = seed(pool);
        assert!(sources.contains(Source::Jitter));
        assert!(!sources.contains(Source::HardwareRandom));
        assert!(is_seeded());
        let (mut a, mut b) = ([0u8; 32], [0u8; 32]);
        fill(&mut a);
        fill(&mut b);
        assert_ne!(a, b);
        assert_ne!(u64(), u64());
    }

    #[test]
    fn below_stays_below_and_reaches_every_value() {
        assert_eq!(below(0), 0);
        assert_eq!(below(1), 0);
        let mut seen = [false; 7];
        for _ in 0..1000 {
            let x = below(7);
            assert!(x < 7);
            seen[x as usize] = true;
        }
        assert!(seen.iter().all(|&s| s));
        assert!(below(u64::MAX) < u64::MAX);
        assert!(below(1 << 63) < 1 << 63);
    }
}
