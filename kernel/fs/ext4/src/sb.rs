// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! The superblock: 1024 bytes at byte 1024 of the volume, which say how
//! big its blocks and groups are, which features it uses, and how much is
//! free.
//!
//! Features come in three sets. A volume with an `incompat` feature Radix
//! does not know is not mounted at all; one with a `ro_compat` feature it
//! does not know is mounted read-only, as on Linux. Radix also mounts
//! read-only some volumes whose features it reads but does not write
//! (quotas, inline data, encryption, case folding, bigalloc, an external
//! journal, fast commits): a writable mount of one fails with
//! [`Error::NotSupported`], and Spark mounts it read-only instead, as for
//! a Btrfs that Radix cannot write (ADR 0130).

use radix_vfs::{Error, Result};

use crate::crc::crc32c;
use crate::{le16, le32, put16, put32};

/// Where the superblock is, in bytes from the volume's start.
pub const OFFSET: u64 = 1024;
/// Its size.
pub const SIZE: usize = 1024;
/// Its magic number, at [`at::MAGIC`].
pub const MAGIC: u16 = 0xef53;
/// The root directory's inode.
pub const ROOT: u32 = 2;

/// Byte offsets of the superblock's fields.
pub mod at {
    pub const INODES: usize = 0x00;
    pub const BLOCKS_LO: usize = 0x04;
    pub const FREE_BLOCKS_LO: usize = 0x0c;
    pub const FREE_INODES: usize = 0x10;
    pub const FIRST_DATA_BLOCK: usize = 0x14;
    pub const LOG_BLOCK_SIZE: usize = 0x18;
    pub const LOG_CLUSTER_SIZE: usize = 0x1c;
    pub const BLOCKS_PER_GROUP: usize = 0x20;
    pub const INODES_PER_GROUP: usize = 0x28;
    pub const MTIME: usize = 0x2c;
    pub const WTIME: usize = 0x30;
    pub const MAGIC: usize = 0x38;
    pub const STATE: usize = 0x3a;
    pub const REV_LEVEL: usize = 0x4c;
    pub const FIRST_INO: usize = 0x54;
    pub const INODE_SIZE: usize = 0x58;
    pub const COMPAT: usize = 0x5c;
    pub const INCOMPAT: usize = 0x60;
    pub const RO_COMPAT: usize = 0x64;
    pub const UUID: usize = 0x68;
    pub const VOLUME_NAME: usize = 0x78;
    pub const RESERVED_GDT: usize = 0xce;
    pub const JOURNAL_INUM: usize = 0xe0;
    pub const LAST_ORPHAN: usize = 0xe8;
    pub const DESC_SIZE: usize = 0xfe;
    pub const FIRST_META_BG: usize = 0x104;
    pub const BLOCKS_HI: usize = 0x150;
    pub const FREE_BLOCKS_HI: usize = 0x158;
    pub const MIN_EXTRA_ISIZE: usize = 0x15c;
    pub const WANT_EXTRA_ISIZE: usize = 0x15e;
    pub const CHECKSUM_TYPE: usize = 0x175;
    pub const BACKUP_BGS: usize = 0x24c;
    pub const CHECKSUM_SEED: usize = 0x270;
    pub const CHECKSUM: usize = 0x3fc;
}

/// `s_feature_compat`.
pub mod compat {
    pub const DIR_PREALLOC: u32 = 0x1;
    pub const IMAGIC_INODES: u32 = 0x2;
    pub const HAS_JOURNAL: u32 = 0x4;
    pub const EXT_ATTR: u32 = 0x8;
    pub const RESIZE_INODE: u32 = 0x10;
    pub const DIR_INDEX: u32 = 0x20;
    pub const LAZY_BG: u32 = 0x40;
    pub const SPARSE_SUPER2: u32 = 0x200;
    pub const FAST_COMMIT: u32 = 0x400;
    pub const STABLE_INODES: u32 = 0x800;
    pub const ORPHAN_FILE: u32 = 0x1000;
}

