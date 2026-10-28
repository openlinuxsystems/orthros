// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Directory blocks.
//!
//! A directory's blocks hold its entries one after another: each the
//! inode, the entry's length (`rec_len`, which reaches the next), the
//! name's length, the file's type (`filetype`), and the name. A removed
//! entry's room joins the one before it, or, first in its block, is kept
//! with inode 0. With `metadata_csum` each block ends in a 12-byte tail:
//! an entry of inode 0 and type `0xde` holding the CRC-32C of the block
//! before it, from the inode's seed.
//!
//! A large directory may have a hash index (`dir_index`): its first block
//! holds `.` and `..` and the index's root inside `..`'s room, and some
//! later blocks are index nodes, each one empty entry the size of the
//! block with the index inside. Read in order, as Radix reads them, they
//! hold no entries, so an indexed directory reads like any other. Radix
//! keeps no index: before it changes an indexed directory it turns those
//! blocks into empty ones and clears the inode's `INDEX` flag, as
//! `e2fsck` would leave a directory it was told not to index; Linux reads
//! it as it is, and indexes it again only with `e2fsck -D`.

use radix_vfs::{Error, Result};

use crate::crc::crc32c;
use crate::{le16, le32, put16, put32};

/// The bytes of a block's checksum tail.
pub const TAIL: usize = 12;
/// The type the tail's entry has.
const TAIL_TYPE: u8 = 0xde;

/// An entry of a directory block.
#[derive(Clone, Copy, Debug)]
pub struct Entry<'a> {
    pub offset: usize,
    pub ino: u32,
    pub rec_len: usize,
    pub name: &'a [u8],
    /// The file's type, 0 if the volume keeps none.
    pub kind: u8,
}

/// The room an entry with a name of `len` bytes needs.
pub fn rec_len(len: usize) -> usize {
    (8 + len + 3) & !3
}

/// Whether block `b` ends in a checksum tail.
pub fn has_tail(b: &[u8]) -> bool {
    let at = b.len() - TAIL;
    le32(b, at) == 0 && le16(b, at + 4) as usize == TAIL && b[at + 6] == 0 && b[at + 7] == TAIL_TYPE
}

/// Where block `b`'s entries end: before its tail, if it has one.
pub fn end_of(b: &[u8]) -> usize {
    if has_tail(b) { b.len() - TAIL } else { b.len() }
}

/// The entry at `offset` of `b`, which must hold one.
fn entry_at(b: &[u8], offset: usize, filetype: bool) -> Entry<'_> {
    let len = usize::from(b[offset + 6]);
    Entry {
        offset,
        ino: le32(b, offset),
        rec_len: usize::from(le16(b, offset + 4)),
        name: &b[offset + 8..offset + 8 + len],
        kind: if filetype { b[offset + 7] } else { 0 },
    }
}

/// Checks block `b`: its checksum against `seed` if it has a tail and
/// `seed` is given, and that its entries fill it exactly; an entry naming
/// an inode past `inodes` is damage too.
pub fn check(b: &[u8], seed: Option<u32>, inodes: u32) -> Result<()> {
    let end = end_of(b);
    if let Some(seed) = seed
        && end != b.len()
        && crc32c(seed, &b[..end]) != le32(b, b.len() - 4)
    {
        return Err(Error::Corrupt);
    }
    let mut at = 0;
    while at < end {
        if at + 8 > end {
            return Err(Error::Corrupt);
        }
        let rec = usize::from(le16(b, at + 4));
        let len = usize::from(b[at + 6]);
        if rec < 8 || rec % 4 != 0 || at + rec > end || 8 + len > rec || le32(b, at) > inodes {
            return Err(Error::Corrupt);
        }
        at += rec;
    }
    Ok(())
}

/// Calls `f` with each entry of `b` that names a file, in order, until it
/// returns false. `b` passed [`check`].
pub fn for_each<'a>(b: &'a [u8], filetype: bool, mut f: impl FnMut(Entry<'a>) -> bool) {
    let end = end_of(b);
    let mut at = 0;
    while at < end {
        let e = entry_at(b, at, filetype);
        if e.rec_len == 0 {
            return;
        }
        if e.ino != 0 && !f(e) {
            return;
        }
        at += e.rec_len;
    }
}

