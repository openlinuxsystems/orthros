// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! BLAKE2b-256 (RFC 7693), unkeyed, one of Btrfs's checksums.
//!
//! In portable Rust, as the kernel runs without SIMD registers; what it
//! hashes is on the disk for anyone to read, so it need not run in
//! constant time.

/// The initial chaining value, the same as SHA-512's.
const IV: [u64; 8] = [
    0x6a09_e667_f3bc_c908,
    0xbb67_ae85_84ca_a73b,
    0x3c6e_f372_fe94_f82b,
    0xa54f_f53a_5f1d_36f1,
    0x510e_527f_ade6_82d1,
    0x9b05_688c_2b3e_6c1f,
    0x1f83_d9ab_fb41_bd6b,
    0x5be0_cd19_137e_2179,
];

/// The order in which each round reads the message words; rounds 10 and
/// 11 repeat rounds 0 and 1.
const SIGMA: [[usize; 16]; 12] = [
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
    [0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15],
    [14, 10, 4, 8, 9, 15, 13, 6, 1, 12, 0, 2, 11, 7, 5, 3],
];

/// Bytes in a block.
const BLOCK: usize = 128;

/// `BLAKE2b-256(data)`.
pub fn hash(data: &[u8]) -> [u8; 32] {
    let mut h = IV;
    // Parameter block: digest length 32, no key, fanout 1, depth 1.
    h[0] ^= 0x0101_0000 ^ 32;
    // Every block but the last as it is; the last, which may be partial
    // (or empty, for no data), padded with zeros and compressed as last.
    let full = data.len().saturating_sub(1) / BLOCK;
    let (blocks, _) = data[..full * BLOCK].as_chunks::<BLOCK>();
    for (n, block) in blocks.iter().enumerate() {
        compress(&mut h, block, ((n + 1) * BLOCK) as u128, false);
    }
    let mut last = [0u8; BLOCK];
    let rest = &data[full * BLOCK..];
    last[..rest.len()].copy_from_slice(rest);
    compress(&mut h, &last, data.len() as u128, true);
    let mut out = [0u8; 32];
    for (bytes, word) in out.as_chunks_mut::<8>().0.iter_mut().zip(h) {
        *bytes = word.to_le_bytes();
    }
    out
}

fn compress(h: &mut [u64; 8], block: &[u8; BLOCK], length: u128, last: bool) {
    let mut m = [0u64; 16];
    for (word, bytes) in m.iter_mut().zip(block.as_chunks::<8>().0) {
        *word = u64::from_le_bytes(*bytes);
    }
    let mut v = [0u64; 16];
    v[..8].copy_from_slice(h);
    v[8..].copy_from_slice(&IV);
    v[12] ^= length as u64;
    v[13] ^= (length >> 64) as u64;
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
}

/// The mixing function G on the words at `[a, b, c, d]`.
fn g(v: &mut [u64; 16], [a, b, c, d]: [usize; 4], x: u64, y: u64) {
    v[a] = v[a].wrapping_add(v[b]).wrapping_add(x);
    v[d] = (v[d] ^ v[a]).rotate_right(32);
    v[c] = v[c].wrapping_add(v[d]);
    v[b] = (v[b] ^ v[c]).rotate_right(24);
    v[a] = v[a].wrapping_add(v[b]).wrapping_add(y);
    v[d] = (v[d] ^ v[a]).rotate_right(16);
    v[c] = v[c].wrapping_add(v[d]);
    v[b] = (v[b] ^ v[c]).rotate_right(63);
}
