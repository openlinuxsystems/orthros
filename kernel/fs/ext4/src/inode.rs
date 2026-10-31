// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Inodes: a file's type, size, link count, times and where its blocks
//! are (`i_block`: the root of its extent tree, an old block map, or a
//! short symbolic link's target).
//!
//! An inode is 128 bytes, or more (256 by default): the bytes after the
//! first 128 hold the times' nanoseconds and the creation time, as far as
//! `i_extra_isize` says, then extended attributes, which Radix keeps as
//! they are. Each inode Radix uses is kept in memory whole, so that what
//! it does not change is written back unchanged.

use radix_vfs::{Error, Kind, Result};

use crate::crc::crc32c;
use crate::sb::{Geometry, ro_compat};
use crate::{le16, le32, put16, put32};

/// Byte offsets of an inode's fields.
pub mod at {
    pub const MODE: usize = 0x00;
    pub const UID: usize = 0x02;
    pub const SIZE_LO: usize = 0x04;
    pub const ATIME: usize = 0x08;
    pub const CTIME: usize = 0x0c;
    pub const MTIME: usize = 0x10;
    pub const DTIME: usize = 0x14;
    pub const GID: usize = 0x18;
    pub const LINKS: usize = 0x1a;
    pub const BLOCKS_LO: usize = 0x1c;
    pub const FLAGS: usize = 0x20;
    pub const BLOCK: usize = 0x28;
    pub const GENERATION: usize = 0x64;
    pub const FILE_ACL_LO: usize = 0x68;
    pub const SIZE_HIGH: usize = 0x6c;
    pub const BLOCKS_HIGH: usize = 0x74;
    pub const FILE_ACL_HIGH: usize = 0x76;
    pub const UID_HIGH: usize = 0x78;
    pub const GID_HIGH: usize = 0x7a;
    pub const CHECKSUM_LO: usize = 0x7c;
    pub const EXTRA_ISIZE: usize = 0x80;
    pub const CHECKSUM_HI: usize = 0x82;
    pub const CTIME_EXTRA: usize = 0x84;
    pub const MTIME_EXTRA: usize = 0x88;
    pub const ATIME_EXTRA: usize = 0x8c;
    pub const CRTIME: usize = 0x90;
    pub const CRTIME_EXTRA: usize = 0x94;
}

/// The bytes of `i_block`.
pub const BLOCK_LEN: usize = 60;

/// `i_mode`'s file types.
pub mod mode {
    pub const TYPE: u16 = 0xf000;
    pub const FIFO: u16 = 0x1000;
    pub const CHAR: u16 = 0x2000;
    pub const DIR: u16 = 0x4000;
    pub const BLOCK: u16 = 0x6000;
    pub const FILE: u16 = 0x8000;
    pub const LINK: u16 = 0xa000;
    pub const SOCKET: u16 = 0xc000;
}

/// `i_flags`.
pub mod flags {
    /// Its contents are encrypted.
    pub const ENCRYPT: u32 = 0x800;
    /// A directory with a hash index.
    pub const INDEX: u32 = 0x1000;
    /// `i_blocks` counts blocks, not sectors.
    pub const HUGE_FILE: u32 = 0x4_0000;
    /// `i_block` holds an extent tree.
    pub const EXTENTS: u32 = 0x8_0000;
    /// Its data is checked by fs-verity.
    pub const VERITY: u32 = 0x10_0000;
    /// It holds a large extended attribute's value.
    pub const EA_INODE: u32 = 0x20_0000;
    /// Its data is in the inode (`inline_data`).
    pub const INLINE_DATA: u32 = 0x1000_0000;
}

/// The kind a file of mode `mode` is, if Radix knows it.
pub fn kind_of(mode: u16) -> Option<Kind> {
    Some(match mode & mode::TYPE {
        mode::FILE => Kind::File,
        mode::DIR => Kind::Directory,
        mode::LINK => Kind::Symlink,
        mode::CHAR => Kind::CharDevice,
        mode::BLOCK => Kind::BlockDevice,
        mode::FIFO => Kind::Pipe,
        mode::SOCKET => Kind::Socket,
        _ => return None,
    })
}