/// `s_feature_incompat`.
pub mod incompat {
    pub const FILETYPE: u32 = 0x2;
    pub const RECOVER: u32 = 0x4;
    pub const META_BG: u32 = 0x10;
    pub const EXTENTS: u32 = 0x40;
    pub const BIT64: u32 = 0x80;
    pub const MMP: u32 = 0x100;
    pub const FLEX_BG: u32 = 0x200;
    pub const EA_INODE: u32 = 0x400;
    pub const CSUM_SEED: u32 = 0x2000;
    pub const LARGEDIR: u32 = 0x4000;
    pub const INLINE_DATA: u32 = 0x8000;
    pub const ENCRYPT: u32 = 0x1_0000;
    pub const CASEFOLD: u32 = 0x2_0000;
}

/// `s_feature_ro_compat`.
pub mod ro_compat {
    pub const SPARSE_SUPER: u32 = 0x1;
    pub const LARGE_FILE: u32 = 0x2;
    pub const BTREE_DIR: u32 = 0x4;
    pub const HUGE_FILE: u32 = 0x8;
    pub const GDT_CSUM: u32 = 0x10;
    pub const DIR_NLINK: u32 = 0x20;
    pub const EXTRA_ISIZE: u32 = 0x40;
    pub const METADATA_CSUM: u32 = 0x400;
}

/// The `incompat` features Radix reads.
const READ_INCOMPAT: u32 = incompat::FILETYPE
    | incompat::RECOVER
    | incompat::META_BG
    | incompat::EXTENTS
    | incompat::BIT64
    | incompat::MMP
    | incompat::FLEX_BG
    | incompat::EA_INODE
    | incompat::CSUM_SEED
    | incompat::LARGEDIR
    | incompat::INLINE_DATA
    | incompat::ENCRYPT
    | incompat::CASEFOLD;

/// The `incompat` features Radix writes; a writable volume has extents.
const WRITE_INCOMPAT: u32 = incompat::FILETYPE
    | incompat::RECOVER
    | incompat::META_BG
    | incompat::EXTENTS
    | incompat::BIT64
    | incompat::FLEX_BG
    | incompat::CSUM_SEED
    | incompat::LARGEDIR;

/// The `ro_compat` features Radix writes.
const WRITE_RO_COMPAT: u32 = ro_compat::SPARSE_SUPER
    | ro_compat::LARGE_FILE
    | ro_compat::BTREE_DIR
    | ro_compat::HUGE_FILE
    | ro_compat::GDT_CSUM
    | ro_compat::DIR_NLINK
    | ro_compat::EXTRA_ISIZE
    | ro_compat::METADATA_CSUM;

/// The `compat` features Radix writes alongside: those it need not keep
/// up to date (the resize inode, the hash index it drops from a directory
/// it changes, an orphan file it leaves empty, fast commits it does not
/// make).
const WRITE_COMPAT: u32 = compat::DIR_PREALLOC
    | compat::IMAGIC_INODES
    | compat::HAS_JOURNAL
    | compat::EXT_ATTR
    | compat::RESIZE_INODE
    | compat::DIR_INDEX
    | compat::LAZY_BG
    | compat::FAST_COMMIT
    | compat::STABLE_INODES
    | compat::ORPHAN_FILE;

/// `s_state`: unmounted cleanly.
pub const VALID_FS: u16 = 1;

/// How the volume's metadata is checksummed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Csum {
    /// Not at all.
    None,
    /// The group descriptors alone, with CRC-16 (`uninit_bg`).
    Crc16,
    /// All of it, with CRC-32C (`metadata_csum`).
    Crc32c,
}

