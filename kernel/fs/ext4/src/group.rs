// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Block group descriptors: where a group's block bitmap, inode bitmap
//! and inode table are, how many of its blocks and inodes are free, and
//! the checksums of its bitmaps (`metadata_csum`). A descriptor is 32
//! bytes, or 64 with `64bit`.
//!
//! With `uninit_bg` or `metadata_csum`, a group may have no bitmaps yet:
//! `BLOCK_UNINIT` says its block bitmap was never written (only its own
//! metadata is in use), `INODE_UNINIT` the same of its inode bitmap, and
//! `itable_unused` how many inodes at the end of its table were never
//! used, which `e2fsck` does not read.

use radix_vfs::{Error, Result};

use crate::crc::{crc16, crc32c};
use crate::sb::{Csum, Geometry};
use crate::{le16, le32, put16, put32};

/// `bg_flags`.
pub mod flags {
    pub const INODE_UNINIT: u16 = 0x1;
    pub const BLOCK_UNINIT: u16 = 0x2;
}

/// Byte offset of `bg_checksum`.
const CHECKSUM: usize = 0x1e;

/// A group's descriptor.
#[derive(Clone, Copy, Debug)]
pub struct Group {
    pub block_bitmap: u64,
    pub inode_bitmap: u64,
    pub inode_table: u64,
    pub free_blocks: u32,
    pub free_inodes: u32,
    pub dirs: u32,
    pub flags: u16,
    pub itable_unused: u32,
    pub block_csum: u32,
    pub inode_csum: u32,
    /// The descriptor as read, for the fields Radix keeps as they are.
    raw: [u8; 64],
}

/// The field of `lo` and, with 64-byte descriptors, `hi` at the given
/// offsets.
fn wide(raw: &[u8], size: usize, lo: usize, hi: usize) -> u64 {
    let high = if size >= 64 { u64::from(le32(raw, hi)) << 32 } else { 0 };
    u64::from(le32(raw, lo)) | high
}

fn wide16(raw: &[u8], size: usize, lo: usize, hi: usize) -> u32 {
    let high = if size >= 64 { u32::from(le16(raw, hi)) << 16 } else { 0 };
    u32::from(le16(raw, lo)) | high
}

impl Group {
    /// Decodes the descriptor `raw` of group `g`, checking its checksum.
    pub fn parse(geo: &Geometry, g: u32, raw: &[u8]) -> Result<Group> {
        let size = geo.desc_size as usize;
        let mut copy = [0; 64];
        copy[..size.min(64)].copy_from_slice(&raw[..size.min(64)]);
        if geo.csum != Csum::None && le16(raw, CHECKSUM) != checksum(geo, g, raw) {
            return Err(Error::Corrupt);
        }
        let s = size.min(64);
        Ok(Group {
            block_bitmap: wide(&copy, s, 0x00, 0x20),
            inode_bitmap: wide(&copy, s, 0x04, 0x24),
            inode_table: wide(&copy, s, 0x08, 0x28),
            free_blocks: wide16(&copy, s, 0x0c, 0x2c),
            free_inodes: wide16(&copy, s, 0x0e, 0x2e),
            dirs: wide16(&copy, s, 0x10, 0x30),
            flags: le16(&copy, 0x12),
            itable_unused: wide16(&copy, s, 0x1c, 0x32),
            block_csum: wide16(&copy, s, 0x18, 0x38),
            inode_csum: wide16(&copy, s, 0x1a, 0x3a),
            raw: copy,
        })
    }

    /// Encodes it into `out` (the descriptor's size) for group `g`, with
    /// its checksum.
    pub fn encode(&self, geo: &Geometry, g: u32, out: &mut [u8]) {
        let size = geo.desc_size as usize;
        let s = size.min(64);
        out[..s].copy_from_slice(&self.raw[..s]);
        let mut put = |lo: usize, hi: usize, v: u64| {
            put32(out, lo, v as u32);
            if s >= 64 {
                put32(out, hi, (v >> 32) as u32);
            }
        };
        put(0x00, 0x20, self.block_bitmap);
        put(0x04, 0x24, self.inode_bitmap);
        put(0x08, 0x28, self.inode_table);
        let mut put16s = |lo: usize, hi: usize, v: u32| {
            put16(out, lo, v as u16);
            if s >= 64 {
                put16(out, hi, (v >> 16) as u16);
            }
        };
        put16s(0x0c, 0x2c, self.free_blocks);
        put16s(0x0e, 0x2e, self.free_inodes);
        put16s(0x10, 0x30, self.dirs);
        put16s(0x1c, 0x32, self.itable_unused);
        put16s(0x18, 0x38, self.block_csum);
        put16s(0x1a, 0x3a, self.inode_csum);
        put16(out, 0x12, self.flags);
        if geo.csum != Csum::None {
            let sum = checksum(geo, g, out);
            put16(out, CHECKSUM, sum);
        }
    }

    /// The bitmaps' checksums as they go in the descriptor: CRC-32C of the
    /// bitmap's bytes that cover the group (Linux's
    /// `ext4_block_bitmap_csum_set`), cut to 16 bits with 32-byte
    /// descriptors.
    pub fn bitmap_csum(geo: &Geometry, sum: u32) -> u32 {
        if geo.desc_size >= 64 { sum } else { sum & 0xffff }
    }
}

/// The checksum of descriptor `raw` of group `g`, as Linux's
/// `ext4_group_desc_csum` makes it.
pub fn checksum(geo: &Geometry, g: u32, raw: &[u8]) -> u16 {
    let size = geo.desc_size as usize;
    let group = g.to_le_bytes();
    match geo.csum {
        Csum::None => 0,
        Csum::Crc32c => {
            let mut sum = crc32c(geo.seed, &group);
            sum = crc32c(sum, &raw[..CHECKSUM]);
            sum = crc32c(sum, &[0, 0]);
            sum = crc32c(sum, &raw[CHECKSUM + 2..size]);
            sum as u16
        }
        Csum::Crc16 => {
            let mut sum = crc16(!0, &geo.uuid);
            sum = crc16(sum, &group);
            sum = crc16(sum, &raw[..CHECKSUM]);
            if geo.has_incompat(crate::sb::incompat::BIT64) && CHECKSUM + 2 < size {
                sum = crc16(sum, &raw[CHECKSUM + 2..size]);
            }
            sum
        }
    }
}