/// The type a directory entry gives a file of kind `kind` (`filetype`).
pub fn dirent_type(kind: Kind) -> u8 {
    match kind {
        Kind::File => 1,
        Kind::Directory => 2,
        Kind::CharDevice => 3,
        Kind::BlockDevice => 4,
        Kind::Pipe => 5,
        Kind::Socket => 6,
        Kind::Symlink => 7,
    }
}

/// The kind a directory entry's type says.
pub fn kind_of_dirent(t: u8) -> Option<Kind> {
    Some(match t {
        1 => Kind::File,
        2 => Kind::Directory,
        3 => Kind::CharDevice,
        4 => Kind::BlockDevice,
        5 => Kind::Pipe,
        6 => Kind::Socket,
        7 => Kind::Symlink,
        _ => return None,
    })
}

const NS: u64 = 1_000_000_000;

/// An inode as Radix uses it: its fields, and the raw bytes it was read
/// from, which [`Inode::encode`] writes them into.
pub struct Inode {
    /// Its type and permissions, as POSIX's `st_mode`.
    pub mode: u16,
    /// Its owner and group, both halves of each, as Linux keeps them
    /// unless mounted with `no_uid32`.
    pub uid: u32,
    pub gid: u32,
    pub links: u16,
    pub size: u64,
    /// The space it takes, in 512-byte sectors.
    pub sectors: u64,
    pub flags: u32,
    pub generation: u32,
    /// Times, in nanoseconds since 1970.
    pub atime: u64,
    pub ctime: u64,
    pub mtime: u64,
    pub crtime: u64,
    /// When it was freed; on the orphan list, the next orphan.
    pub dtime: u32,
    /// The block of its extended attributes, 0 for none.
    pub file_acl: u64,
    pub block: [u8; BLOCK_LEN],
}

/// Whether the field of 4 bytes at `at` is inside an inode whose extra
/// part is `extra` bytes.
fn fits(extra: u16, at: usize) -> bool {
    at + 4 <= 128 + usize::from(extra)
}

/// A time in nanoseconds from the seconds `secs` and, if there is one, the
/// extra field `extra` (two bits of epoch, then nanoseconds), as Linux's
/// `ext4_decode_extra_time` reads them. Times before 1970 are 0.
fn decode_time(secs: u32, extra: Option<u32>) -> u64 {
    let mut s = i64::from(secs as i32);
    let mut nsec = 0;
    if let Some(extra) = extra {
        s += i64::from(extra & 3) << 32;
        nsec = u64::from(extra >> 2).min(NS - 1);
    }
    u64::try_from(s).map_or(0, |s| s.saturating_mul(NS).saturating_add(nsec))
}

/// The seconds and extra field of time `ns`.
fn encode_time(ns: u64) -> (u32, u32) {
    let secs = (ns / NS) as i64;
    let nsec = (ns % NS) as u32;
    let epoch = ((secs - i64::from(secs as i32)) >> 32) as u32 & 3;
    (secs as u32, epoch | (nsec << 2))
}

/// The seed of an inode's checksums: of the volume's, its number and its
/// generation, as Linux's `ext4_inode_csum_seed`.
pub fn seed(geo: &Geometry, ino: u32, generation: u32) -> u32 {
    let sum = crc32c(geo.seed, &ino.to_le_bytes());
    crc32c(sum, &generation.to_le_bytes())
}

/// The checksum of inode `raw`, with the checksum's fields read as zeros
/// (Linux's `ext4_inode_csum`).
fn checksum(raw: &[u8], seed: u32) -> u32 {
    let hi = raw.len() > 128 && le16(raw, at::EXTRA_ISIZE) >= 4;
    let mut sum = crc32c(seed, &raw[..at::CHECKSUM_LO]);
    sum = crc32c(sum, &[0, 0]);
    sum = crc32c(sum, &raw[at::CHECKSUM_LO + 2..128.min(raw.len())]);
    if raw.len() > 128 {
        if hi {
            sum = crc32c(sum, &raw[128..at::CHECKSUM_HI]);
            sum = crc32c(sum, &[0, 0]);
            sum = crc32c(sum, &raw[at::CHECKSUM_HI + 2..]);
        } else {
            sum = crc32c(sum, &raw[128..]);
        }
    }
    sum
}

