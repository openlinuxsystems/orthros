// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Replaying the log tree Linux leaves when a machine stops after an
//! fsync and before the next commit (ADR 0151), as Linux's mount does:
//! each inode the log holds gets the log's inode item, names, extents
//! with their checksums, and extended attributes; the names a logged
//! directory lost go; then the link counts, directory sizes and bytes
//! of what changed are counted again. It is one transaction, which the
//! mount commits before it goes on, with no log.
//!
//! The log's own blocks and the data it names are in no commit, so the
//! free-space tree has them free: the log's blocks are kept from use
//! until the next mount, and its data is taken before anything else is.

use radix_vfs::mem::{self, KVec};
use radix_vfs::{Error, Kind, Result};

use crate::dir::Entries;
use crate::files::{FileExtent, Files, at, extent_type, file_type};
use crate::format::{Key, item, name_hash, tree, u16_at, u64_at};
use crate::fs::Txn;
use crate::inode::Inode;
use crate::tree::{Bounds, Cursor, Root, read_block};
use crate::volume::Volume;

/// The objectid of the log root tree's root items, and the owner of every
/// log tree's blocks (`BTRFS_TREE_LOG_OBJECTID`).
const TREE_LOG: u64 = (-6i64) as u64;

/// A logged directory's range of indexes the log holds whole: from the
/// key's offset to the item's end (`BTRFS_DIR_LOG_INDEX_KEY`).
const DIR_LOG_INDEX: u8 = 72;

/// An item of a log tree, copied.
struct Logged {
    key: Key,
    data: KVec<u8>,
}

/// One subvolume's log: its tree's number, and its items in order.
struct Log {
    subvol: u64,
    items: KVec<Logged>,
}

/// Replays the log the superblock names into the transaction `txn` of
/// `volume`, the first change of a writable mount.
#[inline(never)]
pub(crate) fn replay(volume: &Volume, txn: &mut Txn) -> Result<()> {
    let sb = volume.superblock();
    let root =
        Root { bytenr: sb.log_root, level: sb.log_root_level, generation: sb.generation + 1 };
    let logs = read_logs(volume, txn, &root)?;
    for log in &logs {
        take_data(volume, txn, log)?;
    }
    let now = radix_vfs::now();
    for log in &logs {
        // A log of a subvolume that is gone since has nothing to replay.
        if volume.tree(log.subvol)?.is_none() {
            continue;
        }
        let mut files = txn.files(volume, log.subvol);
        files.whole(|f| Replay { f, log, now, touched: mem::vec() }.run())?;
    }
    Ok(())
}

/// Reads every log tree the log root tree `root` names, keeping their
/// blocks from use.
#[inline(never)]
fn read_logs(volume: &Volume, txn: &mut Txn, root: &Root) -> Result<KVec<Log>> {
    let mut roots = mem::vec();
    keep_blocks(volume, txn, root)?;
    let mut cursor = Cursor::seek(volume, root, &Key::new(0, 0, 0))?;
    while let Some((key, data)) = cursor.get() {
        if key.is(TREE_LOG, item::ROOT_ITEM) {
            let item = crate::format::RootItem::read(data).ok_or(Error::Corrupt)?;
            let log = Root { bytenr: item.bytenr, level: item.level, generation: item.generation };
            mem::push(&mut roots, (key.offset, log))?;
        }
        cursor.advance()?;
    }
    let mut logs = mem::vec();
    for (subvol, log) in roots {
        keep_blocks(volume, txn, &log)?;
        let mut items = mem::vec();
        let mut cursor = Cursor::seek(volume, &log, &Key::new(0, 0, 0))?;
        while let Some((key, data)) = cursor.get() {
            mem::push(&mut items, Logged { key, data: mem::copy(data)? })?;
            cursor.advance()?;
        }
        mem::push(&mut logs, Log { subvol, items })?;
    }
    Ok(logs)
}

/// Keeps every block of the tree `root` from use while this mount lasts:
/// a crash before the replay's commit leaves the log to replay again.
fn keep_blocks(volume: &Volume, txn: &mut Txn, root: &Root) -> Result<()> {
    let nodesize = u64::from(volume.superblock().nodesize);
    let mut pending = mem::vec();
    mem::push(&mut pending, *root)?;
    while let Some(want) = pending.pop() {
        let block = read_block(volume, &want, &Bounds::default())?;
        txn.writer.keep_from_use(want.bytenr, nodesize)?;
        if !block.is_leaf() {
            for slot in 0..block.nritems() {
                mem::push(&mut pending, block.child(slot))?;
            }
        }
    }
    Ok(())
}

