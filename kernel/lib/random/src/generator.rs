// SPDX-FileCopyrightText: 2026 The OrthrOS contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! The generator: ChaCha20 with fast key erasure.
//!
//! Each request runs ChaCha20 under the current key. The first 32 bytes of
//! the key stream become the next key and are never output; the request is
//! served from the rest. Once a request returns, the key that made its
//! bytes is gone, so a later look at the generator's memory cannot recover
//! them (forward secrecy). The design is Bernstein's "fast-key-erasure
//! random-number generators" (2017), which Linux and the BSDs also follow.

use crate::blake2s::Blake2s;
use crate::{chacha, wipe};

/// The nonce. Every key makes at most [`CHUNK`] bytes before it is
/// replaced, so a fixed nonce never repeats a (key, block) pair.
const NONCE: [u32; 3] = [0; 3];

/// The most bytes one key makes. A longer request takes a new key after
/// each `CHUNK` bytes.
pub const CHUNK: usize = 4096;

/// Separates the generator's reseeding hash from any other use of BLAKE2s.
const RESEED_LABEL: &[u8] = b"radix-random reseed v1";

/// A ChaCha20 generator with fast key erasure.
pub struct Generator {
    key: [u32; 8],
    reseeds: u32,
}

impl Generator {
    /// An unseeded generator. Its output is predictable until
    /// [`Generator::reseed`] gives it a seed.
    pub const fn new() -> Generator {
        Generator { key: [0; 8], reseeds: 0 }
    }

    /// Mixes `seed` into the key. The new key is a hash of the old key and
    /// the seed, so it is unpredictable if either of them is.
    pub fn reseed(&mut self, seed: &[u8; 32]) {
        let mut hash = Blake2s::new();
        hash.update(RESEED_LABEL);
        let mut old = [0u8; 32];
        for (bytes, word) in old.as_chunks_mut::<4>().0.iter_mut().zip(self.key) {
            *bytes = word.to_le_bytes();
        }
        hash.update(&old);
        hash.update(seed);
        let mut new = hash.finalize();
        self.key = key_words(&new);
        wipe(&mut old);
        wipe(&mut new);
        self.reseeds = self.reseeds.saturating_add(1);
    }

    /// Whether the generator has been seeded.
    pub fn is_seeded(&self) -> bool {
        self.reseeds > 0
    }

    /// How many times it has been seeded.
    pub fn reseeds(&self) -> u32 {
        self.reseeds
    }

    /// Fills `out` with random bytes and replaces the key.
    pub fn fill(&mut self, out: &mut [u8]) {
        for chunk in out.chunks_mut(CHUNK) {
            let mut first = chacha::block(&self.key, 0, &NONCE);
            let (head, tail) = chunk.split_at_mut(chunk.len().min(32));
            head.copy_from_slice(&first[32..32 + head.len()]);
            for (counter, piece) in (1..).zip(tail.chunks_mut(64)) {
                let mut block = chacha::block(&self.key, counter, &NONCE);
                piece.copy_from_slice(&block[..piece.len()]);
                wipe(&mut block);
            }
            let mut next = [0u8; 32];
            next.copy_from_slice(&first[..32]);
            self.key = key_words(&next);
            wipe(&mut next);
            wipe(&mut first);
        }
    }

    /// 64 random bits.
    pub fn next_u64(&mut self) -> u64 {
        let mut bytes = [0u8; 8];
        self.fill(&mut bytes);
        u64::from_le_bytes(bytes)
    }
}

impl Default for Generator {
    fn default() -> Generator {
        Generator::new()
    }
}

impl Drop for Generator {
    fn drop(&mut self) {
        wipe(&mut self.key);
    }
}

/// A 32-byte key as ChaCha20's eight little-endian words.
fn key_words(bytes: &[u8; 32]) -> [u32; 8] {
    let mut key = [0u32; 8];
    for (word, chunk) in key.iter_mut().zip(bytes.as_chunks::<4>().0) {
        *word = u32::from_le_bytes(*chunk);
    }
    key
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::*;
    use crate::tests::hex;

    /// A generator seeded once with 32 bytes of 0x42.
    fn seeded() -> Generator {
        let mut generator = Generator::new();
        generator.reseed(&[0x42; 32]);
        generator
    }

    /// The output is ChaCha20's key stream after its first 32 bytes, and
    /// the next request runs under those 32 bytes as its key. The expected
    /// bytes come from Python's `hashlib.blake2s` and OpenSSL's ChaCha20.
    #[test]
    fn matches_known_output() {
        let mut generator = seeded();
        assert!(generator.is_seeded());
        let mut out = [0u8; 100];
        generator.fill(&mut out);
        assert_eq!(hex(&out), KNOWN_FIRST);
        let mut out = [0u8; 8];
        generator.fill(&mut out);
        assert_eq!(hex(&out), KNOWN_SECOND);
    }

    const KNOWN_FIRST: &str = "82eb20dc13a984c0cb41a8506f4279ed2a771f17200603aefb53f6fe4ba7d5b8\
                               f1eda7e7c7e820695f24d119abe16829e23d64609bbbfeb3df201904661d0765\
                               13c456c8324dd6898e8f746f6728add5dcc3f1d2464cd123f1de72a9f9ca8ed4\
                               1b6cb69e";
    const KNOWN_SECOND: &str = "90dd73fe3b2bc10f";

    #[test]
    fn long_requests_change_key_every_chunk() {
        // Two requests of one chunk each give the same bytes as one request
        // of two chunks.
        let mut one = seeded();
        let mut whole = vec![0u8; 2 * CHUNK];
        one.fill(&mut whole);
        let mut two = seeded();
        let (first, second) = whole.split_at(CHUNK);
        let mut out = vec![0u8; CHUNK];
        two.fill(&mut out);
        assert_eq!(out, first);
        two.fill(&mut out);
        assert_eq!(out, second);
        assert_eq!(one.next_u64(), two.next_u64());
    }

    #[test]
    fn an_empty_request_changes_nothing() {
        let (mut a, mut b) = (seeded(), seeded());
        a.fill(&mut []);
        assert_eq!(a.next_u64(), b.next_u64());
    }

    proptest! {
        /// Generators seeded alike agree; a different seed, or one more
        /// seed, gives different output.
        #[test]
        fn output_depends_on_every_seed(
            seeds in proptest::collection::vec(any::<[u8; 32]>(), 1..4),
            other in any::<[u8; 32]>(),
            len in 1usize..200,
        ) {
            let make = |seeds: &[[u8; 32]]| {
                let mut generator = Generator::new();
                for seed in seeds {
                    generator.reseed(seed);
                }
                let mut out = vec![0u8; len.max(16)];
                generator.fill(&mut out);
                out
            };
            prop_assert_eq!(make(&seeds), make(&seeds));
            let mut more = seeds.clone();
            more.push(other);
            prop_assert_ne!(make(&seeds), make(&more));
            if other != seeds[0] {
                let mut changed = seeds.clone();
                changed[0] = other;
                prop_assert_ne!(make(&seeds), make(&changed));
            }
        }

        /// Requests never repeat output: no 16-byte window of one request
        /// appears in the next.
        #[test]
        fn requests_do_not_repeat(seed in any::<[u8; 32]>()) {
            let mut generator = Generator::new();
            generator.reseed(&seed);
            let (mut a, mut b) = ([0u8; 64], [0u8; 64]);
            generator.fill(&mut a);
            generator.fill(&mut b);
            for window in a.windows(16) {
                prop_assert!(!b.windows(16).any(|w| w == window));
            }
        }
    }
}
