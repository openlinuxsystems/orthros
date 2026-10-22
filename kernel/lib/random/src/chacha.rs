// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! The ChaCha20 block function (RFC 8439, section 2.3), which turns a key
//! and a block number into 64 bytes of key stream.

use crate::wipe;

/// The first four words of every block: "expand 32-byte k".
const CONSTANTS: [u32; 4] = [0x6170_7865, 0x3320_646e, 0x7962_2d32, 0x6b20_6574];

/// Block `counter` of the ChaCha20 key stream for `key` and `nonce`, laid
/// out as RFC 8439 does (a 32-bit block counter and a 96-bit nonce).
pub fn block(key: &[u32; 8], counter: u32, nonce: &[u32; 3]) -> [u8; 64] {
    let mut input = [0u32; 16];
    input[..4].copy_from_slice(&CONSTANTS);
    input[4..12].copy_from_slice(key);
    input[12] = counter;
    input[13..].copy_from_slice(nonce);
    let mut x = input;
    for _ in 0..10 {
        // A column round, then a diagonal round.
        quarter_round(&mut x, 0, 4, 8, 12);
        quarter_round(&mut x, 1, 5, 9, 13);
        quarter_round(&mut x, 2, 6, 10, 14);
        quarter_round(&mut x, 3, 7, 11, 15);
        quarter_round(&mut x, 0, 5, 10, 15);
        quarter_round(&mut x, 1, 6, 11, 12);
        quarter_round(&mut x, 2, 7, 8, 13);
        quarter_round(&mut x, 3, 4, 9, 14);
    }
    let mut out = [0u8; 64];
    for ((bytes, word), start) in out.as_chunks_mut::<4>().0.iter_mut().zip(x).zip(input) {
        *bytes = word.wrapping_add(start).to_le_bytes();
    }
    wipe(&mut x);
    wipe(&mut input);
    out
}

fn quarter_round(x: &mut [u32; 16], a: usize, b: usize, c: usize, d: usize) {
    x[a] = x[a].wrapping_add(x[b]);
    x[d] = (x[d] ^ x[a]).rotate_left(16);
    x[c] = x[c].wrapping_add(x[d]);
    x[b] = (x[b] ^ x[c]).rotate_left(12);
    x[a] = x[a].wrapping_add(x[b]);
    x[d] = (x[d] ^ x[a]).rotate_left(8);
    x[c] = x[c].wrapping_add(x[d]);
    x[b] = (x[b] ^ x[c]).rotate_left(7);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tests::{hex, key_words};

    /// RFC 8439, section 2.3.2.
    #[test]
    fn matches_rfc_8439() {
        let key = key_words(&core::array::from_fn(|i| i as u8));
        let nonce = [0x0900_0000, 0x4a00_0000, 0];
        assert_eq!(
            hex(&block(&key, 1, &nonce)),
            "10f1e7e4d13b5915500fdd1fa32071c4c7d1f4c733c068030422aa9ac3d46c4e\
             d2826446079faa0914c2d705d98b02a2b5129cd1de164eb9cbd083e8a2503c4e"
        );
    }

    /// RFC 8439, appendix A.1, test vector 1: the all-zero key and nonce.
    #[test]
    fn matches_the_all_zero_vector() {
        assert_eq!(
            hex(&block(&[0; 8], 0, &[0; 3])),
            "76b8e0ada0f13d90405d6ae55386bd28bdd219b8a08ded1aa836efcc8b770dc7\
             da41597c5157488d7724e03fb8d84a376a43b8f41518a11cc387b669b2ee6586"
        );
    }
}