/// Takes the data extents `log` names that no commit has: Linux wrote
/// them before the fsync, but only the log names them.
#[inline(never)]
fn take_data(volume: &Volume, txn: &mut Txn, log: &Log) -> Result<()> {
    // Two items may name one extent: it is taken once.
    let mut taken: KVec<u64> = mem::vec();
    for logged in &log.items {
        if logged.key.kind != item::EXTENT_DATA {
            continue;
        }
        let extent = FileExtent::read(&logged.data)?;
        if extent.kind == extent_type::INLINE || extent.disk_bytenr == 0 {
            continue;
        }
        let key = Key::new(extent.disk_bytenr, item::EXTENT_ITEM, extent.disk_num_bytes);
        if !taken.contains(&extent.disk_bytenr)
            && crate::cow::Writer::extent_item(volume, &key)?.is_none()
        {
            txn.writer.take_logged(extent.disk_bytenr, extent.disk_num_bytes)?;
            mem::push(&mut taken, extent.disk_bytenr)?;
        }
    }
    Ok(())
}

/// The replay of one subvolume's log.
struct Replay<'f, 'a> {
    f: &'f mut Files<'a>,
    log: &'f Log,
    now: u64,
    /// The inodes whose names, sizes or bytes changed, to count again.
    touched: KVec<u64>,
}

impl Replay<'_, '_> {
    fn run(mut self) -> Result<()> {
        // The inodes first, then the names of the logged directories, then
        // the rest: names, extents, checksums and extended attributes.
        for logged in &self.log.items {
            if logged.key.kind == item::INODE_ITEM {
                self.inode(logged)?;
            }
        }
        for logged in &self.log.items {
            if logged.key.kind == item::DIR_INDEX {
                self.dir_index(logged)?;
            }
        }
        for logged in &self.log.items {
            match logged.key.kind {
                item::INODE_REF => self.inode_ref(logged)?,
                item::INODE_EXTREF => self.inode_extref(logged)?,
                item::EXTENT_DATA => self.extent(logged)?,
                item::XATTR_ITEM => self.xattr(logged)?,
                item::EXTENT_CSUM if logged.key.objectid == tree::EXTENT_CSUM => {
                    self.sums(logged)?;
                }
                _ => {}
            }
        }
        let touched = core::mem::replace(&mut self.touched, mem::vec());
        for ino in touched {
            self.count_again(ino)?;
        }
        Ok(())
    }

    fn subvol(&self) -> u64 {
        self.f.subvol
    }

    fn touch(&mut self, ino: u64) -> Result<()> {
        if !self.touched.contains(&ino) {
            mem::push(&mut self.touched, ino)?;
        }
        Ok(())
    }

    /// The inode item of `ino` in the subvolume.
    fn inode_of(&self, ino: u64) -> Result<Option<Inode>> {
        let key = Key::new(ino, item::INODE_ITEM, 0);
        self.f.get(self.subvol(), &key)?.map(|d| Inode::read(&d)).transpose()
    }

    /// Whether the log holds the item `key`.
    fn logged(&self, key: &Key) -> Option<&Logged> {
        self.log.items.binary_search_by(|l| l.key.cmp(key)).ok().map(|at| &self.log.items[at])
    }

    /// An inode item: a directory first loses the names in the ranges the
    /// log holds whole that the log does not have, a file the extents past
    /// its logged size; then the item is the log's.
    fn inode(&mut self, logged: &Logged) -> Result<()> {
        let ino = logged.key.objectid;
        let inode = Inode::read(&logged.data)?;
        let there = self.inode_of(ino)?.is_some();
        if inode.kind()? == Kind::Directory {
            self.dir_deletes(ino)?;
        } else if there && inode.kind()? == Kind::File {
            let sector = u64::from(self.f.volume.superblock().sectorsize);
            let from = inode.size.next_multiple_of(sector);
            self.f.drop_extents(ino, from, u64::MAX)?;
        }
        self.xattr_deletes(ino)?;
        self.f.put_inode(ino, &logged.data)?;
        self.touch(ino)
    }

