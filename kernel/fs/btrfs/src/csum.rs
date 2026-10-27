// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Btrfs's four checksums, which the superblock names
//! (`csum_type`), and which cover the superblock, every tree block and
//! every sector of file data:
//!
//! - CRC-32C, the default: the Castagnoli polynomial, reflected, started
//!   from and finished with all ones (RFC 3720's iSCSI CRC), kept as a
//!   little-endian 32-bit word;
//! - xxHash64 with seed 0, kept as a little-endian 64-bit word;
//! - SHA-256 (FIPS 180-4, [`crate::sha256`]);
//! - BLAKE2b with a 256-bit digest and no key (RFC 7693,
//!   [`crate::blake2b`]).
//!
//! A checksum takes [`CSUM_SIZE`] bytes on the disk, its unused tail
//! zero.

/// The room a checksum has in a superblock or a tree block's header.
pub const CSUM_SIZE: usize = 32;

/// A checksum, as the superblock's `csum_type` names it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CsumType {
    /// CRC-32C, the default.
    Crc32c,
    /// xxHash64.
    Xxhash64,
    /// SHA-256.
    Sha256,
    /// BLAKE2b-256.
    Blake2b,
}

impl CsumType {
    /// The checksum numbered `value` on the disk, if Radix knows it.
    pub fn from_disk(value: u16) -> Option<CsumType> {
        match value {
            0 => Some(CsumType::Crc32c),
            1 => Some(CsumType::Xxhash64),
            2 => Some(CsumType::Sha256),
            3 => Some(CsumType::Blake2b),
            _ => None,
        }
    }

    /// How many bytes of a checksum's room it fills.
    pub fn size(self) -> usize {
        match self {
            CsumType::Crc32c => 4,
            CsumType::Xxhash64 => 8,
            CsumType::Sha256 | CsumType::Blake2b => 32,
        }
    }

    /// Its name, as Linux gives it.
    pub fn name(self) -> &'static str {
        match self {
            CsumType::Crc32c => "crc32c",
            CsumType::Xxhash64 => "xxhash64",
            CsumType::Sha256 => "sha256",
            CsumType::Blake2b => "blake2b",
        }
    }

    /// The checksum of `data`, as the disk keeps it: [`CsumType::size`]
    /// bytes, then zeros.
    pub fn sum(self, data: &[u8]) -> [u8; CSUM_SIZE] {
        let mut out = [0; CSUM_SIZE];
        match self {
            CsumType::Crc32c => out[..4].copy_from_slice(&crc32c(data).to_le_bytes()),
            CsumType::Xxhash64 => out[..8].copy_from_slice(&xxhash64(data).to_le_bytes()),
            CsumType::Sha256 => out = crate::sha256::hash(data),
            CsumType::Blake2b => out = crate::blake2b::hash(data),
        }
        out
    }

    /// Whether `stored`, a checksum's bytes as the disk has them, is the
    /// checksum of `data`.
    pub fn matches(self, data: &[u8], stored: &[u8]) -> bool {
        let size = self.size();
        stored.len() >= size && self.sum(data)[..size] == stored[..size]
    }
}

/// CRC-32C's reflected polynomial.
const CASTAGNOLI: u32 = 0x82f6_3b78;

/// CRC-32C's tables for eight bytes at a time ("slicing by 8"): table 0
/// is the CRC of each byte value, and table `k` that of the byte followed
/// by `k` zero bytes. Worked out at compile time.
const TABLES: [[u32; 256]; 8] = {
    let mut tables = [[0u32; 256]; 8];
    let mut i = 0;
    while i < 256 {
        let mut crc = i as u32;
        let mut bit = 0;
        while bit < 8 {
            crc = if crc & 1 != 0 { (crc >> 1) ^ CASTAGNOLI } else { crc >> 1 };
            bit += 1;
        }
        tables[0][i] = crc;
        i += 1;
    }
    let mut k = 1;
    while k < 8 {
        let mut i = 0;
        while i < 256 {
            let previous = tables[k - 1][i];
            tables[k][i] = (previous >> 8) ^ tables[0][(previous & 0xff) as usize];
            i += 1;
        }
        k += 1;
    }
    tables
};

/// The CRC-32C of `data`.
pub fn crc32c(data: &[u8]) -> u32 {
    !crc32c_update(!0, data)
}

