// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! CRC-32 as GPT checks its headers and entries with it (UEFI 2.10,
//! section 5.3.2): the reflected polynomial 0xEDB88320, from all ones,
//! and inverted at the end, as zlib's `crc32` is.

/// The remainder of each byte.
const TABLE: [u32; 256] = {
    let mut table = [0u32; 256];
    let mut i = 0;
    while i < 256 {
        let mut c = i as u32;
        let mut k = 0;
        while k < 8 {
            c = if c & 1 != 0 { 0xedb8_8320 ^ (c >> 1) } else { c >> 1 };
            k += 1;
        }
        table[i] = c;
        i += 1;
    }
    table
};

/// The CRC-32 of `bytes`.
pub fn crc32(bytes: &[u8]) -> u32 {
    update(0, bytes)
}

/// The CRC-32 of what gave `crc`, followed by `bytes`.
pub fn update(crc: u32, bytes: &[u8]) -> u32 {
    let mut c = !crc;
    for &b in bytes {
        c = TABLE[((c ^ u32::from(b)) & 0xff) as usize] ^ (c >> 8);
    }
    !c
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_values() {
        assert_eq!(crc32(b""), 0);
        assert_eq!(crc32(b"123456789"), 0xcbf4_3926);
        assert_eq!(update(crc32(b"1234"), b"56789"), 0xcbf4_3926);
    }
}
