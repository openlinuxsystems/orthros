// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Directory indexes: B+ trees of file names.
//!
//! A directory's entries are an index named `$I30`, sorted by name as the
//! up-case table orders them ([`crate::upcase`]). Its root is an
//! attribute in the directory's record (`$INDEX_ROOT`); a directory with
//! more entries than fit there has index blocks (`INDX` records, guarded
//! by an update sequence as file records are) in `$INDEX_ALLOCATION`,
//! and `$BITMAP` says which blocks are in use. Each node holds entries in
//! order, every one a real entry, with the reference of its file and a
//! copy of its `$FILE_NAME` as the key; an entry may point to the block
//! of the names before it, and the last, keyless entry to the block of
//! those after the rest.
//!
//! A lookup goes down the tree; a listing walks it in order
//! ([`crate::tree::walk`]).

use core::cmp::Ordering;

use radix_vfs::{Error, Result};

use crate::record::{u16_at, u32_at, u64_at};
use crate::upcase::Upcase;

/// The most UTF-16 units in a name.
pub const NAME_MAX: usize = 255;

/// The deepest tree Radix walks: more is a loop.
const MAX_DEPTH: usize = 32;

/// Where an index root's node header is in its value.
pub const ROOT_HEADER: usize = 0x10;

/// Where an index block's node header is.
pub const BLOCK_HEADER: usize = 0x18;

/// Bits of an index entry's flags.
mod entry_flags {
    /// It points to the block of the names before it.
    pub const SUBNODE: u16 = 1;
    /// It is the node's last, keyless entry.
    pub const LAST: u16 = 2;
}

/// The namespaces of names ([`FileName::namespace`]).
pub mod namespace {
    /// Any units but NUL and `/`, case kept and told apart.
    pub const POSIX: u8 = 0;
    /// A long name, as Windows makes them.
    pub const WIN32: u8 = 1;
    /// An 8.3 short name, kept beside a long one.
    pub const DOS: u8 = 2;
    /// A long name that is a valid short name too.
    pub const WIN32_AND_DOS: u8 = 3;
}

/// Bits of a name's file attributes ([`FileName::attributes`]), as
/// `$STANDARD_INFORMATION` keeps them too.
pub mod attributes {
    /// Read-only.
    pub const READ_ONLY: u32 = 0x0001;
    /// Hidden.
    pub const HIDDEN: u32 = 0x0002;
    /// A file of the system.
    pub const SYSTEM: u32 = 0x0004;
    /// Changed since a backup last cleared it.
    pub const ARCHIVE: u32 = 0x0020;
    /// The file has a reparse point: it is a link, a mount point or the
    /// like.
    pub const REPARSE_POINT: u32 = 0x0400;
    /// Its data is compressed.
    pub const COMPRESSED: u32 = 0x0800;
    /// Its data is not here: it is in the cloud, or on tape.
    pub const OFFLINE: u32 = 0x1000;
    /// Its data is encrypted.
    pub const ENCRYPTED: u32 = 0x4000;
    /// Opening it brings its data back from elsewhere.
    pub const RECALL_ON_OPEN: u32 = 0x0004_0000;
    /// Reading it brings its data back from elsewhere.
    pub const RECALL_ON_DATA_ACCESS: u32 = 0x0040_0000;
    /// The file is a directory (in `$FILE_NAME` alone).
    pub const DIRECTORY: u32 = 0x1000_0000;
}

/// A `$FILE_NAME` value: a name of a file, in a directory.
#[derive(Clone, Copy)]
pub struct FileName<'a> {
    bytes: &'a [u8],
}

impl<'a> FileName<'a> {
    /// The value in `bytes`, checked.
    pub fn new(bytes: &'a [u8]) -> Result<FileName<'a>> {
        if bytes.len() < 0x42 || bytes.len() < 0x42 + 2 * usize::from(bytes[0x40]) {
            return Err(Error::Corrupt);
        }
        Ok(FileName { bytes })
    }

    /// The file reference of its directory.
    pub fn parent(&self) -> u64 {
        u64_at(self.bytes, 0)
    }

    /// The file's attributes as the name was last updated with them.
    pub fn attributes(&self) -> u32 {
        u32_at(self.bytes, 0x38)
    }

    /// The reparse point's tag, for a file with one.
    pub fn reparse_tag(&self) -> u32 {
        u32_at(self.bytes, 0x3c)
    }

    /// Its namespace ([`namespace`]).
    pub fn namespace(&self) -> u8 {
        self.bytes[0x41]
    }

    /// The name's UTF-16 units.
    pub fn units(&self) -> impl Iterator<Item = u16> + Clone + 'a {
        let len = usize::from(self.bytes[0x40]);
        self.bytes[0x42..0x42 + 2 * len].as_chunks::<2>().0.iter().map(|u| u16::from_le_bytes(*u))
    }

    /// How many units the name has.
    pub fn len(&self) -> usize {
        usize::from(self.bytes[0x40])
    }

    /// Whether the name is empty, which no real name is.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// An index entry.
#[derive(Clone, Copy)]
pub struct Entry<'a> {
    /// Where it starts in its node's bytes.
    pub offset: usize,
    bytes: &'a [u8],
}

