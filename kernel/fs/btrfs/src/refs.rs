// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Back references: who refers to each extent, in the extent tree, as
//! Linux keeps them (ADR 0138).
//!
//! An extent's item counts its references and lists them after its
//! header, as inline references, while they fit in a sixteenth of a
//! leaf; past that each is an item of its own, keyed by the extent. A
//! tree block is referred to either by a tree, which means by whichever
//! block of that tree points at it ([`Backref::Tree`]), or, once the
//! block above it is shared by several trees and marked
//! [`FULL_BACKREF`], by that block's address ([`Backref::Shared`]); a
//! data extent is referred to by a file's offset in a tree
//! ([`Backref::Data`]), or by the leaf that holds the file extent item
//! ([`Backref::SharedData`]), each with a count. Inline references are
//! kept by type, and within a type from the highest key down, where
//! Linux looks for them.

use radix_vfs::mem::{self, KVec};
use radix_vfs::{Error, Result};

use crate::cow::Writer;
use crate::csum::crc32c_update;
use crate::files::{put_u32, put_u64};
use crate::format::{Key, item, tree, u32_at, u64_at};
use crate::tree::{Cursor, HEADER_SIZE, ITEM_SIZE};
use crate::volume::Volume;

/// An extent item's header: references, generation and flags.
pub(crate) const EXTENT_HEADER: usize = 24;

/// The flag of a tree block whose children are referred to by its
/// address rather than by its tree ([`Backref::Shared`]).
pub(crate) const FULL_BACKREF: u64 = 1 << 8;

/// The inline reference of simple quotas, which comes first.
pub(crate) const OWNER_REF: u8 = 172;

/// Who refers to an extent.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Backref {
    /// A tree block, from a block of tree `0` that is not
    /// [`FULL_BACKREF`] (`TREE_BLOCK_REF`).
    Tree(u64),
    /// A tree block, from the block at this address (`SHARED_BLOCK_REF`).
    Shared(u64),
    /// A data extent, from file `ino` of tree `root`, whose item's key
    /// less the extent's offset is `offset` (`EXTENT_DATA_REF`).
    Data { root: u64, ino: u64, offset: u64 },
    /// A data extent, from the leaf at this address (`SHARED_DATA_REF`).
    SharedData(u64),
}

/// Linux's `hash_extent_data_ref`: the key offset of a file's reference
/// that is an item of its own.
pub(crate) fn data_ref_hash(root: u64, ino: u64, offset: u64) -> u64 {
    let high = crc32c_update(!0, &root.to_le_bytes());
    let low = crc32c_update(crc32c_update(!0, &ino.to_le_bytes()), &offset.to_le_bytes());
    (u64::from(high) << 31) ^ u64::from(low)
}

impl Backref {
    /// Its type, as an inline reference's first byte and an item's key.
    fn kind(self) -> u8 {
        match self {
            Backref::Tree(_) => item::TREE_BLOCK_REF,
            Backref::Shared(_) => item::SHARED_BLOCK_REF,
            Backref::Data { .. } => item::EXTENT_DATA_REF,
            Backref::SharedData(_) => item::SHARED_DATA_REF,
        }
    }

    /// Whether it carries a count, as a data extent's references do.
    fn counted(self) -> bool {
        matches!(self, Backref::Data { .. } | Backref::SharedData(_))
    }

    /// Where it sorts among inline references of its type, highest
    /// first.
    fn order(self) -> u64 {
        match self {
            Backref::Tree(id) | Backref::Shared(id) | Backref::SharedData(id) => id,
            Backref::Data { root, ino, offset } => data_ref_hash(root, ino, offset),
        }
    }

    /// Its inline form after the type byte, without the count.
    fn body(self, out: &mut [u8]) {
        match self {
            Backref::Tree(id) | Backref::Shared(id) | Backref::SharedData(id) => {
                out[..8].copy_from_slice(&id.to_le_bytes());
            }
            Backref::Data { root, ino, offset } => {
                out[..8].copy_from_slice(&root.to_le_bytes());
                out[8..16].copy_from_slice(&ino.to_le_bytes());
                out[16..24].copy_from_slice(&offset.to_le_bytes());
            }
        }
    }

    /// The bytes of its inline form, with the type byte and any count.
    fn inline_size(self) -> usize {
        1 + body_size(self.kind()).unwrap_or(0)
    }

    /// Where its count is in its inline form.
    fn count_at(self) -> usize {
        match self {
            Backref::Data { .. } => 1 + 24,
            _ => 1 + 8,
        }
    }