/// What the superblock says of the volume's layout.
#[derive(Clone, Debug)]
pub struct Geometry {
    pub block_size: u32,
    /// log2 of the block size.
    pub shift: u32,
    pub blocks: u64,
    pub first_data_block: u64,
    pub blocks_per_group: u32,
    pub inodes_per_group: u32,
    pub inodes: u32,
    pub groups: u32,
    pub desc_size: u32,
    pub inode_size: u32,
    pub first_ino: u32,
    pub reserved_gdt: u32,
    pub first_meta_bg: u32,
    /// The blocks of descriptors, without `meta_bg`.
    pub gdt_blocks: u32,
    pub compat: u32,
    pub incompat: u32,
    pub ro_compat: u32,
    pub csum: Csum,
    /// The seed of every CRC-32C of the volume's metadata.
    pub seed: u32,
    pub uuid: [u8; 16],
    pub label: [u8; 16],
    /// The journal's inode, 0 without one.
    pub journal_inum: u32,
    /// The groups other than 0 with a backup superblock, with
    /// `sparse_super2`.
    pub backup_groups: [u32; 2],
    /// The extra inode bytes new inodes get.
    pub extra_isize: u16,
    /// Whether Radix may write it; otherwise only read.
    pub writable: bool,
}

impl Geometry {
    /// Reads the superblock `sb` of a volume of `bytes` bytes.
    pub fn parse(sb: &[u8], bytes: u64) -> Result<Geometry> {
        if sb.len() < SIZE || le16(sb, at::MAGIC) != MAGIC {
            return Err(Error::UnknownFilesystem);
        }
        let rev = le32(sb, at::REV_LEVEL);
        let compat = le32(sb, at::COMPAT);
        let (incompat, ro_compat) =
            if rev == 0 { (0, 0) } else { (le32(sb, at::INCOMPAT), le32(sb, at::RO_COMPAT)) };
        let log = le32(sb, at::LOG_BLOCK_SIZE);
        // Blocks larger than a page, unknown features that change the
        // format, and clusters of several blocks are not read.
        if rev > 1 || log > 2 || incompat & !READ_INCOMPAT != 0 {
            return Err(Error::NotSupported);
        }
        if le32(sb, at::LOG_CLUSTER_SIZE) != log {
            return Err(Error::NotSupported);
        }
        let shift = 10 + log;
        let block_size = 1u32 << shift;
        let bit64 = incompat & incompat::BIT64 != 0;
        let blocks = u64::from(le32(sb, at::BLOCKS_LO))
            | if bit64 { u64::from(le32(sb, at::BLOCKS_HI)) << 32 } else { 0 };
        let first_data_block = u64::from(le32(sb, at::FIRST_DATA_BLOCK));
        let blocks_per_group = le32(sb, at::BLOCKS_PER_GROUP);
        let inodes_per_group = le32(sb, at::INODES_PER_GROUP);
        let inodes = le32(sb, at::INODES);
        // `mke2fs -g` makes groups smaller than a bitmap block covers.
        if first_data_block != u64::from(block_size == 1024)
            || blocks_per_group < 8
            || blocks_per_group > block_size * 8
            || !blocks_per_group.is_multiple_of(8)
            || inodes_per_group == 0
            || inodes_per_group > block_size * 8
            || blocks <= first_data_block
            || blocks.checked_mul(u64::from(block_size)).is_none_or(|end| end > bytes)
        {
            return Err(Error::Corrupt);
        }
        let groups = (blocks - first_data_block).div_ceil(u64::from(blocks_per_group));
        let groups = u32::try_from(groups).map_err(|_| Error::Corrupt)?;
        if u64::from(groups) * u64::from(inodes_per_group) != u64::from(inodes) {
            return Err(Error::Corrupt);
        }
        let (inode_size, first_ino) = if rev == 0 {
            (128, 11)
        } else {
            (u32::from(le16(sb, at::INODE_SIZE)), le32(sb, at::FIRST_INO))
        };
        // Radix keeps each inode it uses in memory: larger ones than
        // 1 KiB are not read.
        if !inode_size.is_power_of_two() || !(128..=block_size.min(1024)).contains(&inode_size) {
            return Err(Error::NotSupported);
        }
        if first_ino < 11 || first_ino > inodes {
            return Err(Error::Corrupt);
        }
        // Descriptors larger than 64 bytes hold nothing Linux uses: Radix
        // reads only the two sizes `mke2fs` makes.
        let desc_size = if bit64 { u32::from(le16(sb, at::DESC_SIZE)) } else { 32 };
        if bit64 && desc_size != 64 {
            return Err(if desc_size.is_power_of_two() && desc_size > 64 {
                Error::NotSupported
            } else {
                Error::Corrupt
            });
        }
        let csum = if ro_compat & ro_compat::METADATA_CSUM != 0 {
            // CRC-32C is the only type there is.
            if sb[at::CHECKSUM_TYPE] != 1 {
                return Err(Error::NotSupported);
            }
            if le32(sb, at::CHECKSUM) != crc32c(!0, &sb[..at::CHECKSUM]) {
                return Err(Error::Corrupt);
            }
            Csum::Crc32c
        } else if ro_compat & ro_compat::GDT_CSUM != 0 {
            Csum::Crc16
        } else {
            Csum::None
        };
        let mut uuid = [0; 16];
        uuid.copy_from_slice(&sb[at::UUID..at::UUID + 16]);
        let mut label = [0; 16];
        label.copy_from_slice(&sb[at::VOLUME_NAME..at::VOLUME_NAME + 16]);
        let seed = if incompat & incompat::CSUM_SEED != 0 {
            le32(sb, at::CHECKSUM_SEED)
        } else {
            crc32c(!0, &uuid)
        };
        let per_block = block_size / desc_size;
        let gdt_blocks = groups.div_ceil(per_block);
        let journal_inum =
            if compat & compat::HAS_JOURNAL != 0 { le32(sb, at::JOURNAL_INUM) } else { 0 };
        let extra_isize = if inode_size > 128 {
            let want = le16(sb, at::WANT_EXTRA_ISIZE).max(le16(sb, at::MIN_EXTRA_ISIZE)).max(32);
            want.min((inode_size - 128) as u16) & !3
        } else {
            0
        };
        let writable = incompat & !WRITE_INCOMPAT == 0
            && incompat & incompat::EXTENTS != 0
            && ro_compat & !WRITE_RO_COMPAT == 0
            && compat & !WRITE_COMPAT == 0
            // An external journal Radix cannot reach.
            && (compat & compat::HAS_JOURNAL == 0 || journal_inum != 0);
        let geo = Geometry {
            block_size,
            shift,
            blocks,
            first_data_block,
            blocks_per_group,
            inodes_per_group,
            inodes,
            groups,
            desc_size,
            inode_size,
            first_ino,
            reserved_gdt: u32::from(le16(sb, at::RESERVED_GDT)),
            first_meta_bg: le32(sb, at::FIRST_META_BG),
            gdt_blocks,
            compat,
            incompat,
            ro_compat,
            csum,
            seed,
            uuid,
            label,
            journal_inum,
            backup_groups: [le32(sb, at::BACKUP_BGS), le32(sb, at::BACKUP_BGS + 4)],
            extra_isize,
            writable,
        };
        // Every descriptor is on the volume: the last group's is in its
        // last block of them.
        if geo.desc_offset(groups - 1) + u64::from(desc_size) > blocks << shift {
            return Err(Error::Corrupt);
        }
        Ok(geo)
    }