/// The entry of `b` named `name`.
pub fn find<'a>(b: &'a [u8], filetype: bool, name: &[u8]) -> Option<Entry<'a>> {
    let mut found = None;
    for_each(b, filetype, |e| {
        if e.name == name {
            found = Some(e);
        }
        found.is_none()
    });
    found
}

/// Writes an entry at `at` of `b`.
fn put_entry(b: &mut [u8], at: usize, ino: u32, rec: usize, name: &[u8], kind: u8, filetype: bool) {
    put32(b, at, ino);
    put16(b, at + 4, rec as u16);
    b[at + 6] = name.len() as u8;
    b[at + 7] = if filetype { kind } else { 0 };
    b[at + 8..at + 8 + name.len()].copy_from_slice(name);
    let pad = rec_len(name.len()).min(rec);
    b[at + 8 + name.len()..at + pad].fill(0);
}

/// Adds an entry naming inode `ino` `name` to `b`, in the first room that
/// fits it; returns whether there was room.
pub fn insert(b: &mut [u8], filetype: bool, name: &[u8], ino: u32, kind: u8) -> bool {
    let need = rec_len(name.len());
    let end = end_of(b);
    let mut at = 0;
    while at < end {
        let e = entry_at(b, at, filetype);
        let rec = e.rec_len;
        if rec == 0 {
            return false;
        }
        let used = if e.ino == 0 { 0 } else { rec_len(e.name.len()) };
        if rec - used >= need {
            if used == 0 {
                put_entry(b, at, ino, rec, name, kind, filetype);
            } else {
                put16(b, at + 4, used as u16);
                put_entry(b, at + used, ino, rec - used, name, kind, filetype);
            }
            return true;
        }
        at += rec;
    }
    false
}

/// Removes the entry at `offset` of `b`: its room joins the entry before
/// it, or, first in the block, it is kept with inode 0.
pub fn remove(b: &mut [u8], filetype: bool, offset: usize) {
    let end = end_of(b);
    let mut at = 0;
    let mut prev = None;
    while at < end && at < offset {
        let rec = entry_at(b, at, filetype).rec_len;
        if rec == 0 {
            return;
        }
        prev = Some(at);
        at += rec;
    }
    if at != offset {
        return;
    }
    let rec = entry_at(b, offset, filetype).rec_len;
    match prev {
        Some(p) => {
            let joined = entry_at(b, p, filetype).rec_len + rec;
            put16(b, p + 4, joined as u16);
        }
        None => put32(b, offset, 0),
    }
}

/// Sets the inode the entry at `offset` names, and its type: a rename
/// over an existing name.
pub fn retarget(b: &mut [u8], filetype: bool, offset: usize, ino: u32, kind: u8) {
    put32(b, offset, ino);
    if filetype {
        b[offset + 7] = kind;
    }
}

/// Makes `b` an empty block: one entry of inode 0 over it all, and a
/// tail if `tail`.
pub fn empty(b: &mut [u8], tail: bool) {
    b.fill(0);
    let end = b.len() - if tail { TAIL } else { 0 };
    put16(b, 4, end as u16);
    if tail {
        put_tail(b);
    }
}

fn put_tail(b: &mut [u8]) {
    let at = b.len() - TAIL;
    b[at..].fill(0);
    put16(b, at + 4, TAIL as u16);
    b[at + 7] = TAIL_TYPE;
}

/// Makes `b` the first block of a new directory, inode `ino`, in the
/// directory `parent`.
pub fn first(b: &mut [u8], filetype: bool, ino: u32, parent: u32, tail: bool) {
    b.fill(0);
    let end = b.len() - if tail { TAIL } else { 0 };
    put_entry(b, 0, ino, 12, b".", 2, filetype);
    put_entry(b, 12, parent, end - 12, b"..", 2, filetype);
    if tail {
        put_tail(b);
    }
}