    /// Takes the names of directory `dir` that the log's ranges hold whole
    /// but its items do not.
    fn dir_deletes(&mut self, dir: u64) -> Result<()> {
        let log = self.log;
        let ranges = log.items.iter().filter(|l| l.key.is(dir, DIR_LOG_INDEX));
        for range in ranges {
            let (first, last) = (range.key.offset, u64_at(&range.data, 0));
            let mut at = Key::new(dir, item::DIR_INDEX, first);
            while let Some((key, data)) = self.f.next(self.subvol(), &at)?
                && key.is(dir, item::DIR_INDEX)
                && key.offset <= last
            {
                let (name, ino, kind) = one_entry(&data)?;
                let kept = self.logged(&key).is_some_and(|l| {
                    one_entry(&l.data).is_ok_and(|(n, i, _)| n == name && i == ino)
                });
                if !kept && kind == item::INODE_ITEM {
                    self.unlink(dir, &name, ino, Some(key.offset))?;
                    self.touch(ino)?;
                }
                if key.offset == u64::MAX {
                    break;
                }
                at = Key::new(dir, item::DIR_INDEX, key.offset + 1);
            }
        }
        self.touch(dir)
    }

    /// Takes the extended attributes of `ino` that the log does not hold.
    fn xattr_deletes(&mut self, ino: u64) -> Result<()> {
        let mut at = Key::first(ino, item::XATTR_ITEM);
        while let Some((key, _)) = self.f.next(self.subvol(), &at)?
            && key.is(ino, item::XATTR_ITEM)
        {
            if self.logged(&key).is_none() {
                self.f.w.delete(self.f.volume, self.subvol(), &key)?;
            }
            if key.offset == u64::MAX {
                break;
            }
            at = Key::new(ino, item::XATTR_ITEM, key.offset + 1);
        }
        Ok(())
    }

    /// A directory's entry at its index: the name goes there, whatever it
    /// named before, if what it names is in the subvolume.
    fn dir_index(&mut self, logged: &Logged) -> Result<()> {
        let dir = logged.key.objectid;
        let (name, ino, kind) = one_entry(&logged.data)?;
        if kind != item::INODE_ITEM || self.inode_of(dir)?.is_none() {
            return Ok(());
        }
        let Some(inode) = self.inode_of(ino)? else { return Ok(()) };
        self.link(dir, &name, ino, file_type(inode.kind()?), logged.key.offset)
    }

    /// Names `ino` `name` in `dir` at `index`, after taking what `dir`
    /// named there or so before; nothing if it is so already.
    fn link(&mut self, dir: u64, name: &[u8], ino: u64, ft: u8, index: u64) -> Result<()> {
        let at = Key::new(dir, item::DIR_INDEX, index);
        if let Some(data) = self.f.get(self.subvol(), &at)? {
            let (old, other, _) = one_entry(&data)?;
            if old == name && other == ino {
                return Ok(());
            }
            self.unlink(dir, &old, other, Some(index))?;
            self.touch(other)?;
        }
        if let Some(other) = self.named(dir, name)? {
            self.unlink(dir, name, other, None)?;
            self.touch(other)?;
        }
        if self.f.index_of(ino, dir, name)?.is_some() {
            self.unlink(dir, name, ino, None)?;
        }
        self.f.add_entry(dir, name, ino, ft, index, self.now)?;
        self.touch(dir)?;
        self.touch(ino)
    }

    /// What `dir` names `name`, by its entry.
    fn named(&self, dir: u64, name: &[u8]) -> Result<Option<u64>> {
        let key = Key::new(dir, item::DIR_ITEM, name_hash(name));
        let Some(data) = self.f.get(self.subvol(), &key)? else { return Ok(None) };
        for entry in Entries::of(&data) {
            let entry = entry?;
            if entry.name == name {
                return Ok(Some(entry.location.objectid));
            }
        }
        Ok(None)
    }

    /// Takes the name `name` of `ino` from `dir`: whichever of the entry
    /// by name, the entry at its index (`index`, or the reference's) and
    /// the reference there are.
    fn unlink(&mut self, dir: u64, name: &[u8], ino: u64, index: Option<u64>) -> Result<()> {
        let subvol = self.subvol();
        let key = Key::new(dir, item::DIR_ITEM, name_hash(name));
        if let Some(data) = self.f.get(subvol, &key)?
            && let Some((start, len)) = crate::files::dir_entry(&data, name)?
        {
            self.f.cut(subvol, &key, data.len(), start, len)?;
        }
        let index = match self.f.cut_reference(ino, dir, name)? {
            Some(found) => Some(index.unwrap_or(found)),
            None => index,
        };
        if let Some(index) = index {
            let at = Key::new(dir, item::DIR_INDEX, index);
            if let Some(data) = self.f.get(subvol, &at)?
                && one_entry(&data)?.0 == name
            {
                self.f.w.delete(self.f.volume, subvol, &at)?;
            }
        }
        self.touch(dir)
    }

