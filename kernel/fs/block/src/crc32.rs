// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! CRC-32 as GPT uses it (UEFI 2.10, section 5.3.2): the IEEE 802.3
//! polynomial, reflected, starting from and finished with all ones (the
//! same CRC as zlib's and PNG's).

/// The reflected polynomial.
const POLY: u32 = 0xedb8_8320;

/// The CRC of every byte value, worked out at compile time.
const TABLE: [u32; 256] = {
    let mut table = [0u32; 256];
    let mut i = 0;
    while i < 256 {
        let mut crc = i as u32;
        let mut bit = 0;
        while bit < 8 {
            crc = if crc & 1 != 0 { (crc >> 1) ^ POLY } else { crc >> 1 };
            bit += 1;
        }
        table[i] = crc;
        i += 1;
    }
    table
};

/// A CRC-32 being worked out over several pieces.
#[derive(Clone, Copy, Debug)]
pub struct Crc32(u32);

impl Default for Crc32 {
    fn default() -> Self {
        Crc32::new()
    }
}

impl Crc32 {
    /// A CRC of nothing yet.
    pub const fn new() -> Crc32 {
        Crc32(!0)
    }

    /// Adds `bytes`.
    pub fn update(&mut self, bytes: &[u8]) {
        let mut crc = self.0;
        for &byte in bytes {
            crc = TABLE[((crc ^ u32::from(byte)) & 0xff) as usize] ^ (crc >> 8);
        }
        self.0 = crc;
    }

    /// The CRC of everything added.
    pub fn finish(self) -> u32 {
        !self.0
    }
}

/// The CRC-32 of `bytes`.
pub fn crc32(bytes: &[u8]) -> u32 {
    let mut crc = Crc32::new();
    crc.update(bytes);
    crc.finish()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_values() {
        assert_eq!(crc32(b""), 0);
        // The check value of CRC-32/ISO-HDLC.
        assert_eq!(crc32(b"123456789"), 0xcbf4_3926);
        let mut split = Crc32::new();
        split.update(b"1234");
        split.update(b"56789");
        assert_eq!(split.finish(), 0xcbf4_3926);
    }
}