    /// Whether the inline reference `bytes`, from its type byte on, is
    /// this one.
    fn is(self, bytes: &[u8]) -> bool {
        if bytes[0] != self.kind() {
            return false;
        }
        match self {
            Backref::Tree(id) | Backref::Shared(id) | Backref::SharedData(id) => {
                u64_at(bytes, 1) == id
            }
            Backref::Data { root, ino, offset } => {
                u64_at(bytes, 1) == root && u64_at(bytes, 9) == ino && u64_at(bytes, 17) == offset
            }
        }
    }

    /// The order of the inline reference `bytes`, of this one's type.
    fn order_of(self, bytes: &[u8]) -> u64 {
        match self {
            Backref::Data { .. } => {
                data_ref_hash(u64_at(bytes, 1), u64_at(bytes, 9), u64_at(bytes, 17))
            }
            _ => u64_at(bytes, 1),
        }
    }

    /// Whether it is the one reference of the extent item `data`, inline,
    /// with a count of one.
    pub(crate) fn only(self, data: &[u8]) -> Result<bool> {
        let (refs, _) = refs_flags(data)?;
        Ok(refs == 1
            && data.len() == EXTENT_HEADER + self.inline_size()
            && self.is(&data[EXTENT_HEADER..]))
    }

    /// The key of its item of its own, before any step past others with
    /// the same hash.
    fn key(self, bytenr: u64) -> Key {
        Key::new(bytenr, self.kind(), self.order())
    }

    /// Whether the data `data` of an item of its own is this one, and
    /// its count.
    fn item_count(self, data: &[u8]) -> Result<Option<u32>> {
        Ok(match self {
            Backref::Tree(_) | Backref::Shared(_) => Some(1),
            Backref::SharedData(_) => Some(u32_at(data.get(..4).ok_or(Error::Corrupt)?, 0)),
            Backref::Data { root, ino, offset } => {
                if data.len() < 28 {
                    return Err(Error::Corrupt);
                }
                (u64_at(data, 0) == root && u64_at(data, 8) == ino && u64_at(data, 16) == offset)
                    .then(|| u32_at(data, 24))
            }
        })
    }
}

/// The bytes after an inline reference's type byte, by its type.
fn body_size(kind: u8) -> Option<usize> {
    match kind {
        item::TREE_BLOCK_REF | item::SHARED_BLOCK_REF | OWNER_REF => Some(8),
        item::EXTENT_DATA_REF => Some(28),
        item::SHARED_DATA_REF => Some(12),
        _ => None,
    }
}

/// The inline references of the extent item `data`: each one's start.
fn inline_refs(data: &[u8]) -> Result<impl Iterator<Item = usize> + '_> {
    let mut at = EXTENT_HEADER;
    let mut bad = false;
    let refs = core::iter::from_fn(move || {
        if at >= data.len() || bad {
            return None;
        }
        let here = at;
        match body_size(data[at]) {
            Some(n) if at + 1 + n <= data.len() => {
                at += 1 + n;
                Some(here)
            }
            _ => {
                bad = true;
                Some(usize::MAX)
            }
        }
    });
    if data.len() < EXTENT_HEADER {
        return Err(Error::Corrupt);
    }
    Ok(refs)
}

/// The references and flags of an extent item's data.
pub(crate) fn refs_flags(data: &[u8]) -> Result<(u64, u64)> {
    if data.len() < EXTENT_HEADER {
        return Err(Error::Corrupt);
    }
    Ok((u64_at(data, 0), u64_at(data, 16)))
}