    /// An inode's reference item: each name it holds is the inode's in
    /// that directory, and the inode's other names there go.
    fn inode_ref(&mut self, logged: &Logged) -> Result<()> {
        let (ino, dir) = (logged.key.objectid, logged.key.offset);
        // A subvolume's top directory refers to itself, as `..`.
        if dir == ino {
            return Ok(());
        }
        let Some(inode) = self.inode_of(ino)? else { return Ok(()) };
        if self.inode_of(dir)?.is_none() {
            return Ok(());
        }
        let ft = file_type(inode.kind()?);
        let names = ref_names(&logged.data, false)?;
        // The names the subvolume has that the log does not.
        let key = Key::new(ino, item::INODE_REF, dir);
        if let Some(data) = self.f.get(self.subvol(), &key)? {
            for (_, index, name) in ref_names(&data, false)? {
                if !names.iter().any(|(_, _, n)| *n == name) {
                    self.unlink(dir, &name, ino, Some(index))?;
                }
            }
        }
        for (_, index, name) in names {
            self.link(dir, &name, ino, ft, index)?;
        }
        Ok(())
    }

    /// An inode's extended reference item: each name it holds is the
    /// inode's in its directory.
    fn inode_extref(&mut self, logged: &Logged) -> Result<()> {
        let ino = logged.key.objectid;
        let Some(inode) = self.inode_of(ino)? else { return Ok(()) };
        let ft = file_type(inode.kind()?);
        for (dir, index, name) in ref_names(&logged.data, true)? {
            if self.inode_of(dir)?.is_some() {
                self.link(dir, &name, ino, ft, index)?;
            }
        }
        Ok(())
    }

    /// A file extent item: the file's bytes in its range are the item's,
    /// with a reference to its data.
    fn extent(&mut self, logged: &Logged) -> Result<()> {
        let (ino, pos) = (logged.key.objectid, logged.key.offset);
        if self.inode_of(ino)?.is_none() {
            return Ok(());
        }
        let subvol = self.subvol();
        if self.f.get(subvol, &logged.key)?.is_some_and(|d| d == logged.data) {
            return Ok(());
        }
        let extent = FileExtent::read(&logged.data)?;
        let sector = u64::from(self.f.volume.superblock().sectorsize);
        let end = pos.checked_add(extent.len).ok_or(Error::Corrupt)?.next_multiple_of(sector);
        let data = (extent.kind != extent_type::INLINE && extent.disk_bytenr != 0)
            .then(|| Key::new(extent.disk_bytenr, item::EXTENT_ITEM, extent.disk_num_bytes));
        // The reference first, so that dropping what the file named there
        // never frees the extent the log names again.
        let held = match &data {
            Some(key) if self.f.get(tree::EXTENT, key)?.is_some() => {
                let offset = pos.wrapping_sub(extent.offset);
                self.f.add_data_ref(key.objectid, key.offset, ino, offset)?;
                true
            }
            _ => false,
        };
        self.f.drop_extents(ino, pos, end)?;
        self.f.w.insert(self.f.volume, subvol, &logged.key, &logged.data)?;
        if let Some(key) = data
            && !held
        {
            self.f.new_data_extent(
                key.objectid,
                key.offset,
                ino,
                pos.wrapping_sub(extent.offset),
            )?;
        }
        self.touch(ino)
    }

    /// A checksum item: the sectors it covers get its checksums.
    fn sums(&mut self, logged: &Logged) -> Result<()> {
        let sb = self.f.volume.superblock();
        let size = sb.csum_type.size();
        if size == 0 || !logged.data.len().is_multiple_of(size) {
            return Err(Error::Corrupt);
        }
        let len = (logged.data.len() / size) as u64 * u64::from(sb.sectorsize);
        self.f.delete_sums(logged.key.offset, len)?;
        self.f.add_sums(logged.key.offset, &logged.data)
    }