    /// Whether it has `incompat` feature `feature`.
    pub fn has_incompat(&self, feature: u32) -> bool {
        self.incompat & feature != 0
    }

    /// Whether it has `ro_compat` feature `feature`.
    pub fn has_ro_compat(&self, feature: u32) -> bool {
        self.ro_compat & feature != 0
    }

    /// Whether directory entries name their file's type.
    pub fn filetype(&self) -> bool {
        self.has_incompat(incompat::FILETYPE)
    }

    /// Whether its metadata has CRC-32C checksums (`metadata_csum`).
    pub fn metadata_csum(&self) -> bool {
        self.csum == Csum::Crc32c
    }

    /// Byte `offset` of block `block`.
    pub fn offset(&self, block: u64) -> u64 {
        block << self.shift
    }

    /// The first block of group `g`.
    pub fn group_start(&self, g: u32) -> u64 {
        self.first_data_block + u64::from(g) * u64::from(self.blocks_per_group)
    }

    /// The blocks group `g` has: all but the last have as many.
    pub fn group_blocks(&self, g: u32) -> u32 {
        (self.blocks - self.group_start(g)).min(u64::from(self.blocks_per_group)) as u32
    }

    /// The group block `block` is in.
    pub fn group_of(&self, block: u64) -> u32 {
        ((block.saturating_sub(self.first_data_block)) / u64::from(self.blocks_per_group)) as u32
    }

