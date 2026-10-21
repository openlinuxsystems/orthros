//! BLAKE2s-256 (RFC 7693), the hash that condenses everything the entropy
//! sources produce into a seed.

use crate::wipe;

/// The initial chaining value, the same as SHA-256's.
const IV: [u32; 8] = [
    0x6a09_e667,
    0xbb67_ae85,
    0x3c6e_f372,
    0xa54f_f53a,
    0x510e_527f,
    0x9b05_688c,
    0x1f83_d9ab,
    0x5be0_cd19,
];

/// The order in which each round reads the message words.
const SIGMA: [[usize; 16]; 10] = [
    [0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15],
    [14, 10, 4, 8, 9, 15, 13, 6, 1, 12, 0, 2, 11, 7, 5, 3],
    [11, 8, 12, 0, 5, 2, 15, 13, 10, 14, 3, 6, 7, 1, 9, 4],
    [7, 9, 3, 1, 13, 12, 11, 14, 2, 6, 5, 10, 4, 0, 15, 8],
    [9, 0, 5, 7, 2, 4, 10, 15, 14, 1, 11, 12, 6, 8, 3, 13],
    [2, 12, 6, 10, 0, 11, 8, 3, 4, 13, 7, 5, 15, 14, 1, 9],
    [12, 5, 1, 15, 14, 13, 4, 10, 0, 7, 6, 3, 9, 2, 8, 11],
    [13, 11, 7, 14, 12, 1, 3, 9, 5, 0, 15, 4, 8, 6, 2, 10],
    [6, 15, 14, 9, 11, 3, 0, 8, 12, 2, 13, 7, 1, 4, 10, 5],
    [10, 2, 8, 4, 7, 6, 1, 5, 15, 11, 9, 14, 3, 12, 13, 0],
];

/// Bytes in a block.
const BLOCK: usize = 64;

/// A BLAKE2s-256 hash in progress (unkeyed, with a 32-byte digest).
#[derive(Clone)]
pub struct Blake2s {
    h: [u32; 8],
    buffer: [u8; BLOCK],
    /// Bytes in `buffer`. A full buffer is compressed only when more input
    /// arrives, because the last block is compressed differently.
    filled: usize,
    /// Bytes compressed so far.
    length: u64,
}

impl Blake2s {
    /// A hash of nothing yet.
    pub const fn new() -> Blake2s {
        let mut h = IV;
        // Parameter block: digest length 32, no key, fanout 1, depth 1.
        h[0] ^= 0x0101_0000 ^ 32;
        Blake2s { h, buffer: [0; BLOCK], filled: 0, length: 0 }
    }

    /// Adds `data` to the hashed input.
    pub fn update(&mut self, mut data: &[u8]) {
        while !data.is_empty() {
            if self.filled == BLOCK {
                self.length += BLOCK as u64;
                compress(&mut self.h, &self.buffer, self.length, false);
                self.filled = 0;
            }
            let take = (BLOCK - self.filled).min(data.len());
            let (now, rest) = data.split_at(take);
            self.buffer[self.filled..self.filled + take].copy_from_slice(now);
            self.filled += take;
            data = rest;
        }
    }

    /// The digest of everything added.
    pub fn finalize(mut self) -> [u8; 32] {
        self.length += self.filled as u64;
        self.buffer[self.filled..].fill(0);
        compress(&mut self.h, &self.buffer, self.length, true);
        let mut out = [0u8; 32];
        for (bytes, word) in out.as_chunks_mut::<4>().0.iter_mut().zip(self.h) {
            *bytes = word.to_le_bytes();
        }
        self.wipe();
        out
    }

    fn wipe(&mut self) {
        wipe(&mut self.h);
        wipe(&mut self.buffer);
    }
}

impl Default for Blake2s {
    fn default() -> Blake2s {
        Blake2s::new()
    }
}

impl Drop for Blake2s {
    fn drop(&mut self) {
        self.wipe();
    }
}

/// `BLAKE2s-256(data)`.
pub fn hash(data: &[u8]) -> [u8; 32] {
    let mut hash = Blake2s::new();
    hash.update(data);
    hash.finalize()
}