    /// An extended attribute item: the log's, in place of the subvolume's.
    fn xattr(&mut self, logged: &Logged) -> Result<()> {
        if self.inode_of(logged.key.objectid)?.is_none() {
            return Ok(());
        }
        let (volume, subvol) = (self.f.volume, self.subvol());
        match self.f.w.delete(volume, subvol, &logged.key) {
            Ok(()) | Err(Error::NotFound) => {}
            Err(e) => return Err(e),
        }
        self.f.w.insert(volume, subvol, &logged.key, &logged.data)
    }

    /// Counts `ino`'s names, and a directory's size or a file's bytes,
    /// again; an inode left with no name goes, as an orphan would.
    fn count_again(&mut self, ino: u64) -> Result<()> {
        let Some(inode) = self.inode_of(ino)? else { return Ok(()) };
        let subvol = self.subvol();
        let mut names = 0u32;
        let mut at = Key::first(ino, item::INODE_REF);
        while let Some((key, data)) = self.f.next(subvol, &at)?
            && key.objectid == ino
            && (key.kind == item::INODE_REF || key.kind == item::INODE_EXTREF)
        {
            let extended = key.kind == item::INODE_EXTREF;
            names += ref_names(&data, extended)?.len() as u32;
            if key.offset == u64::MAX {
                break;
            }
            at = Key::new(ino, key.kind, key.offset + 1);
        }
        let top = ino == tree::FIRST_FREE;
        if inode.kind()? == Kind::Directory {
            let mut size = 0;
            let mut at = Key::first(ino, item::DIR_INDEX);
            while let Some((key, data)) = self.f.next(subvol, &at)?
                && key.is(ino, item::DIR_INDEX)
            {
                size += 2 * one_entry(&data)?.0.len() as u64;
                if key.offset == u64::MAX {
                    break;
                }
                at = Key::new(ino, item::DIR_INDEX, key.offset + 1);
            }
            if names == 0 && !top && size == 0 {
                return self.f.free_inode(ino, false);
            }
            return self.f.change_inode(ino, |d| {
                crate::files::put_u64(d, at::SIZE, size);
                crate::files::put_u32(d, at::NLINK, 1);
            });
        }
        if names == 0 {
            return self.f.free_inode(ino, false);
        }
        let mut nbytes = 0;
        let mut at = Key::first(ino, item::EXTENT_DATA);
        while let Some((key, data)) = self.f.next(subvol, &at)?
            && key.is(ino, item::EXTENT_DATA)
        {
            let extent = FileExtent::read(&data)?;
            if extent.kind == extent_type::INLINE || extent.disk_bytenr != 0 {
                nbytes += extent.len;
            }
            if key.offset == u64::MAX {
                break;
            }
            at = Key::new(ino, item::EXTENT_DATA, key.offset + 1);
        }
        self.f.change_inode(ino, |d| {
            crate::files::put_u32(d, at::NLINK, names);
            crate::files::put_u64(d, at::NBYTES, nbytes);
        })
    }
}

/// The one entry of a `DIR_INDEX` item: its name, what it names, and
/// that's key type (an inode's, or a subvolume's root item).
fn one_entry(data: &[u8]) -> Result<(KVec<u8>, u64, u8)> {
    let Some(entry) = Entries::of(data).next() else { return Err(Error::Corrupt) };
    let entry = entry?;
    Ok((mem::copy(entry.name)?, entry.location.objectid, entry.location.kind))
}

/// The names a reference item `data` holds: for each, its directory (0
/// in a plain reference, whose key says it), its index and its name.
fn ref_names(data: &[u8], extended: bool) -> Result<KVec<(u64, u64, KVec<u8>)>> {
    let mut out = mem::vec();
    let mut at = 0;
    let header = if extended { 18 } else { 10 };
    while at < data.len() {
        let head = data.get(at..at + header).ok_or(Error::Corrupt)?;
        let (dir, index, len) = if extended {
            (u64_at(head, 0), u64_at(head, 8), usize::from(u16_at(head, 16)))
        } else {
            (0, u64_at(head, 0), usize::from(u16_at(head, 8)))
        };
        let name = data.get(at + header..at + header + len).ok_or(Error::Corrupt)?;
        mem::push(&mut out, (dir, index, mem::copy(name)?))?;
        at += header + len;
    }
    Ok(out)
}
