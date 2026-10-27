// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! What every part of Btrfs's on-disk format shares: little-endian
//! numbers at fixed offsets, the key every item is sorted by, and the
//! numbers of the trees and item types Radix reads.

/// The little-endian `u16` at `at` in `bytes`, or 0 past its end.
pub fn u16_at(bytes: &[u8], at: usize) -> u16 {
    let mut word = [0; 2];
    if let Some(src) = bytes.get(at..at + 2) {
        word.copy_from_slice(src);
    }
    u16::from_le_bytes(word)
}

/// The little-endian `u32` at `at` in `bytes`, or 0 past its end.
pub fn u32_at(bytes: &[u8], at: usize) -> u32 {
    let mut word = [0; 4];
    if let Some(src) = bytes.get(at..at + 4) {
        word.copy_from_slice(src);
    }
    u32::from_le_bytes(word)
}

/// The little-endian `u64` at `at` in `bytes`, or 0 past its end.
pub fn u64_at(bytes: &[u8], at: usize) -> u64 {
    let mut word = [0; 8];
    if let Some(src) = bytes.get(at..at + 8) {
        word.copy_from_slice(src);
    }
    u64::from_le_bytes(word)
}

/// The 16 bytes at `at` in `bytes` (a UUID), or zeros past its end.
pub fn uuid_at(bytes: &[u8], at: usize) -> [u8; 16] {
    let mut uuid = [0; 16];
    if let Some(src) = bytes.get(at..at + 16) {
        uuid.copy_from_slice(src);
    }
    uuid
}

/// The hash of a name in a directory: the offset of its `DIR_ITEM` key
/// and of its `XATTR_ITEM` key.
pub fn name_hash(name: &[u8]) -> u64 {
    u64::from(crate::csum::crc32c_update(!1, name))
}

/// A key: what every item of every tree is found and sorted by, in the
/// order of its three fields.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Key {
    /// What the item is about: an inode, a tree, a chunk's owner.
    pub objectid: u64,
    /// The item's type ([`item`]).
    pub kind: u8,
    /// A number whose meaning the type gives.
    pub offset: u64,
}

impl Key {
    /// A key's size on the disk.
    pub const SIZE: usize = 17;

    /// The key whose fields are these.
    pub const fn new(objectid: u64, kind: u8, offset: u64) -> Key {
        Key { objectid, kind, offset }
    }

    /// The key at `at` in `bytes`.
    pub fn read(bytes: &[u8], at: usize) -> Key {
        Key {
            objectid: u64_at(bytes, at),
            kind: bytes.get(at + 8).copied().unwrap_or(0),
            offset: u64_at(bytes, at + 9),
        }
    }

    /// The smallest key of `objectid` and `kind`.
    pub const fn first(objectid: u64, kind: u8) -> Key {
        Key::new(objectid, kind, 0)
    }

    /// The largest key of `objectid` and `kind`.
    pub const fn last(objectid: u64, kind: u8) -> Key {
        Key::new(objectid, kind, u64::MAX)
    }

    /// Whether it has `objectid` and `kind`, whatever its offset.
    pub fn is(&self, objectid: u64, kind: u8) -> bool {
        self.objectid == objectid && self.kind == kind
    }
}

/// The trees, by the object ID of their root items in the root tree.
pub mod tree {
    /// The root tree: every other tree's root item.
    pub const ROOT: u64 = 1;
    /// The extent tree: which extents are allocated, and who refers to
    /// them.
    pub const EXTENT: u64 = 2;
    /// The chunk tree: where each range of logical addresses lies on the
    /// devices.
    pub const CHUNK: u64 = 3;
    /// The device tree: each device's allocated ranges.
    pub const DEV: u64 = 4;
    /// The top-level subvolume, the filesystem tree mounted by default.
    pub const FS: u64 = 5;
    /// The directory in the root tree that names the default subvolume.
    pub const ROOT_TREE_DIR: u64 = 6;
    /// The checksums of file data.
    pub const CSUM: u64 = 7;
    /// The quota tree.
    pub const QUOTA: u64 = 8;
    /// Subvolumes by UUID.
    pub const UUID: u64 = 9;
    /// The free-space tree.
    pub const FREE_SPACE: u64 = 10;
    /// The block-group tree (`block-group-tree` feature).
    pub const BLOCK_GROUP: u64 = 11;
    /// The first object ID of a subvolume, or of an inode in one.
    pub const FIRST_FREE: u64 = 256;
    /// The object ID of every chunk item in the chunk tree.
    pub const FIRST_CHUNK: u64 = 256;
    /// The object ID of every checksum item in the checksum tree.
    pub const EXTENT_CSUM: u64 = (-10i64) as u64;
}