fn compress(h: &mut [u32; 8], block: &[u8; BLOCK], length: u64, last: bool) {
    let mut m = [0u32; 16];
    for (word, bytes) in m.iter_mut().zip(block.as_chunks::<4>().0) {
        *word = u32::from_le_bytes(*bytes);
    }
    let mut v = [0u32; 16];
    v[..8].copy_from_slice(h);
    v[8..].copy_from_slice(&IV);
    v[12] ^= length as u32;
    v[13] ^= (length >> 32) as u32;
    if last {
        v[14] = !v[14];
    }
    for s in &SIGMA {
        g(&mut v, [0, 4, 8, 12], m[s[0]], m[s[1]]);
        g(&mut v, [1, 5, 9, 13], m[s[2]], m[s[3]]);
        g(&mut v, [2, 6, 10, 14], m[s[4]], m[s[5]]);
        g(&mut v, [3, 7, 11, 15], m[s[6]], m[s[7]]);
        g(&mut v, [0, 5, 10, 15], m[s[8]], m[s[9]]);
        g(&mut v, [1, 6, 11, 12], m[s[10]], m[s[11]]);
        g(&mut v, [2, 7, 8, 13], m[s[12]], m[s[13]]);
        g(&mut v, [3, 4, 9, 14], m[s[14]], m[s[15]]);
    }
    for (i, word) in h.iter_mut().enumerate() {
        *word ^= v[i] ^ v[i + 8];
    }
    wipe(&mut m);
    wipe(&mut v);
}

/// The mixing function G on the words at `[a, b, c, d]`.
fn g(v: &mut [u32; 16], [a, b, c, d]: [usize; 4], x: u32, y: u32) {
    v[a] = v[a].wrapping_add(v[b]).wrapping_add(x);
    v[d] = (v[d] ^ v[a]).rotate_right(16);
    v[c] = v[c].wrapping_add(v[d]);
    v[b] = (v[b] ^ v[c]).rotate_right(12);
    v[a] = v[a].wrapping_add(v[b]).wrapping_add(y);
    v[d] = (v[d] ^ v[a]).rotate_right(8);
    v[c] = v[c].wrapping_add(v[d]);
    v[b] = (v[b] ^ v[c]).rotate_right(7);
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::*;
    use crate::tests::hex;

    /// Bytes 0, 1, 2, ... wrapping at 256.
    fn counting(len: usize) -> Vec<u8> {
        (0..len).map(|i| i as u8).collect()
    }

    /// RFC 7693, appendix B, and digests of messages around the block
    /// size from Python's `hashlib.blake2s`.
    #[test]
    fn matches_known_digests() {
        assert_eq!(
            hex(&hash(b"abc")),
            "508c5e8c327c14e2e1a72ba34eeb452f37458b209ed63a294d999b4c86675982"
        );
        assert_eq!(
            hex(&hash(b"")),
            "69217a3079908094e11121d042354a7c1f55b6482ca1a51e1b250dfd1ed0eef9"
        );
        for (len, digest) in [
            (63, "e57cb79487dd57902432b250733813bd96a84efce59f650fac26e6696aefafc3"),
            (64, "56f34e8b96557e90c1f24b52d0c89d51086acf1b00f634cf1dde9233b8eaaa3e"),
            (65, "1b53ee94aaf34e4b159d48de352c7f0661d0a40edff95a0b1639b4090e974472"),
            (1000, "b5f9d7799111edafc9326fbf667be98140b5e20ce5e151793c59125bf654ac18"),
        ] {
            assert_eq!(hex(&hash(&counting(len))), digest, "{len} bytes");
        }
    }

    proptest! {
        /// Feeding the input in pieces gives the digest of the whole.
        #[test]
        fn pieces_hash_like_the_whole(
            data in proptest::collection::vec(any::<u8>(), 0..300),
            cuts in proptest::collection::vec(0usize..300, 0..6),
        ) {
            let mut cuts: Vec<usize> = cuts.into_iter().map(|c| c.min(data.len())).collect();
            cuts.sort_unstable();
            let mut hash = Blake2s::new();
            let mut start = 0;
            for cut in cuts {
                hash.update(&data[start..cut]);
                start = cut;
            }
            hash.update(&data[start..]);
            prop_assert_eq!(hash.finalize(), super::hash(&data));
        }
    }
}