/// Every reference to the extent of item `key`, whose data is `data`, and
/// its count: those inline, then those that are items of their own. A
/// balance follows them to what refers to the extent (ADR 0159).
pub(crate) fn backrefs(volume: &Volume, key: &Key, data: &[u8]) -> Result<KVec<(Backref, u32)>> {
    let mut refs = mem::vec();
    for at in inline_refs(data)? {
        let b = data.get(at..).ok_or(Error::Corrupt)?;
        let r = match b[0] {
            item::TREE_BLOCK_REF => (Backref::Tree(u64_at(b, 1)), 1),
            item::SHARED_BLOCK_REF => (Backref::Shared(u64_at(b, 1)), 1),
            item::EXTENT_DATA_REF => {
                let r =
                    Backref::Data { root: u64_at(b, 1), ino: u64_at(b, 9), offset: u64_at(b, 17) };
                (r, u32_at(b, 25))
            }
            item::SHARED_DATA_REF => (Backref::SharedData(u64_at(b, 1)), u32_at(b, 9)),
            _ => continue,
        };
        mem::push(&mut refs, r)?;
    }
    let root = Writer::tree_root(volume, tree::EXTENT)?;
    let mut cursor = Cursor::seek(volume, &root, &Key::new(key.objectid, item::TREE_BLOCK_REF, 0))?;
    while let Some((found, data)) = cursor.get() {
        if found.objectid != key.objectid {
            break;
        }
        let r = match found.kind {
            item::TREE_BLOCK_REF => (Backref::Tree(found.offset), 1),
            item::SHARED_BLOCK_REF => (Backref::Shared(found.offset), 1),
            item::EXTENT_DATA_REF if data.len() >= 28 => {
                let r = Backref::Data {
                    root: u64_at(data, 0),
                    ino: u64_at(data, 8),
                    offset: u64_at(data, 16),
                };
                (r, u32_at(data, 24))
            }
            item::SHARED_DATA_REF if data.len() >= 4 => {
                (Backref::SharedData(found.offset), u32_at(data, 0))
            }
            item::EXTENT_DATA_REF | item::SHARED_DATA_REF => return Err(Error::Corrupt),
            _ => {
                cursor.advance()?;
                continue;
            }
        };
        mem::push(&mut refs, r)?;
        cursor.advance()?;
    }
    Ok(refs)
}

/// Where a reference is: inline at this byte of its extent's item, or an
/// item of its own with this key, and its count.
enum Found {
    Inline(usize, u32),
    Keyed(Key, u32),
}

impl Writer {
    /// The key of the extent item of the tree block at `bytenr`, a block
    /// at `level`.
    pub(crate) fn block_key(bytenr: u64, level: u8) -> Key {
        Key::new(bytenr, item::METADATA_ITEM, level.into())
    }

    /// The extent item `key`, copied, as the transaction has it.
    pub(crate) fn extent_item(
        volume: &Volume,
        key: &Key,
    ) -> Result<Option<radix_vfs::mem::KVec<u8>>> {
        crate::tree::find(volume, &Writer::tree_root(volume, tree::EXTENT)?, key)
    }

    /// The most bytes an extent item keeps its references in: a
    /// sixteenth of a leaf, less an item header, as Linux has it.
    fn max_extent_item(volume: &Volume) -> usize {
        ((volume.superblock().nodesize as usize - HEADER_SIZE) >> 4) - ITEM_SIZE
    }

    /// Where the reference `r` to the extent of item `key`, whose data is
    /// `data`, is, if it has one.
    fn find_ref(volume: &Volume, key: &Key, data: &[u8], r: Backref) -> Result<Option<Found>> {
        for at in inline_refs(data)? {
            let b = data.get(at..).ok_or(Error::Corrupt)?;
            if r.is(b) {
                let count = if r.counted() { u32_at(b, r.count_at()) } else { 1 };
                return Ok(Some(Found::Inline(at, count)));
            }
        }
        let root = Writer::tree_root(volume, tree::EXTENT)?;
        let mut want = r.key(key.objectid);
        loop {
            let cursor = Cursor::seek(volume, &root, &want)?;
            let Some((found, item)) = cursor.get() else { return Ok(None) };
            if found.objectid != key.objectid || found.kind != r.kind() {
                return Ok(None);
            }
            if !matches!(r, Backref::Data { .. }) {
                if found.offset != want.offset {
                    return Ok(None);
                }
                let count = r.item_count(item)?.ok_or(Error::Corrupt)?;
                return Ok(Some(Found::Keyed(found, count)));
            }
            // Hashes alike go to the next key free, as Linux places them.
            if let Some(count) = r.item_count(item)? {
                return Ok(Some(Found::Keyed(found, count)));
            }
            if found.offset == u64::MAX {
                return Ok(None);
            }
            want = Key::new(key.objectid, r.kind(), found.offset + 1);
        }
    }