impl<'a> Entry<'a> {
    /// The file reference of its file.
    pub fn reference(&self) -> u64 {
        u64_at(self.bytes, 0)
    }

    /// Whether it is its node's last, keyless entry.
    pub fn is_last(&self) -> bool {
        u16_at(self.bytes, 0x0c) & entry_flags::LAST != 0
    }

    /// The VCN of the block it points to, if any.
    pub fn subnode(&self) -> Option<u64> {
        let flags = u16_at(self.bytes, 0x0c);
        (flags & entry_flags::SUBNODE != 0).then(|| u64_at(self.bytes, self.bytes.len() - 8))
    }

    /// Its key, the name.
    pub fn name(&self) -> Result<FileName<'a>> {
        let len = usize::from(u16_at(self.bytes, 0x0a));
        FileName::new(&self.bytes[0x10..0x10 + len])
    }

    /// Its length, so where the next entry starts.
    pub fn len(&self) -> usize {
        self.bytes.len()
    }

    /// Whether it has no bytes, which no entry has.
    pub fn is_empty(&self) -> bool {
        self.bytes.is_empty()
    }
}

/// The entries of the node in `node` whose header is at `header`, in
/// order, up to its last; a malformed entry ends them with an error.
pub fn entries(node: &[u8], header: usize) -> impl Iterator<Item = Result<Entry<'_>>> {
    let bounds = (|| {
        let h = node.get(header..header + 16)?;
        let first = header.checked_add(u32_at(h, 0) as usize)?;
        let end = header.checked_add(u32_at(h, 4) as usize)?;
        (first <= end && end <= node.len()).then_some((first, end))
    })();
    let (mut at, end) = bounds.unwrap_or((0, 0));
    let mut done = false;
    let failed = bounds.is_none();
    core::iter::from_fn(move || {
        if done {
            return None;
        }
        if failed {
            done = true;
            return Some(Err(Error::Corrupt));
        }
        let rest = &node[at..end];
        let entry = (|| {
            if rest.len() < 0x10 {
                return None;
            }
            let len = usize::from(u16_at(rest, 8));
            let key = usize::from(u16_at(rest, 0x0a));
            let flags = u16_at(rest, 0x0c);
            let tail = if flags & entry_flags::SUBNODE != 0 { 8 } else { 0 };
            if len % 8 != 0 || len > rest.len() || 0x10 + key + tail > len {
                return None;
            }
            Some(Entry { offset: at, bytes: &rest[..len] })
        })();
        match entry {
            Some(entry) => {
                at += entry.len();
                if entry.is_last() {
                    done = true;
                }
                Some(Ok(entry))
            }
            None => {
                done = true;
                Some(Err(Error::Corrupt))
            }
        }
    })
}

/// What a lookup found: the file reference of the name, and whether its
/// case was the one asked for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Found {
    /// The file reference of its file.
    pub reference: u64,
    /// Whether the name's case is the one asked for.
    pub exact: bool,
}

/// Reads an index block: `read(vcn, buf)` fills `buf` with the block at
/// VCN `vcn`, its update sequence put back.
pub type ReadBlock<'a> = &'a mut dyn FnMut(u64, &mut [u8]) -> Result<()>;

/// Finds `name` in the index whose root's value is `root`: a name the
/// same as it but for case, as the up-case table says, and the one of
/// exactly its case when there are more. `buf` holds a block.
pub fn find(
    root: &[u8],
    upcase: &Upcase,
    name: &[u16],
    read: ReadBlock<'_>,
    buf: &mut [u8],
) -> Result<Option<Found>> {
    let mut found = None;
    let mut next = descend(root, ROOT_HEADER, upcase, name, &mut found)?;
    let mut depth = 0;
    while let Some(vcn) = next {
        if found.is_some_and(|f: Found| f.exact) {
            break;
        }
        depth += 1;
        if depth > MAX_DEPTH {
            return Err(Error::Corrupt);
        }
        read(vcn, buf)?;
        next = descend(buf, BLOCK_HEADER, upcase, name, &mut found)?;
    }
    Ok(found)
}

/// Looks for `name` among the entries of one node; returns the block to
/// look in next, if any. Each name the same but for case goes in `found`
/// unless one is there; the exact name replaces it, and ends the search.
fn descend(
    node: &[u8],
    header: usize,
    upcase: &Upcase,
    name: &[u16],
    found: &mut Option<Found>,
) -> Result<Option<u64>> {
    for entry in entries(node, header) {
        let entry = entry?;
        if entry.is_last() {
            return Ok(entry.subnode());
        }
        let key = entry.name()?;
        let order = match upcase.compare(name.iter().copied(), key.units()) {
            Ordering::Equal => {
                if key.units().eq(name.iter().copied()) {
                    *found = Some(Found { reference: entry.reference(), exact: true });
                    return Ok(None);
                }
                if found.is_none() {
                    *found = Some(Found { reference: entry.reference(), exact: false });
                }
                // Names the same but for case are in the order of their
                // units.
                name.iter().copied().cmp(key.units())
            }
            order => order,
        };
        if order == Ordering::Less {
            return Ok(entry.subnode());
        }
    }
    // A node without its last entry.
    Err(Error::Corrupt)
}