/// The CRC-32C register `crc` after `data`, with neither the first nor
/// the last inversion: Btrfs hashes names with it, from `!1`
/// ([`crate::format::name_hash`]).
pub fn crc32c_update(crc: u32, data: &[u8]) -> u32 {
    let mut crc = crc;
    let (words, rest) = data.as_chunks::<8>();
    for word in words {
        let low = u32::from_le_bytes([word[0], word[1], word[2], word[3]]) ^ crc;
        let high = u32::from_le_bytes([word[4], word[5], word[6], word[7]]);
        crc = TABLES[7][(low & 0xff) as usize]
            ^ TABLES[6][((low >> 8) & 0xff) as usize]
            ^ TABLES[5][((low >> 16) & 0xff) as usize]
            ^ TABLES[4][(low >> 24) as usize]
            ^ TABLES[3][(high & 0xff) as usize]
            ^ TABLES[2][((high >> 8) & 0xff) as usize]
            ^ TABLES[1][((high >> 16) & 0xff) as usize]
            ^ TABLES[0][(high >> 24) as usize];
    }
    for &byte in rest {
        crc = (crc >> 8) ^ TABLES[0][((crc ^ u32::from(byte)) & 0xff) as usize];
    }
    crc
}

const PRIME64_1: u64 = 0x9e37_79b1_85eb_ca87;
const PRIME64_2: u64 = 0xc2b2_ae3d_27d4_eb4f;
const PRIME64_3: u64 = 0x1656_67b1_9e37_79f9;
const PRIME64_4: u64 = 0x85eb_ca77_c2b2_ae63;
const PRIME64_5: u64 = 0x27d4_eb2f_1656_67c5;

/// One round of xxHash64: `lane` takes in the 8 bytes `input`.
fn round(lane: u64, input: u64) -> u64 {
    lane.wrapping_add(input.wrapping_mul(PRIME64_2)).rotate_left(31).wrapping_mul(PRIME64_1)
}

/// Folds `lane` into the hash `hash` of 32 bytes or more.
fn merge(hash: u64, lane: u64) -> u64 {
    (hash ^ round(0, lane)).wrapping_mul(PRIME64_1).wrapping_add(PRIME64_4)
}

/// The xxHash64 of `data`, with seed 0, as Yann Collet's specification
/// ("xxHash fast digest algorithm", version 0.1.1) defines it.
pub fn xxhash64(data: &[u8]) -> u64 {
    let mut rest = data;
    let mut hash = if data.len() >= 32 {
        let mut lanes =
            [PRIME64_1.wrapping_add(PRIME64_2), PRIME64_2, 0, 0u64.wrapping_sub(PRIME64_1)];
        let (stripes, after) = data.as_chunks::<32>();
        for stripe in stripes {
            for (lane, input) in lanes.iter_mut().zip(stripe.as_chunks::<8>().0) {
                *lane = round(*lane, u64::from_le_bytes(*input));
            }
        }
        rest = after;
        let mut hash = lanes[0]
            .rotate_left(1)
            .wrapping_add(lanes[1].rotate_left(7))
            .wrapping_add(lanes[2].rotate_left(12))
            .wrapping_add(lanes[3].rotate_left(18));
        for lane in lanes {
            hash = merge(hash, lane);
        }
        hash
    } else {
        PRIME64_5
    };
    hash = hash.wrapping_add(data.len() as u64);
    let (words, after) = rest.as_chunks::<8>();
    for word in words {
        hash ^= round(0, u64::from_le_bytes(*word));
        hash = hash.rotate_left(27).wrapping_mul(PRIME64_1).wrapping_add(PRIME64_4);
    }
    rest = after;
    if rest.len() >= 4 {
        let word = u32::from_le_bytes([rest[0], rest[1], rest[2], rest[3]]);
        hash ^= u64::from(word).wrapping_mul(PRIME64_1);
        hash = hash.rotate_left(23).wrapping_mul(PRIME64_2).wrapping_add(PRIME64_3);
        rest = &rest[4..];
    }
    for &byte in rest {
        hash ^= u64::from(byte).wrapping_mul(PRIME64_5);
        hash = hash.rotate_left(11).wrapping_mul(PRIME64_1);
    }
    hash ^= hash >> 33;
    hash = hash.wrapping_mul(PRIME64_2);
    hash ^= hash >> 29;
    hash = hash.wrapping_mul(PRIME64_3);
    hash ^ (hash >> 32)
}
