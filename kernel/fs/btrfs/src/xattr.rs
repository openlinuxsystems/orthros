// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Extended attributes (ADR 0256), as Linux keeps them: an `XATTR_ITEM`
//! of the inode, keyed by the CRC-32C hash of the name as a directory's
//! entries are ([`crate::format::name_hash`]), holding each attribute
//! whose name has that hash in the form of a directory entry: an empty
//! location, the transaction that made it, the value's and the name's
//! lengths, the type `BTRFS_FT_XATTR`, the name and the value. Access
//! lists are kept as their attributes' values, in Linux's
//! `posix_acl_xattr` form. An attribute and the item it is in fit in a
//! tree block, as on Linux, which leaves a 16 KiB block about 16,000
//! bytes for a name and a value.

use radix_vfs::mem::{self, KVec};
use radix_vfs::{Error, Result, Setting};

use crate::files::{Files, at, dir_entry, put_time, put_u16, put_u64};
use crate::format::{Key, item, name_hash, u16_at};
use crate::tree::{Block, Cursor, Root};
use crate::volume::Volume;

/// The size of an attribute before its name: a directory entry's.
const HEADER: usize = 30;

/// The type byte of an attribute: Linux's `BTRFS_FT_XATTR`.
const FT_XATTR: u8 = 8;

/// The key of the item that holds `ino`'s attribute `name`.
fn key_of(ino: u64, name: &[u8]) -> Key {
    Key::new(ino, item::XATTR_ITEM, name_hash(name))
}

/// The value of attribute `name` in the item `data`, if it holds it.
fn value_in<'a>(data: &'a [u8], name: &[u8]) -> Result<Option<&'a [u8]>> {
    Ok(dir_entry(data, name)?.map(|(start, len)| &data[start + HEADER + name.len()..start + len]))
}

/// The value of inode `ino`'s attribute `name` in the tree `root`;
/// the caller holds the transaction on a writable mount.
pub(crate) fn get(volume: &Volume, root: &Root, ino: u64, name: &[u8]) -> Result<KVec<u8>> {
    let key = key_of(ino, name);
    let cursor = Cursor::seek(volume, root, &key)?;
    match cursor.get() {
        Some((found, data)) if found == key => match value_in(data, name)? {
            Some(value) => Ok(mem::copy(value)?),
            None => Err(Error::NoAttribute),
        },
        _ => Err(Error::NoAttribute),
    }
}

/// The names of inode `ino`'s attributes in the tree `root`, each with a
/// NUL after it, in the order of their hashes, as Linux lists them; the
/// caller holds the transaction on a writable mount.
pub(crate) fn list(volume: &Volume, root: &Root, ino: u64) -> Result<KVec<u8>> {
    let mut names = mem::vec();
    let mut at = Cursor::seek(volume, root, &Key::first(ino, item::XATTR_ITEM))?;
    while let Some((key, data)) = at.get() {
        if !key.is(ino, item::XATTR_ITEM) {
            break;
        }
        let mut rest = data;
        while !rest.is_empty() {
            if rest.len() < HEADER {
                return Err(Error::Corrupt);
            }
            let len = HEADER + usize::from(u16_at(rest, 27)) + usize::from(u16_at(rest, 25));
            let name_len = usize::from(u16_at(rest, 27));
            if rest.len() < len || name_len == 0 {
                return Err(Error::Corrupt);
            }
            mem::extend(&mut names, &rest[HEADER..HEADER + name_len])?;
            mem::extend(&mut names, &[0])?;
            rest = &rest[len..];
        }
        at.advance()?;
    }
    Ok(names)
}

/// Where an attribute's change finds it: the item that holds attributes
/// of its name's hash, if there is one, and where in it the attribute
/// is, if it is there.
pub(crate) struct Found {
    held: Option<KVec<u8>>,
    at: Option<(usize, usize)>,
}

impl Files<'_> {
    /// Finds inode `ino`'s attribute `name`, for a change.
    fn find_xattr(&self, ino: u64, name: &[u8]) -> Result<Found> {
        let held = self.get(self.subvol, &key_of(ino, name))?;
        let at = match &held {
            Some(data) => dir_entry(data, name)?,
            None => None,
        };
        Ok(Found { held, at })
    }

    /// Checks that inode `ino`'s attribute `name` may be set to `value`
    /// as `how` says, before any change: [`Error::Exists`] and
    /// [`Error::NoAttribute`] as `how` has them, and [`Error::NoSpace`] if
    /// the item with it would not fit in a tree block, as Linux's ENOSPC.
    pub(crate) fn check_set_xattr(
        &self,
        ino: u64,
        name: &[u8],
        value: &[u8],
        how: Setting,
    ) -> Result<Found> {
        let found = self.find_xattr(ino, name)?;
        match (found.at, how) {
            (Some(_), Setting::Create) => return Err(Error::Exists),
            (None, Setting::Replace) => return Err(Error::NoAttribute),
            _ => {}
        }
        let others =
            found.held.as_deref().map_or(0, <[u8]>::len) - found.at.map_or(0, |(_, len)| len);
        let max = Block::max_item(self.volume.superblock().nodesize);
        if others + HEADER + name.len() + value.len() > max {
            return Err(Error::NoSpace);
        }
        Ok(found)
    }

    /// Sets inode `ino`'s attribute `name` to `value`, where
    /// [`Files::check_set_xattr`] found it, and its change time to `now`.
    pub(crate) fn set_xattr(
        &mut self,
        ino: u64,
        name: &[u8],
        value: &[u8],
        found: &Found,
        now: u64,
    ) -> Result<()> {
        let key = key_of(ino, name);
        let mut entry = crate::zeroed(HEADER + name.len() + value.len())?;
        put_u64(&mut entry, 17, self.w.generation);
        put_u16(&mut entry, 25, value.len() as u16);
        put_u16(&mut entry, 27, name.len() as u16);
        entry[29] = FT_XATTR;
        entry[HEADER..HEADER + name.len()].copy_from_slice(name);
        entry[HEADER + name.len()..].copy_from_slice(value);
        if let (Some(data), Some((start, len))) = (&found.held, found.at) {
            self.cut(self.subvol, &key, data.len(), start, len)?;
        }
        self.append(self.subvol, &key, &entry)?;
        self.change_inode(ino, |d| put_time(d, at::CTIME, now))
    }

    /// Checks that inode `ino` has the attribute `name`, before it is
    /// removed: [`Error::NoAttribute`] if not.
    pub(crate) fn check_remove_xattr(&self, ino: u64, name: &[u8]) -> Result<Found> {
        let found = self.find_xattr(ino, name)?;
        if found.at.is_none() {
            return Err(Error::NoAttribute);
        }
        Ok(found)
    }

    /// Removes inode `ino`'s attribute `name`, where
    /// [`Files::check_remove_xattr`] found it, and sets its change time to
    /// `now`.
    pub(crate) fn remove_xattr(
        &mut self,
        ino: u64,
        name: &[u8],
        found: &Found,
        now: u64,
    ) -> Result<()> {
        let (Some(data), Some((start, len))) = (&found.held, found.at) else {
            return Err(Error::NoAttribute);
        };
        self.cut(self.subvol, &key_of(ino, name), data.len(), start, len)?;
        self.change_inode(ino, |d| put_time(d, at::CTIME, now))
    }
}