    /// Adds `count` references `r` to the extent of item `key` (one, for a
    /// tree block's).
    pub(crate) fn add_ref(
        &mut self,
        volume: &Volume,
        key: &Key,
        r: Backref,
        count: u32,
    ) -> Result<()> {
        let data = Writer::extent_item(volume, key)?.ok_or(Error::Corrupt)?;
        let found = Writer::find_ref(volume, key, &data, r)?;
        if found.is_some() && !r.counted() {
            // A tree refers to a block once, and so does a block above.
            return Err(Error::Corrupt);
        }
        let add = u64::from(count);
        match found {
            Some(Found::Inline(at, old)) => {
                let new = old.checked_add(count).ok_or(Error::Corrupt)?;
                let count_at = at + r.count_at();
                self.update(volume, tree::EXTENT, key, |d| {
                    put_u64(d, 0, u64_at(d, 0) + add);
                    put_u32(d, count_at, new);
                })?;
            }
            Some(Found::Keyed(rkey, old)) => {
                let new = old.checked_add(count).ok_or(Error::Corrupt)?;
                let count_at = if let Backref::Data { .. } = r { 24 } else { 0 };
                self.update(volume, tree::EXTENT, &rkey, |d| {
                    put_u32(d, count_at, new);
                })?;
                self.update(volume, tree::EXTENT, key, |d| {
                    put_u64(d, 0, u64_at(d, 0) + add);
                })?;
            }
            None if data.len() + r.inline_size() < Writer::max_extent_item(volume) => {
                // Before the first of a later type, or of its type and
                // lower.
                let mut place = data.len();
                for at in inline_refs(&data)? {
                    let b = data.get(at..).ok_or(Error::Corrupt)?;
                    if b[0] > r.kind() || (b[0] == r.kind() && r.order_of(b) < r.order()) {
                        place = at;
                        break;
                    }
                }
                let size = r.inline_size();
                self.resize(volume, tree::EXTENT, key, data.len() + size)?;
                let end = data.len();
                self.update(volume, tree::EXTENT, key, |d| {
                    d.copy_within(place..end, place + size);
                    put_u64(d, 0, u64_at(d, 0) + add);
                    d[place] = r.kind();
                    r.body(&mut d[place + 1..]);
                    if r.counted() {
                        put_u32(d, place + r.count_at(), count);
                    }
                })?;
            }
            None => {
                let mut rkey = r.key(key.objectid);
                if let Backref::Data { root, ino, offset } = r {
                    // The first key free from the hash on.
                    let extent = Writer::tree_root(volume, tree::EXTENT)?;
                    while crate::tree::find(volume, &extent, &rkey)?.is_some() {
                        rkey.offset = rkey.offset.checked_add(1).ok_or(Error::Corrupt)?;
                    }
                    self.insert_with(volume, tree::EXTENT, &rkey, 28, |d| {
                        put_u64(d, 0, root);
                        put_u64(d, 8, ino);
                        put_u64(d, 16, offset);
                        put_u32(d, 24, count);
                    })?;
                } else if r.counted() {
                    self.insert(volume, tree::EXTENT, &rkey, &count.to_le_bytes())?;
                } else {
                    self.insert(volume, tree::EXTENT, &rkey, &[])?;
                }
                self.update(volume, tree::EXTENT, key, |d| {
                    put_u64(d, 0, u64_at(d, 0) + add);
                })?;
            }
        }
        Ok(())
    }

    /// Takes one reference `r` from the extent of item `key`, and returns
    /// how many it has left. An extent with none left keeps its item,
    /// for its caller to free.
    pub(crate) fn drop_ref(&mut self, volume: &Volume, key: &Key, r: Backref) -> Result<u64> {
        let data = Writer::extent_item(volume, key)?.ok_or(Error::Corrupt)?;
        let refs = u64_at(&data, 0).checked_sub(1).ok_or(Error::Corrupt)?;
        match Writer::find_ref(volume, key, &data, r)?.ok_or(Error::Corrupt)? {
            Found::Inline(at, count) if count > 1 => {
                let count_at = at + r.count_at();
                self.update(volume, tree::EXTENT, key, |d| {
                    put_u64(d, 0, refs);
                    put_u32(d, count_at, count - 1);
                })?;
            }
            Found::Inline(at, _) => {
                let size = r.inline_size();
                self.update(volume, tree::EXTENT, key, |d| {
                    put_u64(d, 0, refs);
                    d.copy_within(at + size.., at);
                })?;
                self.resize(volume, tree::EXTENT, key, data.len() - size)?;
            }
            Found::Keyed(rkey, count) => {
                if count > 1 {
                    let count_at = if let Backref::Data { .. } = r { 24 } else { 0 };
                    self.update(volume, tree::EXTENT, &rkey, |d| {
                        put_u32(d, count_at, count - 1);
                    })?;
                } else {
                    self.delete(volume, tree::EXTENT, &rkey)?;
                }
                self.update(volume, tree::EXTENT, key, |d| {
                    put_u64(d, 0, refs);
                })?;
            }
        }
        Ok(refs)
    }

    /// Sets `flags` in the extent item `key`.
    pub(crate) fn set_extent_flags(
        &mut self,
        volume: &Volume,
        key: &Key,
        flags: u64,
    ) -> Result<()> {
        self.update(volume, tree::EXTENT, key, |d| {
            put_u64(d, 16, u64_at(d, 16) | flags);
        })
    }
}