impl Inode {
    /// Decodes inode `ino` from `raw`, its bytes, checking its checksum.
    pub fn parse(geo: &Geometry, ino: u32, raw: &[u8]) -> Result<Inode> {
        let extra = if raw.len() > 128 { le16(raw, at::EXTRA_ISIZE) } else { 0 };
        if 128 + usize::from(extra) > raw.len() {
            return Err(Error::Corrupt);
        }
        let generation = le32(raw, at::GENERATION);
        if geo.metadata_csum() {
            let sum = checksum(raw, seed(geo, ino, generation));
            let stored_hi = if extra >= 4 { le16(raw, at::CHECKSUM_HI) } else { 0 };
            let stored = u32::from(le16(raw, at::CHECKSUM_LO)) | (u32::from(stored_hi) << 16);
            let want = if extra >= 4 { sum } else { sum & 0xffff };
            if stored != want {
                return Err(Error::Corrupt);
            }
        }
        let mode = le16(raw, at::MODE);
        let flags = le32(raw, at::FLAGS);
        let large = geo.has_ro_compat(ro_compat::LARGE_FILE)
            || mode & mode::TYPE == mode::FILE
            || geo.has_incompat(crate::sb::incompat::LARGEDIR);
        let size = u64::from(le32(raw, at::SIZE_LO))
            | if large { u64::from(le32(raw, at::SIZE_HIGH)) << 32 } else { 0 };
        let huge = geo.has_ro_compat(ro_compat::HUGE_FILE);
        let mut sectors = u64::from(le32(raw, at::BLOCKS_LO))
            | if huge { u64::from(le16(raw, at::BLOCKS_HIGH)) << 32 } else { 0 };
        if huge && flags & flags::HUGE_FILE != 0 {
            sectors <<= geo.shift - 9;
        }
        let time = |at: usize, extra_at: usize| {
            decode_time(le32(raw, at), fits(extra, extra_at).then(|| le32(raw, extra_at)))
        };
        let file_acl = u64::from(le32(raw, at::FILE_ACL_LO))
            | if geo.has_incompat(crate::sb::incompat::BIT64) {
                u64::from(le16(raw, at::FILE_ACL_HIGH)) << 32
            } else {
                0
            };
        let mut block = [0; BLOCK_LEN];
        block.copy_from_slice(&raw[at::BLOCK..at::BLOCK + BLOCK_LEN]);
        Ok(Inode {
            mode,
            uid: u32::from(le16(raw, at::UID)) | u32::from(le16(raw, at::UID_HIGH)) << 16,
            gid: u32::from(le16(raw, at::GID)) | u32::from(le16(raw, at::GID_HIGH)) << 16,
            links: le16(raw, at::LINKS),
            size,
            sectors,
            flags,
            generation,
            atime: time(at::ATIME, at::ATIME_EXTRA),
            ctime: time(at::CTIME, at::CTIME_EXTRA),
            mtime: time(at::MTIME, at::MTIME_EXTRA),
            crtime: if fits(extra, at::CRTIME) { time(at::CRTIME, at::CRTIME_EXTRA) } else { 0 },
            dtime: le32(raw, at::DTIME),
            file_acl,
            block,
        })
    }

