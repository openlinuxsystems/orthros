// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! The superblock: 4 KiB at 64 KiB into the device, naming the
//! filesystem, its checksum, its block sizes, the root blocks of the root
//! tree and the chunk tree, and the chunks that hold the chunk tree
//! itself (the "system chunk array").
//!
//! Like Linux, Radix mounts from the first copy only; the copies at
//! 64 MiB and 256 GiB are for repair tools.

use radix_vfs::mem::{self, KVec};
use radix_vfs::{Error, Result};

use crate::csum::{CSUM_SIZE, CsumType};
use crate::format::{u16_at, u32_at, u64_at, uuid_at};

/// Where the superblock is on a device.
pub const OFFSET: u64 = 64 * 1024;

/// The superblock's size, all of it under its checksum.
pub const SIZE: usize = 4096;

/// Its magic number: `_BHRfS_M`.
pub const MAGIC: u64 = 0x4d5f_5366_5248_425f;

/// The most bytes the system chunk array holds.
pub const SYS_CHUNK_ARRAY_MAX: usize = 2048;

/// Where the system chunk array is in the superblock.
const SYS_CHUNK_ARRAY_AT: usize = 0x32b;

/// The most bytes a label holds, its NUL included.
pub const LABEL_MAX: usize = 256;

/// Features a filesystem may have that a writer must know (the
/// superblock's `compat_ro_flags`): one it does not know mounts read-only.
pub mod compat_ro {
    /// The free-space tree keeps each block group's free space.
    pub const FREE_SPACE_TREE: u64 = 1 << 0;
    /// The free-space tree is complete and current.
    pub const FREE_SPACE_TREE_VALID: u64 = 1 << 1;
    /// Some files have fs-verity's Merkle trees.
    pub const VERITY: u64 = 1 << 2;
    /// Block group items are in a tree of their own.
    pub const BLOCK_GROUP_TREE: u64 = 1 << 3;

    /// The features Radix writes.
    pub const WRITE: u64 = FREE_SPACE_TREE | FREE_SPACE_TREE_VALID | BLOCK_GROUP_TREE;
}

/// Features a filesystem may use that change what is on the disk; a
/// filesystem with one Radix does not know cannot be read.
pub mod incompat {
    /// Back references of the newer kind; every filesystem since 2008.
    pub const MIXED_BACKREF: u64 = 1 << 0;
    /// A default subvolume other than the top one may be set.
    pub const DEFAULT_SUBVOL: u64 = 1 << 1;
    /// Data and metadata in the same chunks (`mkfs.btrfs --mixed`).
    pub const MIXED_GROUPS: u64 = 1 << 2;
    /// Files compressed with LZO.
    pub const COMPRESS_LZO: u64 = 1 << 3;
    /// Files compressed with Zstandard.
    pub const COMPRESS_ZSTD: u64 = 1 << 4;
    /// Tree blocks larger than a page.
    pub const BIG_METADATA: u64 = 1 << 5;
    /// `INODE_EXTREF` items, for many hard links in one directory.
    pub const EXTENDED_IREF: u64 = 1 << 6;
    /// RAID5 or RAID6 chunks.
    pub const RAID56: u64 = 1 << 7;
    /// Tree blocks' extents as `METADATA_ITEM`s.
    pub const SKINNY_METADATA: u64 = 1 << 8;
    /// Holes in files are left out instead of kept as extents.
    pub const NO_HOLES: u64 = 1 << 9;
    /// Tree blocks carry a UUID other than the filesystem's.
    pub const METADATA_UUID: u64 = 1 << 10;
    /// RAID1C3 or RAID1C4 chunks.
    pub const RAID1C34: u64 = 1 << 11;
    /// On a zoned device: written in sequence.
    pub const ZONED: u64 = 1 << 12;
    /// The second extent tree, still experimental.
    pub const EXTENT_TREE_V2: u64 = 1 << 13;
    /// Where stripes are, in a tree of their own.
    pub const RAID_STRIPE_TREE: u64 = 1 << 14;
    /// Quotas counted by who allocated, not who refers.
    pub const SIMPLE_QUOTA: u64 = 1 << 16;

    /// The features Radix reads. Zoned devices, the second extent tree
    /// and the RAID stripe tree change where things are, and are refused;
    /// so is RAID5 and RAID6, which Dukovo never offers, as they are unsafe
    /// even on Linux.
    pub const READ: u64 = MIXED_BACKREF
        | DEFAULT_SUBVOL
        | MIXED_GROUPS
        | COMPRESS_LZO
        | COMPRESS_ZSTD
        | BIG_METADATA
        | EXTENDED_IREF
        | SKINNY_METADATA
        | NO_HOLES
        | METADATA_UUID
        | RAID1C34
        | SIMPLE_QUOTA;
}