/// The inode `..` names in a directory's first block `b`.
pub fn parent_of(b: &[u8], filetype: bool) -> Option<u32> {
    let dot = entry_at(b, 0, filetype);
    if dot.name != b"." || dot.rec_len < 12 || dot.rec_len + 12 > b.len() {
        return None;
    }
    let dotdot = entry_at(b, dot.rec_len, filetype);
    (dotdot.name == b"..").then_some(dotdot.ino)
}

/// Sets the inode `..` names in a directory's first block `b`.
pub fn set_parent(b: &mut [u8], filetype: bool, parent: u32) -> Result<()> {
    let dot = entry_at(b, 0, filetype);
    if dot.name != b"." || dot.rec_len + 12 > b.len() {
        return Err(Error::Corrupt);
    }
    put32(b, dot.rec_len, parent);
    Ok(())
}

/// Sets block `b`'s checksum from the inode's `seed`, if it has a tail.
pub fn seal(b: &mut [u8], seed: u32) {
    if has_tail(b) {
        let at = b.len() - TAIL;
        let sum = crc32c(seed, &b[..at]);
        put32(b, b.len() - 4, sum);
    }
}

/// Whether block `b` of an indexed directory is an index node: one empty
/// entry over the whole block. (Without checksums an emptied leaf looks
/// the same; either holds no entries.)
pub fn is_index_node(b: &[u8]) -> bool {
    le32(b, 0) == 0 && usize::from(le16(b, 4)) == b.len()
}

/// Checks the first block `b` of an indexed directory: `.`, `..` over
/// the rest, and the index in it, against `seed` if given.
pub fn check_root(b: &[u8], seed: Option<u32>) -> Result<()> {
    let bs = b.len();
    let info = usize::from(b[24 + 5]);
    if usize::from(le16(b, 4)) != 12 || usize::from(le16(b, 12 + 4)) != bs - 12 || info != 8 {
        return Err(Error::Corrupt);
    }
    match seed {
        Some(seed) => check_index(b, 24 + info, seed),
        None => Ok(()),
    }
}

/// Checks an index node `b` of a directory against `seed`.
pub fn check_node(b: &[u8], seed: u32) -> Result<()> {
    check_index(b, 8, seed)
}

/// Checks the checksum of the index in `b` whose count and limit are at
/// `count_offset`: over the entries in use, then the tail's first word
/// and a zero word, as Linux sums it.
fn check_index(b: &[u8], count_offset: usize, seed: u32) -> Result<()> {
    let limit = usize::from(le16(b, count_offset));
    let count = usize::from(le16(b, count_offset + 2));
    let tail = count_offset + limit * 8;
    if count > limit || tail + 8 > b.len() {
        return Err(Error::Corrupt);
    }
    let sum = crc32c(seed, &b[..count_offset + count * 8]);
    let sum = crc32c(sum, &b[tail..tail + 4]);
    let sum = crc32c(sum, &[0; 4]);
    if sum != le32(b, tail + 4) {
        return Err(Error::Corrupt);
    }
    Ok(())
}

/// Turns the first block `b` of an indexed directory into a plain one:
/// `.` and `..`, `..` over the rest but the tail if `tail`.
pub fn unindex_root(b: &mut [u8], filetype: bool, tail: bool) -> Result<()> {
    let (ino, parent) = {
        let dot = entry_at(b, 0, filetype);
        let parent = parent_of(b, filetype).ok_or(Error::Corrupt)?;
        (dot.ino, parent)
    };
    first(b, filetype, ino, parent, tail);
    Ok(())
}

#[cfg(kani)]
mod proofs {
    use super::*;

    /// A directory block that passes [`check`] is read without a fault:
    /// every entry lies in it, its name too.
    #[kani::proof]
    #[kani::unwind(10)]
    fn checked_blocks_read_safely() {
        let b: [u8; 64] = kani::any();
        let filetype: bool = kani::any();
        if check(&b, None, kani::any()).is_ok() {
            for_each(&b, filetype, |e| {
                assert!(e.offset + e.rec_len <= b.len());
                assert!(8 + e.name.len() <= e.rec_len);
                true
            });
            let name: [u8; 3] = kani::any();
            let _ = find(&b, filetype, &name);
        }
    }
}