    /// Writes its fields into `raw`, its bytes, with its checksum.
    pub fn encode(&self, geo: &Geometry, ino: u32, raw: &mut [u8]) {
        let extra = if raw.len() > 128 { le16(raw, at::EXTRA_ISIZE) } else { 0 };
        put16(raw, at::MODE, self.mode);
        put16(raw, at::UID, self.uid as u16);
        put16(raw, at::UID_HIGH, (self.uid >> 16) as u16);
        put16(raw, at::GID, self.gid as u16);
        put16(raw, at::GID_HIGH, (self.gid >> 16) as u16);
        put16(raw, at::LINKS, self.links);
        put32(raw, at::SIZE_LO, self.size as u32);
        put32(raw, at::SIZE_HIGH, (self.size >> 32) as u32);
        let huge = geo.has_ro_compat(ro_compat::HUGE_FILE);
        let mut flags = self.flags & !flags::HUGE_FILE;
        let mut sectors = self.sectors;
        if huge && sectors >> 48 != 0 {
            flags |= flags::HUGE_FILE;
            sectors >>= geo.shift - 9;
        }
        put32(raw, at::BLOCKS_LO, sectors as u32);
        if huge {
            put16(raw, at::BLOCKS_HIGH, (sectors >> 32) as u16);
        }
        put32(raw, at::FLAGS, flags);
        put32(raw, at::GENERATION, self.generation);
        put32(raw, at::DTIME, self.dtime);
        put32(raw, at::FILE_ACL_LO, self.file_acl as u32);
        if geo.has_incompat(crate::sb::incompat::BIT64) {
            put16(raw, at::FILE_ACL_HIGH, (self.file_acl >> 32) as u16);
        }
        let mut time = |ns: u64, at: usize, extra_at: usize| {
            let (secs, more) = encode_time(ns);
            put32(raw, at, secs);
            if fits(extra, extra_at) {
                put32(raw, extra_at, more);
            }
        };
        time(self.atime, at::ATIME, at::ATIME_EXTRA);
        time(self.ctime, at::CTIME, at::CTIME_EXTRA);
        time(self.mtime, at::MTIME, at::MTIME_EXTRA);
        if fits(extra, at::CRTIME) {
            time(self.crtime, at::CRTIME, at::CRTIME_EXTRA);
        }
        raw[at::BLOCK..at::BLOCK + BLOCK_LEN].copy_from_slice(&self.block);
        if geo.metadata_csum() {
            let sum = checksum(raw, seed(geo, ino, self.generation));
            put16(raw, at::CHECKSUM_LO, sum as u16);
            if extra >= 4 {
                put16(raw, at::CHECKSUM_HI, (sum >> 16) as u16);
            }
        }
    }

    /// The kind of file it is.
    pub fn kind(&self) -> Result<Kind> {
        kind_of(self.mode).ok_or(Error::Corrupt)
    }

    /// The device a character or block device inode stands for, as
    /// Linux's `ext4_iget` reads it from `i_block`: an old 16-bit number
    /// in its first word, else a new 32-bit one in its second; as
    /// [`radix_vfs::device_number`] holds it. 0 for any other inode.
    pub fn device(&self) -> u64 {
        if !matches!(self.mode & mode::TYPE, mode::CHAR | mode::BLOCK) {
            return 0;
        }
        let old = le32(&self.block, 0);
        let (major, minor) = if old != 0 {
            ((old >> 8) & 0xff, old & 0xff)
        } else {
            let new = le32(&self.block, 4);
            ((new & 0xf_ff00) >> 8, (new & 0xff) | ((new >> 12) & 0xf_ff00))
        };
        radix_vfs::device_number(major, minor)
    }

    /// Whether its blocks are in an extent tree.
    pub fn has_extents(&self) -> bool {
        self.flags & flags::EXTENTS != 0
    }

    /// Whether it is a symbolic link whose target is in `i_block`: one
    /// with no blocks but its attributes' (Linux's
    /// `ext4_inode_is_fast_symlink`).
    pub fn fast_link(&self, geo: &Geometry) -> bool {
        let acl = if self.file_acl != 0 { u64::from(geo.block_size) / 512 } else { 0 };
        self.mode & mode::TYPE == mode::LINK
            && self.flags & flags::INLINE_DATA == 0
            && self.sectors.saturating_sub(acl) == 0
    }
}

/// A new inode's bytes: zero but for the size of its extra part.
pub fn fresh(geo: &Geometry, raw: &mut [u8]) {
    raw.fill(0);
    if raw.len() > 128 {
        put16(raw, at::EXTRA_ISIZE, geo.extra_isize);
    }
}

/// Times that went through [`encode_time`]: what the disk keeps of `ns`,
/// with or without nanoseconds.
pub fn kept_time(geo: &Geometry, ns: u64) -> u64 {
    if fits(geo.extra_isize, at::MTIME_EXTRA) { ns } else { ns / NS * NS }
}