/// What the superblock says.
pub struct Superblock {
    /// The checksum of every block.
    pub csum_type: CsumType,
    /// The filesystem's UUID.
    pub fsid: [u8; 16],
    /// The UUID its tree blocks carry: the filesystem's, unless it was
    /// changed later without rewriting them (`METADATA_UUID`).
    pub metadata_uuid: [u8; 16],
    /// The last transaction committed.
    pub generation: u64,
    /// The logical address of the root tree's root block.
    pub root: u64,
    /// The root tree's root block's level.
    pub root_level: u8,
    /// The logical address of the chunk tree's root block.
    pub chunk_root: u64,
    /// The chunk tree's root block's level.
    pub chunk_root_level: u8,
    /// The transaction that wrote the chunk tree's root block.
    pub chunk_root_generation: u64,
    /// The log tree's root block, 0 if there is no log to replay.
    pub log_root: u64,
    /// The log tree's root block's level.
    pub log_root_level: u8,
    /// The filesystem's size.
    pub total_bytes: u64,
    /// How much of it is used.
    pub bytes_used: u64,
    /// How many devices the filesystem spans.
    pub num_devices: u64,
    /// The smallest unit of data.
    pub sectorsize: u32,
    /// The size of a tree block.
    pub nodesize: u32,
    /// The features that change what is on the disk ([`incompat`]).
    pub incompat_flags: u64,
    /// The features a reader may ignore, but a writer may not.
    pub compat_ro_flags: u64,
    /// This device's ID within the filesystem.
    pub devid: u64,
    /// This device's UUID.
    pub dev_uuid: [u8; 16],
    /// The label's bytes, up to its NUL.
    pub label: KVec<u8>,
    /// The chunks of the system block group, which hold the chunk tree:
    /// a key and a chunk item each.
    pub sys_chunk_array: KVec<u8>,
}

impl Superblock {
    /// Reads the superblock in `bytes`, the [`SIZE`] bytes at [`OFFSET`]
    /// of a device, and checks it as Linux does before mounting.
    ///
    /// Fails with [`Error::UnknownFilesystem`] when it is not a Btrfs
    /// superblock, [`Error::Corrupt`] when it is a damaged one, and
    /// [`Error::NotSupported`] when it uses a feature Radix does not read.
    pub fn read(bytes: &[u8]) -> Result<Superblock> {
        if bytes.len() < SIZE || u64_at(bytes, 0x40) != MAGIC {
            return Err(Error::UnknownFilesystem);
        }
        let csum_type = CsumType::from_disk(u16_at(bytes, 0xc4)).ok_or(Error::NotSupported)?;
        if !csum_type.matches(&bytes[CSUM_SIZE..SIZE], &bytes[..CSUM_SIZE]) {
            return Err(Error::Corrupt);
        }
        let incompat_flags = u64_at(bytes, 0xbc);
        if incompat_flags & !incompat::READ != 0 {
            return Err(Error::NotSupported);
        }
        let fsid = uuid_at(bytes, 0x20);
        let metadata_uuid = if incompat_flags & incompat::METADATA_UUID != 0 {
            uuid_at(bytes, 0x23b)
        } else {
            fsid
        };
        let label = &bytes[0x12b..0x12b + LABEL_MAX];
        let label_len = label.iter().position(|&b| b == 0).unwrap_or(LABEL_MAX);
        let sys_chunk_array_size = u32_at(bytes, 0xa0) as usize;
        if sys_chunk_array_size > SYS_CHUNK_ARRAY_MAX {
            return Err(Error::Corrupt);
        }
        let sb = Superblock {
            csum_type,
            fsid,
            metadata_uuid,
            generation: u64_at(bytes, 0x48),
            root: u64_at(bytes, 0x50),
            root_level: bytes[0xc6],
            chunk_root: u64_at(bytes, 0x58),
            chunk_root_level: bytes[0xc7],
            chunk_root_generation: u64_at(bytes, 0xa4),
            log_root: u64_at(bytes, 0x60),
            log_root_level: bytes[0xc8],
            total_bytes: u64_at(bytes, 0x70),
            bytes_used: u64_at(bytes, 0x78),
            num_devices: u64_at(bytes, 0x88),
            sectorsize: u32_at(bytes, 0x90),
            nodesize: u32_at(bytes, 0x94),
            incompat_flags,
            compat_ro_flags: u64_at(bytes, 0xb4),
            devid: u64_at(bytes, 0xc9),
            dev_uuid: uuid_at(bytes, 0xc9 + 66),
            label: mem::copy(&label[..label_len])?,
            sys_chunk_array: mem::copy(
                &bytes[SYS_CHUNK_ARRAY_AT..SYS_CHUNK_ARRAY_AT + sys_chunk_array_size],
            )?,
        };
        sb.check(u64_at(bytes, 0x30))?;
        Ok(sb)
    }

    /// The checks of Linux's `btrfs_validate_super` that reading needs.
    fn check(&self, bytenr: u64) -> Result<()> {
        let sector = self.sectorsize;
        let node = self.nodesize;
        let sizes_fit = sector.is_power_of_two()
            && (4096..=65536).contains(&sector)
            && node.is_power_of_two()
            && node >= sector
            && node <= 65536;
        let aligned = |at: u64| at.is_multiple_of(u64::from(sector));
        // The tree levels go up to 7, as BTRFS_MAX_LEVEL says.
        let fits = bytenr == OFFSET
            && sizes_fit
            && aligned(self.root)
            && aligned(self.chunk_root)
            && aligned(self.log_root)
            && self.root_level < 8
            && self.chunk_root_level < 8
            && self.log_root_level < 8
            && self.num_devices > 0
            && self.root != 0
            && self.chunk_root != 0;
        if !fits {
            return Err(Error::Corrupt);
        }
        Ok(())
    }

    /// The label, as text, if it is UTF-8.
    pub fn label(&self) -> Option<&str> {
        core::str::from_utf8(&self.label).ok()
    }

    /// Whether it has the incompatible feature `flag`.
    pub fn has(&self, flag: u64) -> bool {
        self.incompat_flags & flag != 0
    }

    /// The system chunk array's bytes.
    pub fn sys_chunks(&self) -> &[u8] {
        &self.sys_chunk_array
    }
}