    /// Descriptors in a block.
    pub fn descs_per_block(&self) -> u32 {
        self.block_size / self.desc_size
    }

    /// Whether group `g` holds a copy of the superblock.
    pub fn has_super(&self, g: u32) -> bool {
        if g == 0 {
            return true;
        }
        if self.compat & compat::SPARSE_SUPER2 != 0 {
            return self.backup_groups.contains(&g);
        }
        if g == 1 || !self.has_ro_compat(ro_compat::SPARSE_SUPER) {
            return true;
        }
        [3u32, 5, 7].iter().any(|&base| {
            let mut n = base;
            while n < g {
                n = n.saturating_mul(base);
            }
            n == g
        })
    }

    /// Where group `g`'s descriptor is, in bytes.
    pub fn desc_offset(&self, g: u32) -> u64 {
        let per = self.descs_per_block();
        let block = if !self.has_incompat(incompat::META_BG) || g / per < self.first_meta_bg {
            self.first_data_block + 1 + u64::from(g / per)
        } else {
            let first = g / per * per;
            self.group_start(first) + u64::from(self.has_super(first))
        };
        self.offset(block) + u64::from(g % per * self.desc_size)
    }

    /// The blocks at the start of group `g` that hold the superblock's
    /// copy and the descriptors, as Linux's `ext4_num_base_meta_blocks`
    /// counts them: what a group not yet given a bitmap uses.
    pub fn base_blocks(&self, g: u32) -> u32 {
        let per = self.descs_per_block();
        let sup = u32::from(self.has_super(g));
        if !self.has_incompat(incompat::META_BG) || g < self.first_meta_bg.saturating_mul(per) {
            if sup == 0 {
                return 0;
            }
            let descs = if self.has_incompat(incompat::META_BG) {
                self.first_meta_bg
            } else {
                self.gdt_blocks
            };
            sup + descs + self.reserved_gdt
        } else {
            let first = g / per * per;
            sup + u32::from(g == first || g == first + 1 || g == first + per - 1)
        }
    }
}

/// Sets the superblock's checksum, on a volume with `metadata_csum`.
pub fn seal(geo: &Geometry, sb: &mut [u8]) {
    if geo.metadata_csum() {
        let sum = crc32c(!0, &sb[..at::CHECKSUM]);
        put32(sb, at::CHECKSUM, sum);
    }
}

/// Sets the free counts in the superblock `sb`.
pub fn set_free(geo: &Geometry, sb: &mut [u8], blocks: u64, inodes: u32) {
    put32(sb, at::FREE_BLOCKS_LO, blocks as u32);
    if geo.has_incompat(incompat::BIT64) {
        put32(sb, at::FREE_BLOCKS_HI, (blocks >> 32) as u32);
    }
    put32(sb, at::FREE_INODES, inodes);
}

/// Sets or clears `RECOVER`: whether the journal must be replayed.
pub fn set_recover(sb: &mut [u8], on: bool) {
    let mut features = le32(sb, at::INCOMPAT);
    if on {
        features |= incompat::RECOVER;
    } else {
        features &= !incompat::RECOVER;
    }
    put32(sb, at::INCOMPAT, features);
}

/// Sets `s_state`.
pub fn set_state(sb: &mut [u8], state: u16) {
    put16(sb, at::STATE, state);
}
