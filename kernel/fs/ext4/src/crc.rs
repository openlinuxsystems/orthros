// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! The checksums ext4 and its journal use.
//!
//! - CRC-32C (the Castagnoli polynomial, reflected) covers the metadata of
//!   a volume with `metadata_csum` and the journal's blocks. ext4 keeps
//!   the register as it is, with neither the first nor the last
//!   inversion: each checksum starts from a seed (`!0`, or the volume's
//!   seed) and is stored as the register ends.
//! - CRC-16 (ANSI, reflected, `0xa001`) covers the group descriptors of a
//!   volume with `uninit_bg` (`gdt_csum`) but without `metadata_csum`.

/// The CRC-32C register `crc` after `data`, without inversions. The
/// processor's instruction works it out where it has one
/// ([`radix_hw::crc32c`]); Kani's proofs take the table.
pub fn crc32c(crc: u32, data: &[u8]) -> u32 {
    #[cfg(not(kani))]
    if let Some(sum) = radix_hw::crc32c::crc32c(crc, data) {
        return sum;
    }
    crc32c_table(crc, data)
}

/// [`crc32c`] a byte at a time with the table.
pub fn crc32c_table(crc: u32, data: &[u8]) -> u32 {
    let mut crc = crc;
    for &byte in data {
        crc = (crc >> 8) ^ CRC32C[((crc ^ u32::from(byte)) & 0xff) as usize];
    }
    crc
}

/// The CRC-16 register `crc` after `data`, as Linux's `crc16` keeps it.
pub fn crc16(crc: u16, data: &[u8]) -> u16 {
    let mut crc = crc;
    for &byte in data {
        crc = (crc >> 8) ^ CRC16[((crc ^ u16::from(byte)) & 0xff) as usize];
    }
    crc
}

const CRC32C: [u32; 256] = {
    let mut table = [0u32; 256];
    let mut i = 0;
    while i < 256 {
        let mut crc = i as u32;
        let mut bit = 0;
        while bit < 8 {
            crc = if crc & 1 != 0 { (crc >> 1) ^ 0x82f6_3b78 } else { crc >> 1 };
            bit += 1;
        }
        table[i] = crc;
        i += 1;
    }
    table
};

const CRC16: [u16; 256] = {
    let mut table = [0u16; 256];
    let mut i = 0;
    while i < 256 {
        let mut crc = i as u16;
        let mut bit = 0;
        while bit < 8 {
            crc = if crc & 1 != 0 { (crc >> 1) ^ 0xa001 } else { crc >> 1 };
            bit += 1;
        }
        table[i] = crc;
        i += 1;
    }
    table
};
