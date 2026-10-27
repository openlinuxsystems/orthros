// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Directory entries: `DIR_ITEM`s, keyed by the hash of their names
//! ([`crate::format::name_hash`]), for lookups, and `DIR_INDEX`es, keyed
//! by the order they were made in, for listings. A `DIR_ITEM` holds every
//! name of the directory with its hash, one after the other; a
//! `DIR_INDEX` holds one.

use radix_vfs::{Error, Kind, Result};

use crate::format::{Key, u16_at};

/// The size of an entry before its name.
const HEADER: usize = 30;

/// The longest name, in bytes.
pub const NAME_MAX: usize = 255;

/// An entry of a directory.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Entry<'a> {
    /// What it names: an inode of the same subvolume (`INODE_ITEM`), or a
    /// subvolume (`ROOT_ITEM`), by its tree's number.
    pub location: Key,
    /// Its kind, as the entry says.
    pub kind: Option<Kind>,
    /// Its name.
    pub name: &'a [u8],
}

/// The entries in the item `bytes`.
pub struct Entries<'a> {
    bytes: &'a [u8],
}

impl<'a> Entries<'a> {
    /// The entries of the `DIR_ITEM` or `DIR_INDEX` item `bytes`.
    pub fn of(bytes: &'a [u8]) -> Entries<'a> {
        Entries { bytes }
    }
}

impl<'a> Iterator for Entries<'a> {
    type Item = Result<Entry<'a>>;

    fn next(&mut self) -> Option<Result<Entry<'a>>> {
        if self.bytes.is_empty() {
            return None;
        }
        let b = self.bytes;
        let data_len = usize::from(u16_at(b, 25));
        let name_len = usize::from(u16_at(b, 27));
        let end = HEADER + name_len + data_len;
        if b.len() < end || name_len == 0 || name_len > NAME_MAX {
            self.bytes = &[];
            return Some(Err(Error::Corrupt));
        }
        let entry = Entry {
            location: Key::read(b, 0),
            kind: kind_of(b[29]),
            name: &b[HEADER..HEADER + name_len],
        };
        self.bytes = &b[end..];
        Some(Ok(entry))
    }
}

/// The kind an entry's type byte names: Linux's `BTRFS_FT_*`, the same
/// numbers as `DT_*` shifted down.
fn kind_of(byte: u8) -> Option<Kind> {
    Some(match byte {
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
