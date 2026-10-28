// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Inode items: what Btrfs keeps of each file, directory and link, in the
//! tree of its subvolume, under its inode number (`INODE_ITEM`).

use radix_vfs::{Error, Kind, Result};

use crate::format::{u32_at, u64_at};

/// The size of an inode item.
pub const SIZE: usize = 160;

/// The object ID of a subvolume's top directory.
pub const FIRST: u64 = 256;

/// Inode flags Radix reads.
pub mod flags {
    /// The file's data has no checksums.
    pub const NODATASUM: u64 = 1 << 0;
    /// The file's data is written in place; such files are made with
    /// `NODATASUM` too.
    pub const NODATACOW: u64 = 1 << 1;
    /// The file's data is never compressed (`chattr +m`).
    pub const NOCOMPRESS: u64 = 1 << 3;
    /// The file's data is compressed, whatever the mount says
    /// (`chattr +c`).
    pub const COMPRESS: u64 = 1 << 11;
}

/// The file type bits of a mode, and the types, as POSIX numbers them.
mod mode {
    pub const TYPE: u32 = 0o170_000;
    pub const FIFO: u32 = 0o010_000;
    pub const CHAR: u32 = 0o020_000;
    pub const DIR: u32 = 0o040_000;
    pub const BLOCK: u32 = 0o060_000;
    pub const FILE: u32 = 0o100_000;
    pub const LINK: u32 = 0o120_000;
    pub const SOCKET: u32 = 0o140_000;
}

/// An inode item.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Inode {
    /// The file's size in bytes: a directory's is the length of its
    /// names, twice.
    pub size: u64,
    /// The bytes its extents take on the disk.
    pub nbytes: u64,
    /// How many names it has.
    pub nlink: u32,
    /// Its owner and group.
    pub uid: u32,
    /// Its group.
    pub gid: u32,
    /// Its type and permissions, as POSIX's `st_mode`.
    pub mode: u32,
    /// Its device number, for a device.
    pub rdev: u64,
    /// Its [`flags`].
    pub flags: u64,
    /// When it was last read, in nanoseconds since 1970.
    pub atime: u64,
    /// When it last changed, contents or not.
    pub ctime: u64,
    /// When its contents last changed.
    pub mtime: u64,
    /// When it was made.
    pub otime: u64,
}

/// The time at `at` in an inode item, a `btrfs_timespec` (seconds, then
/// nanoseconds), in nanoseconds since 1970; times before 1970 are 0.
fn time_at(bytes: &[u8], at: usize) -> u64 {
    let secs = u64_at(bytes, at);
    let nanos = u64::from(u32_at(bytes, at + 8)).min(999_999_999);
    if secs > i64::MAX as u64 {
        return 0;
    }
    secs.saturating_mul(1_000_000_000).saturating_add(nanos)
}

impl Inode {
    /// The inode item in `bytes`.
    pub fn read(bytes: &[u8]) -> Result<Inode> {
        if bytes.len() < SIZE {
            return Err(Error::Corrupt);
        }
        Ok(Inode {
            size: u64_at(bytes, 16),
            nbytes: u64_at(bytes, 24),
            nlink: u32_at(bytes, 40),
            uid: u32_at(bytes, 44),
            gid: u32_at(bytes, 48),
            mode: u32_at(bytes, 52),
            rdev: u64_at(bytes, 56),
            flags: u64_at(bytes, 64),
            atime: time_at(bytes, 112),
            ctime: time_at(bytes, 124),
            mtime: time_at(bytes, 136),
            otime: time_at(bytes, 148),
        })
    }

    /// What kind of node it is.
    pub fn kind(&self) -> Result<Kind> {
        Ok(match self.mode & mode::TYPE {
            mode::FILE => Kind::File,
            mode::DIR => Kind::Directory,
            mode::LINK => Kind::Symlink,
            mode::CHAR => Kind::CharDevice,
            mode::BLOCK => Kind::BlockDevice,
            mode::FIFO => Kind::Pipe,
            mode::SOCKET => Kind::Socket,
            _ => return Err(Error::Corrupt),
        })
    }

    /// Whether its data has checksums: as on Linux, unless `NODATASUM`
    /// says otherwise (which `NODATACOW` files are made with).
    pub fn has_sums(&self) -> bool {
        self.flags & flags::NODATASUM == 0
    }
}