/// Item types, the middle field of a key.
pub mod item {
    /// An inode: its size, mode, owner, times.
    pub const INODE_ITEM: u8 = 1;
    /// A name of an inode in its directory, by the directory's inode.
    pub const INODE_REF: u8 = 12;
    /// A name of an inode, when too many names hash alike for `INODE_REF`.
    pub const INODE_EXTREF: u8 = 13;
    /// An extended attribute, by its name's hash.
    pub const XATTR_ITEM: u8 = 24;
    /// A directory entry, by its name's hash ([`name_hash`]).
    pub const DIR_ITEM: u8 = 84;
    /// A directory entry, by the order it was made in.
    pub const DIR_INDEX: u8 = 96;
    /// Where a file's bytes from an offset are.
    pub const EXTENT_DATA: u8 = 108;
    /// Checksums of data sectors, by their logical address.
    pub const EXTENT_CSUM: u8 = 128;
    /// A tree's root block, in the root tree.
    pub const ROOT_ITEM: u8 = 132;
    /// A subvolume's name, by the subvolume.
    pub const ROOT_BACKREF: u8 = 144;
    /// A subvolume's name, by the subvolume holding it.
    pub const ROOT_REF: u8 = 156;
    /// An allocated extent and who refers to it.
    pub const EXTENT_ITEM: u8 = 168;
    /// An allocated tree block and who refers to it.
    pub const METADATA_ITEM: u8 = 169;
    /// A tree block's reference from the tree that owns it, inline in its
    /// extent item or an item of its own.
    pub const TREE_BLOCK_REF: u8 = 176;
    /// A data extent's reference from a file, inline or an item.
    pub const EXTENT_DATA_REF: u8 = 178;
    /// A tree block's reference from the block above it (snapshots).
    pub const SHARED_BLOCK_REF: u8 = 182;
    /// A data extent's reference from the leaf that holds it (snapshots).
    pub const SHARED_DATA_REF: u8 = 184;
    /// A chunk's use: how many of its bytes are allocated.
    pub const BLOCK_GROUP_ITEM: u8 = 192;
    /// A block group's entry in the free-space tree: how its free space
    /// is kept.
    pub const FREE_SPACE_INFO: u8 = 198;
    /// A free range of a block group.
    pub const FREE_SPACE_EXTENT: u8 = 199;
    /// A bitmap of a block group's free sectors.
    pub const FREE_SPACE_BITMAP: u8 = 200;
    /// A range of a device that a chunk lies on.
    pub const DEV_EXTENT: u8 = 204;
    /// A device.
    pub const DEV_ITEM: u8 = 216;
    /// A chunk ([`crate::chunk`]).
    pub const CHUNK_ITEM: u8 = 228;
}

/// A root item (`ROOT_ITEM` in the root tree): where a tree's root block
/// is, and how far down it reaches.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RootItem {
    /// The transaction that last wrote the tree.
    pub generation: u64,
    /// The object ID of the tree's top directory, for a subvolume.
    pub root_dirid: u64,
    /// The logical address of its root block.
    pub bytenr: u64,
    /// How many directory entries name it, for a subvolume: 0 once it is
    /// deleted.
    pub refs: u32,
    /// The level of its root block: 0 for a leaf.
    pub level: u8,
}

impl RootItem {
    /// The bytes a root item needs, up to its root block's level; newer
    /// ones are longer.
    pub const SIZE: usize = 239;

    /// The root item in `bytes`, if it is long enough to be one.
    pub fn read(bytes: &[u8]) -> Option<RootItem> {
        // After an inode item of 160 bytes: generation, root_dirid,
        // bytenr, byte_limit, bytes_used, last_snapshot, flags, refs,
        // drop_progress (a key) and drop_level, then level.
        (bytes.len() >= RootItem::SIZE).then(|| RootItem {
            generation: u64_at(bytes, 160),
            root_dirid: u64_at(bytes, 168),
            bytenr: u64_at(bytes, 176),
            refs: u32_at(bytes, 216),
            level: bytes[238],
        })
    }
}
