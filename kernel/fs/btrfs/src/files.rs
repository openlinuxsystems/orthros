// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Changing a subvolume's files, in the running transaction (ADR 0130):
//! inode items, directory entries, file extents, the extent items and
//! checksums of their data, and orphans.
//!
//! Each change here is several items in several trees, which only
//! together say what Linux's `btrfs check` expects. A name in a directory
//! is a `DIR_ITEM`, a `DIR_INDEX` and the inode's `INODE_REF` (or, once
//! that item is full, an `INODE_EXTREF`), and the directory's size counts
//! it twice. A file's extent is its `EXTENT_DATA`
//! item, the extent's item in the extent tree with a reference back to
//! the file, and its data's checksums, and the inode's `nbytes` counts
//! it. The caller checks what it can before a change starts, and runs it
//! in [`Files::whole`]: any failure once it has begun leaves the writer
//! refusing every change, so no half-made change is ever committed.

use radix_block::{PAGE_SIZE, Page, SECTOR, Segment};
use radix_vfs::mem::{self, KVec};
use radix_vfs::{Error, Kind, Result};

use crate::cow::Writer;
use crate::format::{Key, item, name_hash, tree, u16_at, u64_at};
use crate::inode::Inode;
use crate::refs::Backref;
use crate::superblock::incompat;
use crate::tree::{Block, Cursor, ITEM_SIZE};
use crate::volume::Volume;

const PAGE: u64 = PAGE_SIZE as u64;

/// Object IDs of a subvolume's tree that are not inodes.
pub(crate) mod object {
    /// Past the highest inode number.
    pub const LAST_FREE: u64 = (-256i64) as u64;
    /// The orphans: inodes removed while open, whose items go once they
    /// close, or at the next writable mount after a crash.
    pub const ORPHAN: u64 = (-5i64) as u64;
}

/// The item type of an orphan, keyed by its inode number.
pub(crate) const ORPHAN_ITEM: u8 = 48;

/// What a data extent's item says it is.
const EXTENT_FLAG_DATA: u64 = 1 << 0;

/// The size of a directory entry before its name.
const DIR_HEADER: usize = 30;
/// The size of an inode reference before its name.
const REF_HEADER: usize = 10;
/// The size of an extended inode reference before its name: its
/// directory, its index there and the name's length.
const EXTREF_HEADER: usize = 18;
/// The size of an extent item before its inline references.
const EXTENT_HEADER: usize = 24;
/// The size of a file's reference to a data extent: its tree, inode,
/// offset and count.
const DATA_REF: usize = 28;
/// The size of a regular file extent item.
pub(crate) const FILE_EXTENT: usize = 53;
/// The size of an inline file extent item before its data.
const INLINE_HEADER: usize = 21;

/// Where an inode item's fields are.
pub(crate) mod at {
    pub const GENERATION: usize = 0;
    pub const TRANSID: usize = 8;
    pub const SIZE: usize = 16;
    pub const NBYTES: usize = 24;
    pub const NLINK: usize = 40;
    pub const MODE: usize = 52;
    pub const RDEV: usize = 56;
    pub const FLAGS: usize = 64;
    pub const SEQUENCE: usize = 72;
    pub const ATIME: usize = 112;
    pub const CTIME: usize = 124;
    pub const MTIME: usize = 136;
    pub const OTIME: usize = 148;
}

/// How a file extent's data is compressed.
pub(crate) mod compression {
    pub const NONE: u8 = 0;
    pub const ZSTD: u8 = 3;
}

/// The kinds of file extent item.
pub(crate) mod extent_type {
    pub const INLINE: u8 = 0;
    pub const REGULAR: u8 = 1;
}

/// The most sectors one request writes, so its segments fit on the stack:
/// 128 KiB, an extent written as it is.
const MAX_SEGMENTS: usize = 32;

/// An extent of [`Files::add_extents`]: `pages` of the data written, which
/// hold the file's `len` bytes from `pos` on; the file is at least `size`
/// bytes long after it.
#[derive(Clone, Copy, Default)]
pub(crate) struct Packed {
    pub pos: u64,
    pub pages: usize,
    pub len: u64,
    pub size: u64,
}

/// Data [`Files::put_data`] wrote: where it starts, and the checksums of
/// its sectors, if it has them.
pub(crate) struct Put {
    bytenr: u64,
    checksums: KVec<u8>,
}

pub(crate) fn put_u16(bytes: &mut [u8], at: usize, value: u16) {
    bytes[at..at + 2].copy_from_slice(&value.to_le_bytes());
}

pub(crate) fn put_u32(bytes: &mut [u8], at: usize, value: u32) {
    bytes[at..at + 4].copy_from_slice(&value.to_le_bytes());
}

pub(crate) fn put_u64(bytes: &mut [u8], at: usize, value: u64) {
    bytes[at..at + 8].copy_from_slice(&value.to_le_bytes());
}

/// Writes `ns`, nanoseconds since 1970, as a `btrfs_timespec` at `at`.
pub(crate) fn put_time(bytes: &mut [u8], at: usize, ns: u64) {
    put_u64(bytes, at, ns / 1_000_000_000);
    put_u32(bytes, at + 8, (ns % 1_000_000_000) as u32);
}

/// The type byte a directory entry gives `kind`: Linux's `BTRFS_FT_*`.
pub(crate) fn file_type(kind: Kind) -> u8 {
    match kind {
        Kind::File => 1,
        Kind::Directory => 2,
        Kind::CharDevice => 3,
        Kind::BlockDevice => 4,
        Kind::Pipe => 5,
        Kind::Socket => 6,
        Kind::Symlink => 7,
    }
}

/// Where the entry named `name` is in the `DIR_ITEM` `data`: its start
/// and length.
pub(crate) fn dir_entry(data: &[u8], name: &[u8]) -> Result<Option<(usize, usize)>> {
    let mut at = 0;
    while at < data.len() {
        let b = &data[at..];
        let len = DIR_HEADER + usize::from(u16_at(b, 27)) + usize::from(u16_at(b, 25));
        if b.len() < len {
            return Err(Error::Corrupt);
        }
        if &b[DIR_HEADER..DIR_HEADER + usize::from(u16_at(b, 27))] == name {
            return Ok(Some((at, len)));
        }
        at += len;
    }
    Ok(None)
}

/// The key offset of the extended reference to `name` in directory
/// `dir`, Linux's `btrfs_extref_hash`: the CRC-32C of the name, seeded
/// with the directory's low 32 bits.
fn extref_hash(dir: u64, name: &[u8]) -> u64 {
    u64::from(crate::csum::crc32c_update(dir as u32, name))
}

/// Where the extended reference to `name` in directory `dir` is in the
/// `INODE_EXTREF` `data`: its start, its length, and its directory index.
fn extref_entry(data: &[u8], dir: u64, name: &[u8]) -> Result<Option<(usize, usize, u64)>> {
    let mut at = 0;
    while at < data.len() {
        let b = &data[at..];
        if b.len() < EXTREF_HEADER {
            return Err(Error::Corrupt);
        }
        let len = EXTREF_HEADER + usize::from(u16_at(b, 16));
        if b.len() < len {
            return Err(Error::Corrupt);
        }
        if u64_at(b, 0) == dir && &b[EXTREF_HEADER..len] == name {
            return Ok(Some((at, len, u64_at(b, 8))));
        }
        at += len;
    }
    Ok(None)
}

/// Where the reference named `name` is in the `INODE_REF` `data`: its
/// start, its length, and its directory index.
fn ref_entry(data: &[u8], name: &[u8]) -> Result<Option<(usize, usize, u64)>> {
    let mut at = 0;
    while at < data.len() {
        let b = &data[at..];
        let len = REF_HEADER + usize::from(u16_at(b, 8));
        if b.len() < len {
            return Err(Error::Corrupt);
        }
        if &b[REF_HEADER..len] == name {
            return Ok(Some((at, len, u64_at(b, 0))));
        }
        at += len;
    }
    Ok(None)
}

/// A file extent item's fields.
#[derive(Clone, Copy)]
pub(crate) struct FileExtent {
    pub(crate) kind: u8,
    pub(crate) disk_bytenr: u64,
    pub(crate) disk_num_bytes: u64,
    pub(crate) offset: u64,
    /// The file's bytes it covers.
    pub(crate) len: u64,
}

impl FileExtent {
    pub(crate) fn read(data: &[u8]) -> Result<FileExtent> {
        if data.len() < 21 {
            return Err(Error::Corrupt);
        }
        let kind = data[20];
        if kind == extent_type::INLINE {
            let len = u64_at(data, 8);
            return Ok(FileExtent { kind, disk_bytenr: 0, disk_num_bytes: 0, offset: 0, len });
        }
        if data.len() != FILE_EXTENT || kind > 2 {
            return Err(Error::Corrupt);
        }
        Ok(FileExtent {
            kind,
            disk_bytenr: u64_at(data, 21),
            disk_num_bytes: u64_at(data, 29),
            offset: u64_at(data, 37),
            len: u64_at(data, 45),
        })
    }
}

/// The data extent the file extent item `data` names: its address, its
/// size and the offset in it the item starts at; `None` if it names
/// none, inline or a hole.
pub(crate) fn disk_extent(data: &[u8]) -> Result<Option<(u64, u64, u64)>> {
    let extent = FileExtent::read(data)?;
    if extent.kind == extent_type::INLINE || extent.disk_bytenr == 0 {
        return Ok(None);
    }
    Ok(Some((extent.disk_bytenr, extent.disk_num_bytes, extent.offset)))
}

/// A subvolume, changed in the running transaction.
pub(crate) struct Files<'a> {
    pub(crate) w: &'a mut Writer,
    pub(crate) volume: &'a Volume,
    /// The subvolume: the number of its tree.
    pub(crate) subvol: u64,
    /// The inode items the last two inodes it changed have, as
    /// [`Files::change_inode`] left them: what a node of one reads again
    /// after the change, without a walk down the tree.
    pub(crate) changed: [Option<(u64, Inode)>; 2],
}

impl Files<'_> {
    /// Runs the change `f`, which checked what it could first: if it
    /// fails, it is half made, and the writer refuses every later change
    /// and commit.
    pub(crate) fn whole<R>(&mut self, f: impl FnOnce(&mut Self) -> Result<R>) -> Result<R> {
        let result = f(self);
        if result.is_err() {
            self.w.break_off();
        }
        result
    }

    /// Inode `ino`'s item as this change last left it, if it changed it.
    pub(crate) fn changed(&self, ino: u64) -> Option<Inode> {
        self.changed.iter().flatten().find(|(i, _)| *i == ino).map(|&(_, inode)| inode)
    }

    /// Notes inode `ino`'s item as a change left it, or that it went.
    fn note_inode(&mut self, ino: u64, inode: Option<Inode>) {
        let [last, before] = &mut self.changed;
        if last.is_some_and(|(i, _)| i != ino) {
            *before = last.take();
        }
        if before.is_some_and(|(i, _)| i == ino) {
            *before = None;
        }
        *last = inode.map(|inode| (ino, inode));
    }

    /// A copy of item `key` of tree `tree`.
    pub(crate) fn get(&self, tree: u64, key: &Key) -> Result<Option<KVec<u8>>> {
        crate::tree::find(self.volume, &Writer::tree_root(self.volume, tree)?, key)
    }

    /// The first item of tree `tree` at `key` or after it, copied.
    pub(crate) fn next(&self, tree: u64, key: &Key) -> Result<Option<(Key, KVec<u8>)>> {
        let root = Writer::tree_root(self.volume, tree)?;
        let cursor = Cursor::seek(self.volume, &root, key)?;
        match cursor.get() {
            Some((found, data)) => Ok(Some((found, mem::copy(data)?))),
            None => Ok(None),
        }
    }

    /// The last item of tree `tree` at `key` or before it, copied.
    fn prev(&self, tree: u64, key: &Key) -> Result<Option<(Key, KVec<u8>)>> {
        let root = Writer::tree_root(self.volume, tree)?;
        let Some(cursor) = Cursor::seek_last(self.volume, &root, key)? else { return Ok(None) };
        match cursor.get() {
            Some((found, data)) => Ok(Some((found, mem::copy(data)?))),
            None => Ok(None),
        }
    }

    /// One more than the highest inode number of the subvolume.
    pub(crate) fn next_ino(&self) -> Result<u64> {
        let last = self.prev(self.subvol, &Key::new(object::LAST_FREE, 0, 0))?;
        let highest = last.map_or(tree::FIRST_FREE, |(key, _)| key.objectid);
        Ok(highest.max(tree::FIRST_FREE) + 1)
    }

    /// The index the next entry of directory `dir` gets: one past its
    /// last, and 2 for its first, as Linux numbers them.
    pub(crate) fn next_index(&self, dir: u64) -> Result<u64> {
        Ok(match self.prev(self.subvol, &Key::last(dir, item::DIR_INDEX))? {
            Some((key, _)) if key.is(dir, item::DIR_INDEX) => key.offset + 1,
            _ => 2,
        })
    }

    /// Whether directory `dir` has no entries.
    pub(crate) fn is_empty_dir(&self, dir: u64) -> Result<bool> {
        Ok(match self.next(self.subvol, &Key::first(dir, item::DIR_ITEM))? {
            Some((key, _)) => {
                key.objectid != dir || (key.kind != item::DIR_ITEM && key.kind != item::DIR_INDEX)
            }
            None => true,
        })
    }

    /// The directory holding inode `ino`, by its first reference, and
    /// that reference's name; `None` for a subvolume's top directory.
    pub(crate) fn parent_of(&self, ino: u64) -> Result<Option<u64>> {
        match self.next(self.subvol, &Key::first(ino, item::INODE_REF))? {
            Some((key, _)) if key.is(ino, item::INODE_REF) && key.offset != ino => {
                Ok(Some(key.offset))
            }
            _ => Ok(None),
        }
    }

    /// The directory index of the name `name` that inode `ino` has in
    /// directory `dir`, from its reference.
    pub(crate) fn index_of(&self, ino: u64, dir: u64, name: &[u8]) -> Result<Option<u64>> {
        if let Some(refs) = self.get(self.subvol, &Key::new(ino, item::INODE_REF, dir))?
            && let Some((_, _, index)) = ref_entry(&refs, name)?
        {
            return Ok(Some(index));
        }
        let key = Key::new(ino, item::INODE_EXTREF, extref_hash(dir, name));
        let Some(refs) = self.get(self.subvol, &key)? else {
            return Ok(None);
        };
        Ok(extref_entry(&refs, dir, name)?.map(|(_, _, index)| index))
    }

    /// Where another name `name` of inode `ino` in directory `dir` goes:
    /// `Some(false)` in the inode's reference item there, `Some(true)` in
    /// an extended reference once that item is full, as Linux puts it, or
    /// `None` if no item has room for it (Linux's `EMLINK`).
    pub(crate) fn name_room(&self, dir: u64, name: &[u8], ino: u64) -> Result<Option<bool>> {
        let max = Block::max_item(self.volume.superblock().nodesize);
        let len =
            |key: &Key| -> Result<usize> { Ok(self.get(self.subvol, key)?.map_or(0, |d| d.len())) };
        if len(&Key::new(dir, item::DIR_ITEM, name_hash(name)))? + DIR_HEADER + name.len() > max {
            return Ok(None);
        }
        if len(&Key::new(ino, item::INODE_REF, dir))? + REF_HEADER + name.len() <= max {
            return Ok(Some(false));
        }
        let extref = Key::new(ino, item::INODE_EXTREF, extref_hash(dir, name));
        Ok((self.volume.superblock().has(incompat::EXTENDED_IREF)
            && len(&extref)? + EXTREF_HEADER + name.len() <= max)
            .then_some(true))
    }

    /// Makes inode `ino` with one name: `mode` (its type and permissions),
    /// `flags`, `size` bytes and `nbytes` of extents, made `now`.
    pub(crate) fn make_inode(
        &mut self,
        ino: u64,
        (mode, rdev): (u32, u64),
        flags: u64,
        size: u64,
        nbytes: u64,
        now: u64,
    ) -> Result<()> {
        let generation = self.w.generation;
        let key = Key::new(ino, item::INODE_ITEM, 0);
        self.w.insert_with(self.volume, self.subvol, &key, crate::inode::SIZE, |d| {
            d.fill(0);
            put_u64(d, at::GENERATION, generation);
            put_u64(d, at::TRANSID, generation);
            put_u64(d, at::SIZE, size);
            put_u64(d, at::NBYTES, nbytes);
            put_u32(d, at::NLINK, 1);
            put_u32(d, at::MODE, mode);
            put_u64(d, at::RDEV, rdev);
            put_u64(d, at::FLAGS, flags);
            for time in [at::ATIME, at::CTIME, at::MTIME, at::OTIME] {
                put_time(d, time, now);
            }
        })
    }

    /// Makes file `ino`'s one extent: `data` inline, uncompressed, as a
    /// link's path is kept.
    pub(crate) fn inline_extent(&mut self, ino: u64, data: &[u8]) -> Result<()> {
        let sector = u64::from(self.volume.superblock().sectorsize);
        self.w.logging.note(self.subvol, ino, 0, sector);
        let generation = self.w.generation;
        let key = Key::new(ino, item::EXTENT_DATA, 0);
        self.w.insert_with(self.volume, self.subvol, &key, INLINE_HEADER + data.len(), |d| {
            d.fill(0);
            put_u64(d, 0, generation);
            put_u64(d, 8, data.len() as u64);
            d[20] = extent_type::INLINE;
            d[INLINE_HEADER..].copy_from_slice(data);
        })
    }

    /// Changes inode `ino`'s item with `f`, noting the transaction did.
    pub(crate) fn change_inode<R>(
        &mut self,
        ino: u64,
        f: impl FnOnce(&mut [u8]) -> R,
    ) -> Result<R> {
        let generation = self.w.generation;
        let key = Key::new(ino, item::INODE_ITEM, 0);
        let (result, inode) = self.w.update(self.volume, self.subvol, &key, |d| {
            put_u64(d, at::TRANSID, generation);
            let sequence = u64_at(d, at::SEQUENCE);
            put_u64(d, at::SEQUENCE, sequence.wrapping_add(1));
            let result = f(d);
            (result, Inode::read(d).ok())
        })?;
        self.note_inode(ino, inode);
        Ok(result)
    }

    /// Makes inode `ino`'s item `data`, whether it has one or not.
    pub(crate) fn put_inode(&mut self, ino: u64, data: &[u8]) -> Result<()> {
        let key = Key::new(ino, item::INODE_ITEM, 0);
        match self.w.insert(self.volume, self.subvol, &key, data) {
            Err(Error::Exists) => {
                self.w.update(self.volume, self.subvol, &key, |d| d.copy_from_slice(data))?;
            }
            made => made?,
        }
        self.note_inode(ino, Inode::read(data).ok());
        Ok(())
    }

    /// Takes the name `name` in directory `dir` out of inode `ino`'s
    /// references, plain or extended; returns its index, if it had it.
    pub(crate) fn cut_reference(&mut self, ino: u64, dir: u64, name: &[u8]) -> Result<Option<u64>> {
        let key = Key::new(ino, item::INODE_REF, dir);
        if let Some(refs) = self.get(self.subvol, &key)?
            && let Some((start, len, index)) = ref_entry(&refs, name)?
        {
            self.cut(self.subvol, &key, refs.len(), start, len)?;
            return Ok(Some(index));
        }
        let key = Key::new(ino, item::INODE_EXTREF, extref_hash(dir, name));
        if let Some(refs) = self.get(self.subvol, &key)?
            && let Some((start, len, index)) = extref_entry(&refs, dir, name)?
        {
            self.cut(self.subvol, &key, refs.len(), start, len)?;
            return Ok(Some(index));
        }
        Ok(None)
    }

    /// Adds `bytes` to the end of item `key` of tree `tree`, made if it is
    /// missing.
    pub(crate) fn append(&mut self, tree: u64, key: &Key, bytes: &[u8]) -> Result<()> {
        // Most often it is missing (a name's hash or a file's directory
        // seldom has one already): one walk down the tree.
        match self.w.insert(self.volume, tree, key, bytes) {
            Err(Error::Exists) => {}
            made => return made,
        }
        let n = self.get(tree, key)?.ok_or(Error::Corrupt)?.len();
        self.w.resize(self.volume, tree, key, n + bytes.len())?;
        self.w.update(self.volume, tree, key, |d| d[n..].copy_from_slice(bytes))
    }

    /// Cuts `len` bytes from `start` out of item `key` of tree `tree`,
    /// `old` long; deletes it when nothing is left.
    pub(crate) fn cut(
        &mut self,
        tree: u64,
        key: &Key,
        old: usize,
        start: usize,
        len: usize,
    ) -> Result<()> {
        if old == len {
            return self.w.delete(self.volume, tree, key);
        }
        self.w.update(self.volume, tree, key, |d| d.copy_within(start + len.., start))?;
        self.w.resize(self.volume, tree, key, old - len)
    }

    /// Names inode `ino`, of type byte `ft`, `name` in directory `dir`, at
    /// index `index`: the directory's two entries and the inode's
    /// reference, and the directory's size and times.
    pub(crate) fn add_entry(
        &mut self,
        dir: u64,
        name: &[u8],
        ino: u64,
        ft: u8,
        index: u64,
        now: u64,
    ) -> Result<()> {
        self.add_dir_entries(dir, name, &Key::new(ino, item::INODE_ITEM, 0), ft, index, now)?;
        let mut reference = crate::zeroed(REF_HEADER + name.len())?;
        put_u64(&mut reference, 0, index);
        put_u16(&mut reference, 8, name.len() as u16);
        reference[REF_HEADER..].copy_from_slice(name);
        let key = Key::new(ino, item::INODE_REF, dir);
        match self.w.insert(self.volume, self.subvol, &key, &reference) {
            Err(Error::Exists) => self.add_reference(&key, &reference, dir, index, name),
            made => made,
        }
    }

    /// Names what `location` is, of type byte `ft`, `name` in directory
    /// `dir`, at index `index`: the directory's two entries, and its size
    /// and times.
    pub(crate) fn add_dir_entries(
        &mut self,
        dir: u64,
        name: &[u8],
        location: &Key,
        ft: u8,
        index: u64,
        now: u64,
    ) -> Result<()> {
        let mut entry = crate::zeroed(DIR_HEADER + name.len())?;
        put_u64(&mut entry, 0, location.objectid);
        entry[8] = location.kind;
        put_u64(&mut entry, 9, location.offset);
        put_u64(&mut entry, 17, self.w.generation);
        put_u16(&mut entry, 27, name.len() as u16);
        entry[29] = ft;
        entry[DIR_HEADER..].copy_from_slice(name);
        self.append(self.subvol, &Key::new(dir, item::DIR_ITEM, name_hash(name)), &entry)?;
        self.w.insert(self.volume, self.subvol, &Key::new(dir, item::DIR_INDEX, index), &entry)?;
        self.change_inode(dir, |d| {
            put_u64(d, at::SIZE, u64_at(d, at::SIZE) + 2 * name.len() as u64);
            put_time(d, at::MTIME, now);
            put_time(d, at::CTIME, now);
        })
    }

    /// Adds `reference`, a name in directory `dir` at `index`, to the
    /// inode's reference item `key` there, or once that item is full to
    /// an extended reference, as Linux does; the caller checked one has
    /// room ([`Files::name_room`]).
    fn add_reference(
        &mut self,
        key: &Key,
        reference: &[u8],
        dir: u64,
        index: u64,
        name: &[u8],
    ) -> Result<()> {
        let n = self.get(self.subvol, key)?.ok_or(Error::Corrupt)?.len();
        if n + reference.len() <= Block::max_item(self.volume.superblock().nodesize) {
            self.w.resize(self.volume, self.subvol, key, n + reference.len())?;
            return self.w.update(self.volume, self.subvol, key, |d| {
                d[n..].copy_from_slice(reference);
            });
        }
        let mut extended = crate::zeroed(EXTREF_HEADER + name.len())?;
        put_u64(&mut extended, 0, dir);
        put_u64(&mut extended, 8, index);
        put_u16(&mut extended, 16, name.len() as u16);
        extended[EXTREF_HEADER..].copy_from_slice(name);
        let key = Key::new(key.objectid, item::INODE_EXTREF, extref_hash(dir, name));
        self.append(self.subvol, &key, &extended)
    }

    /// Takes the name `name` of inode `ino` out of directory `dir`: the
    /// directory's two entries and the inode's reference, and the
    /// directory's size and times.
    pub(crate) fn remove_entry(&mut self, dir: u64, name: &[u8], ino: u64, now: u64) -> Result<()> {
        self.cut_dir_item(dir, name)?;
        let ref_key = Key::new(ino, item::INODE_REF, dir);
        let found = match self.get(self.subvol, &ref_key)? {
            Some(refs) => ref_entry(&refs, name)?.map(|at| (ref_key, refs.len(), at)),
            None => None,
        };
        let (key, old, (start, len, index)) = match found {
            Some(found) => found,
            None => {
                let key = Key::new(ino, item::INODE_EXTREF, extref_hash(dir, name));
                let refs = self.get(self.subvol, &key)?.ok_or(Error::Corrupt)?;
                (key, refs.len(), extref_entry(&refs, dir, name)?.ok_or(Error::Corrupt)?)
            }
        };
        self.cut(self.subvol, &key, old, start, len)?;
        self.drop_dir_index(dir, name, index, now)
    }

    /// Takes the entry `name` out of directory `dir`'s `DIR_ITEM`.
    pub(crate) fn cut_dir_item(&mut self, dir: u64, name: &[u8]) -> Result<()> {
        let key = Key::new(dir, item::DIR_ITEM, name_hash(name));
        let entries = self.get(self.subvol, &key)?.ok_or(Error::Corrupt)?;
        let (start, len) = dir_entry(&entries, name)?.ok_or(Error::Corrupt)?;
        self.cut(self.subvol, &key, entries.len(), start, len)
    }

    /// Deletes directory `dir`'s entry at `index`, named `name`, and sets
    /// its size and times.
    pub(crate) fn drop_dir_index(
        &mut self,
        dir: u64,
        name: &[u8],
        index: u64,
        now: u64,
    ) -> Result<()> {
        self.w.delete(self.volume, self.subvol, &Key::new(dir, item::DIR_INDEX, index))?;
        self.change_inode(dir, |d| {
            put_u64(d, at::SIZE, u64_at(d, at::SIZE).saturating_sub(2 * name.len() as u64));
            put_time(d, at::MTIME, now);
            put_time(d, at::CTIME, now);
        })
    }

    /// Notes that inode `ino`, which has no names left, is still open: it
    /// goes once it closes, or at the next writable mount.
    pub(crate) fn add_orphan(&mut self, ino: u64) -> Result<()> {
        let key = Key::new(object::ORPHAN, ORPHAN_ITEM, ino);
        self.w.insert(self.volume, self.subvol, &key, &[])
    }

    /// The orphans of the subvolume, at most `max` of them.
    pub(crate) fn orphans(&self, max: usize) -> Result<KVec<u64>> {
        let mut out = mem::vec();
        let mut key = Key::first(object::ORPHAN, ORPHAN_ITEM);
        while out.len() < max
            && let Some((found, _)) = self.next(self.subvol, &key)?
            && found.is(object::ORPHAN, ORPHAN_ITEM)
        {
            mem::push(&mut out, found.offset)?;
            key = Key::new(object::ORPHAN, ORPHAN_ITEM, found.offset.saturating_add(1));
            if found.offset == u64::MAX {
                break;
            }
        }
        Ok(out)
    }

    /// Deletes inode `ino`: every item it has, its extents, and its
    /// orphan item.
    pub(crate) fn free_inode(&mut self, ino: u64, orphan: bool) -> Result<()> {
        self.drop_extents(ino, 0, u64::MAX)?;
        self.note_inode(ino, None);
        // The inode item, then whatever else is left (extended attributes).
        match self.w.delete(self.volume, self.subvol, &Key::new(ino, item::INODE_ITEM, 0)) {
            Ok(()) | Err(Error::NotFound) => {}
            Err(e) => return Err(e),
        }
        while let Some((key, _)) = self.next(self.subvol, &Key::new(ino, 0, 0))?
            && key.objectid == ino
        {
            self.w.delete(self.volume, self.subvol, &key)?;
        }
        if !orphan {
            return Ok(());
        }
        match self.w.delete(self.volume, self.subvol, &Key::new(object::ORPHAN, ORPHAN_ITEM, ino)) {
            Ok(()) | Err(Error::NotFound) => Ok(()),
            Err(e) => Err(e),
        }
    }

    /// The extent of file `ino` that holds byte `pos`, or the first after
    /// it.
    fn extent_at(&self, ino: u64, pos: u64) -> Result<Option<(Key, KVec<u8>)>> {
        self.extent_in(self.subvol, ino, pos)
    }

    /// [`Files::extent_at`] for a file of the subvolume whose tree is
    /// `tree`.
    fn extent_in(&self, tree: u64, ino: u64, pos: u64) -> Result<Option<(Key, KVec<u8>)>> {
        let key = Key::new(ino, item::EXTENT_DATA, pos);
        // An extent before `pos` may hold it; none starts before byte 0.
        if pos > 0
            && let Some((found, data)) = self.prev(tree, &key)?
            && found.is(ino, item::EXTENT_DATA)
            && found.offset.saturating_add(FileExtent::read(&data)?.len) > pos
        {
            return Ok(Some((found, data)));
        }
        Ok(self.next(tree, &key)?.filter(|(found, _)| found.is(ino, item::EXTENT_DATA)))
    }

    /// Makes file `ino`'s `len` bytes from `to` on (whole sectors) name
    /// the data file `source` of the subvolume whose tree is `from` names
    /// from `pos` on, as Linux's `btrfs_clone` does: an item each for the
    /// same extents, with a reference of the file's, after what the file
    /// named there goes. Holes stay holes. An inline extent, which must be
    /// a file's only one, is copied only to the start of a file that
    /// holds nothing past the range; otherwise this fails with
    /// [`Error::NotSupported`], before any change. The file grows to
    /// `size` if it is smaller. The two ranges are apart.
    pub(crate) fn clone_extents(
        &mut self,
        (from, source, pos): (u64, u64, u64),
        len: u64,
        (ino, to): (u64, u64),
        size: u64,
    ) -> Result<()> {
        let stop = pos.checked_add(len).ok_or(Error::InvalidArgument)?;
        let end = to.checked_add(len).ok_or(Error::InvalidArgument)?;
        if let Some((_, data)) = self.extent_in(from, source, pos)?
            && FileExtent::read(&data)?.kind == extent_type::INLINE
            && (pos != 0 || to != 0 || self.extent_in(self.subvol, ino, end)?.is_some())
        {
            return Err(Error::NotSupported);
        }
        let generation = self.w.generation;
        self.whole(|f| {
            let lost = f.drop_extents(ino, to, end)?;
            let mut gained = 0;
            let mut cur = pos;
            while cur < stop
                && let Some((key, data)) = f.extent_in(from, source, cur)?
                && key.offset < stop
            {
                let extent = FileExtent::read(&data)?;
                let (a, b) =
                    (key.offset, key.offset.checked_add(extent.len).ok_or(Error::Corrupt)?);
                if b <= cur {
                    return Err(Error::Corrupt);
                }
                let (start, stop_at) = (a.max(cur), b.min(stop));
                cur = b;
                let mut copy = mem::copy(&data)?;
                put_u64(&mut copy, 0, generation);
                if extent.kind == extent_type::INLINE {
                    f.w.insert(f.volume, f.subvol, &Key::new(ino, item::EXTENT_DATA, 0), &copy)?;
                    gained += extent.len;
                    continue;
                }
                if extent.disk_bytenr == 0 {
                    continue;
                }
                let offset = extent.offset + (start - a);
                let place = to + (start - pos);
                put_u64(&mut copy, 37, offset);
                put_u64(&mut copy, 45, stop_at - start);
                f.w.insert(f.volume, f.subvol, &Key::new(ino, item::EXTENT_DATA, place), &copy)?;
                // As every reference of a file's: where the extent would
                // start in it.
                f.add_data_ref(
                    extent.disk_bytenr,
                    extent.disk_num_bytes,
                    ino,
                    place.wrapping_sub(offset),
                )?;
                gained += stop_at - start;
            }
            f.change_inode(ino, |d| {
                put_u64(d, at::NBYTES, u64_at(d, at::NBYTES).saturating_sub(lost) + gained);
                if u64_at(d, at::SIZE) < size {
                    put_u64(d, at::SIZE, size);
                }
            })
        })
    }

    /// Whether file `ino` keeps its bytes inline, in its one extent item.
    pub(crate) fn has_inline(&self, ino: u64) -> Result<bool> {
        let key = Key::new(ino, item::EXTENT_DATA, 0);
        Ok(match self.get(self.subvol, &key)? {
            Some(data) => FileExtent::read(&data)?.kind == extent_type::INLINE,
            None => false,
        })
    }

    /// Drops file `ino`'s bytes from `start` to `end` (whole sectors, or
    /// to `u64::MAX`), leaving a hole: extents that cross either end are
    /// cut, and those wholly inside go, with their data once no file
    /// names it. An inline extent only goes whole, from the start.
    /// Returns how many bytes the inode's `nbytes` loses.
    pub(crate) fn drop_extents(&mut self, ino: u64, start: u64, end: u64) -> Result<u64> {
        // What an fsync's log holds of the file covers this range.
        self.w.logging.note(self.subvol, ino, start, end);
        let mut lost = 0;
        let mut pos = start;
        while pos < end
            && let Some((key, data)) = self.extent_at(ino, pos)?
            && key.offset < end
        {
            let extent = FileExtent::read(&data)?;
            let (a, b) = (key.offset, key.offset.checked_add(extent.len).ok_or(Error::Corrupt)?);
            if b <= pos {
                return Err(Error::Corrupt);
            }
            if extent.kind == extent_type::INLINE {
                if a != 0 || start != 0 {
                    return Err(Error::NotSupported);
                }
                self.w.delete(self.volume, self.subvol, &key)?;
                lost += extent.len;
                pos = b;
                continue;
            }
            // Holes take no space; preallocated extents do.
            let counts = extent.disk_bytenr != 0;
            // What the extent's reference back to the file says: where the
            // extent would start in the file, which no cut changes.
            let ref_offset = a.wrapping_sub(extent.offset);
            if a < start {
                // The front stays.
                let keep = start - a;
                self.w.update(self.volume, self.subvol, &key, |d| put_u64(d, 45, keep))?;
                if b > end {
                    // And the back too: a second item names the extent.
                    let mut back = mem::copy(&data)?;
                    put_u64(&mut back, 37, extent.offset + (end - a));
                    put_u64(&mut back, 45, b - end);
                    let at = Key::new(ino, item::EXTENT_DATA, end);
                    self.w.insert(self.volume, self.subvol, &at, &back)?;
                    if counts {
                        self.add_data_ref(
                            extent.disk_bytenr,
                            extent.disk_num_bytes,
                            ino,
                            ref_offset,
                        )?;
                        lost += end - start;
                    }
                    break;
                }
                if counts {
                    lost += b - start;
                }
            } else if b > end {
                // The back stays: its item moves to `end`.
                self.w.delete(self.volume, self.subvol, &key)?;
                let mut back = mem::copy(&data)?;
                put_u64(&mut back, 37, extent.offset + (end - a));
                put_u64(&mut back, 45, b - end);
                self.w.insert(
                    self.volume,
                    self.subvol,
                    &Key::new(ino, item::EXTENT_DATA, end),
                    &back,
                )?;
                if counts {
                    lost += end - a;
                }
                break;
            } else {
                self.w.delete(self.volume, self.subvol, &key)?;
                if counts {
                    lost += extent.len;
                    self.drop_data_ref(extent.disk_bytenr, extent.disk_num_bytes, ino, ref_offset)?;
                }
            }
            pos = b;
        }
        Ok(lost)
    }

    /// Writes `pages`, file `ino`'s bytes from `pos` on, to a new extent,
    /// with their checksums unless `sums` is false, and points the file at
    /// it. The file's inode gets the extent's bytes and grows to `size` if
    /// it is smaller. Nothing changes if writing the data fails.
    pub(crate) fn write_data(
        &mut self,
        ino: u64,
        pos: u64,
        pages: &[&Page],
        sums: bool,
        size: u64,
    ) -> Result<()> {
        let len = pages.len() as u64 * PAGE;
        self.write_extent(ino, pos, pages, (len, compression::NONE), sums, size)
    }

    fn write_extent(
        &mut self,
        ino: u64,
        pos: u64,
        pages: &[&Page],
        (len, kind): (u64, u8),
        sums: bool,
        size: u64,
    ) -> Result<()> {
        let put = self.put_data(pages, sums)?;
        self.add_extents(ino, &put, &[Packed { pos, pages: pages.len(), len, size }], kind)
    }

    /// Writes `pages` to one new run of the disk, with the checksums of
    /// their sectors unless `sums` is false, for [`Files::add_extents`];
    /// nothing else may be allocated in between. Its own frame, so the
    /// pages' segments are off the stack while the trees change.
    #[inline(never)]
    pub(crate) fn put_data(&mut self, pages: &[&Page], sums: bool) -> Result<Put> {
        let disk = pages.len() as u64 * PAGE;
        let csum = self.volume.superblock().csum_type;
        let size = csum.size();
        let mut checksums = mem::vec();
        if sums {
            checksums = crate::zeroed(pages.len() * size)?;
            for (page, out) in pages.iter().zip(checksums.chunks_exact_mut(size)) {
                out.copy_from_slice(&csum.sum(page.bytes())[..size]);
            }
        }
        let bytenr = self.w.alloc_data(self.volume, disk)?;
        if let Err(e) = write_pages(self.volume, bytenr, pages) {
            self.w.free_data(bytenr, disk, true)?;
            return Err(e);
        }
        Ok(Put { bytenr, checksums })
    }

    /// Makes `extents` of the data [`Files::put_data`] wrote, each of its
    /// pages in turn, compressed as `kind` says ([`compression`]), and
    /// points file `ino` at them. The file's inode gets their bytes and
    /// grows to their sizes.
    pub(crate) fn add_extents(
        &mut self,
        ino: u64,
        put: &Put,
        extents: &[Packed],
        kind: u8,
    ) -> Result<()> {
        let size = self.volume.superblock().csum_type.size();
        let (sums, generation) = (!put.checksums.is_empty(), self.w.generation);
        self.whole(|f| {
            let (mut at, mut lost, mut len, mut end) = (0, 0, 0, 0);
            for e in extents {
                let (start, disk) = (put.bytenr + at as u64 * PAGE, e.pages as u64 * PAGE);
                lost += f.drop_extents(ino, e.pos, e.pos + e.len)?;
                let key = Key::new(ino, item::EXTENT_DATA, e.pos);
                f.w.insert_with(f.volume, f.subvol, &key, FILE_EXTENT, |d| {
                    d.fill(0);
                    put_u64(d, 0, generation);
                    put_u64(d, 8, e.len);
                    d[16] = kind;
                    d[20] = extent_type::REGULAR;
                    put_u64(d, 21, start);
                    put_u64(d, 29, disk);
                    put_u64(d, 45, e.len);
                })?;
                f.new_data_extent(start, disk, ino, e.pos)?;
                if sums {
                    f.add_sums(start, &put.checksums[at * size..(at + e.pages) * size])?;
                }
                (at, len, end) = (at + e.pages, len + e.len, end.max(e.size));
            }
            f.change_inode(ino, |d| {
                let nbytes = u64_at(d, at::NBYTES).saturating_sub(lost) + len;
                put_u64(d, at::NBYTES, nbytes);
                if u64_at(d, at::SIZE) < end {
                    put_u64(d, at::SIZE, end);
                }
            })
        })?;
        if kind == compression::ZSTD {
            self.w.incompat |= incompat::COMPRESS_ZSTD;
        }
        Ok(())
    }

    /// Makes the item of the data extent `[bytenr, bytenr + len)`, which
    /// file `ino` names from byte `pos` on: this subvolume's, to simple
    /// quotas (ADR 0170).
    pub(crate) fn new_data_extent(
        &mut self,
        bytenr: u64,
        len: u64,
        ino: u64,
        pos: u64,
    ) -> Result<()> {
        let owner = crate::quota::simple(self.volume).then_some(self.subvol);
        self.new_owned_extent(bytenr, len, ino, pos, owner)
    }

    /// As [`Files::new_data_extent`], the extent counted to `owner`, if
    /// it names one: its inline reference comes first, as Linux's
    /// `EXTENT_OWNER_REF`.
    pub(crate) fn new_owned_extent(
        &mut self,
        bytenr: u64,
        len: u64,
        ino: u64,
        pos: u64,
        owner: Option<u64>,
    ) -> Result<()> {
        let (generation, root) = (self.w.generation, self.subvol);
        let key = Key::new(bytenr, item::EXTENT_ITEM, len);
        let first = EXTENT_HEADER + if owner.is_some() { 9 } else { 0 };
        self.w.insert_with(self.volume, tree::EXTENT, &key, first + 1 + DATA_REF, |d| {
            put_u64(d, 0, 1);
            put_u64(d, 8, generation);
            put_u64(d, 16, EXTENT_FLAG_DATA);
            if let Some(owner) = owner {
                d[EXTENT_HEADER] = crate::refs::OWNER_REF;
                put_u64(d, EXTENT_HEADER + 1, owner);
            }
            d[first] = item::EXTENT_DATA_REF;
            put_u64(d, first + 1, root);
            put_u64(d, first + 9, ino);
            put_u64(d, first + 17, pos);
            put_u32(d, first + 25, 1);
        })?;
        match owner {
            Some(owner) => Writer::account(self.volume, owner, generation, len as i64),
            None => Ok(()),
        }
    }

    /// Adds one to file `ino`'s count of names for the data extent
    /// `[bytenr, bytenr + len)` with `offset`: a cut made it two items.
    pub(crate) fn add_data_ref(
        &mut self,
        bytenr: u64,
        len: u64,
        ino: u64,
        offset: u64,
    ) -> Result<()> {
        let key = Key::new(bytenr, item::EXTENT_ITEM, len);
        self.w.add_ref(self.volume, &key, Backref::Data { root: self.subvol, ino, offset }, 1)
    }

    /// Takes one from file `ino`'s count of names for the data extent
    /// `[bytenr, bytenr + len)` with `offset`, and frees the extent and
    /// its checksums once nothing refers to it.
    fn drop_data_ref(&mut self, bytenr: u64, len: u64, ino: u64, offset: u64) -> Result<()> {
        let key = Key::new(bytenr, item::EXTENT_ITEM, len);
        self.drop_data(&key, Backref::Data { root: self.subvol, ino, offset })
    }

    /// Takes the reference `r` from the data extent of item `key`, and
    /// frees the extent and its checksums once nothing refers to it.
    pub(crate) fn drop_data(&mut self, key: &Key, r: Backref) -> Result<()> {
        let data = self.get(tree::EXTENT, key)?.ok_or(Error::Corrupt)?;
        // An extent only this refers to, the most common, goes as it is.
        if !r.only(&data)? && self.w.drop_ref(self.volume, key, r)? != 0 {
            return Ok(());
        }
        self.w.delete(self.volume, tree::EXTENT, key)?;
        self.delete_sums(key.objectid, key.offset)?;
        if let Some(owner) = crate::quota::owner_of(&data) {
            Writer::account(self.volume, owner, u64_at(&data, 8), -(key.offset as i64))?;
        }
        // An extent this transaction wrote is named by no tree on the disk.
        let made = u64_at(&data, 8) == self.w.generation;
        self.w.free_data(key.objectid, key.offset, made)
    }

    /// The key of the checksum item from logical address `at`.
    fn sum_key(at: u64) -> Key {
        Key::new(tree::EXTENT_CSUM, item::EXTENT_CSUM, at)
    }

    /// Adds `sums`, the checksums of the data sectors from `bytenr` on, to
    /// the checksum tree, in items no larger than Linux makes them.
    pub(crate) fn add_sums(&mut self, bytenr: u64, sums: &[u8]) -> Result<()> {
        let sb = self.volume.superblock();
        let size = sb.csum_type.size();
        let sector = u64::from(sb.sectorsize);
        let per_item = (Block::max_item(sb.nodesize) - ITEM_SIZE) / size - 1;
        for (n, chunk) in sums.chunks(per_item * size).enumerate() {
            let at = bytenr + (n * per_item) as u64 * sector;
            self.w.insert(self.volume, tree::CSUM, &Self::sum_key(at), chunk)?;
        }
        Ok(())
    }

    /// Deletes the checksums of the data sectors in `[bytenr, bytenr +
    /// len)`, cutting the items that cross either end.
    pub(crate) fn delete_sums(&mut self, bytenr: u64, len: u64) -> Result<()> {
        let sb = self.volume.superblock();
        let size = sb.csum_type.size() as u64;
        let sector = u64::from(sb.sectorsize);
        let end = bytenr + len;
        let mut pos = bytenr;
        while pos < end {
            let covers = |(found, data): &(Key, KVec<u8>)| {
                found.is(tree::EXTENT_CSUM, item::EXTENT_CSUM)
                    && found.offset + data.len() as u64 / size * sector > pos
            };
            let found = match self.prev(tree::CSUM, &Self::sum_key(pos))?.filter(covers) {
                Some(found) => Some(found),
                None => self
                    .next(tree::CSUM, &Self::sum_key(pos))?
                    .filter(|(k, _)| k.is(tree::EXTENT_CSUM, item::EXTENT_CSUM)),
            };
            let Some((key, data)) = found else { break };
            let (a, b) = (key.offset, key.offset + data.len() as u64 / size * sector);
            if a >= end {
                break;
            }
            let front = (bytenr.saturating_sub(a) / sector * size) as usize;
            if b > end {
                let from = ((end - a) / sector * size) as usize;
                if front == 0 {
                    self.w.delete(self.volume, tree::CSUM, &key)?;
                } else {
                    self.w.resize(self.volume, tree::CSUM, &key, front)?;
                }
                self.w.insert(self.volume, tree::CSUM, &Self::sum_key(end), &data[from..])?;
                break;
            }
            if front == 0 {
                self.w.delete(self.volume, tree::CSUM, &key)?;
            } else {
                self.w.resize(self.volume, tree::CSUM, &key, front)?;
            }
            pos = b;
        }
        Ok(())
    }
}

/// Writes `pages` to the data at logical address `bytenr`, on every copy.
/// A disk that fails is left out, while the others hold every chunk
/// ([`Volume::write_failed`]).
#[inline(never)]
pub(crate) fn write_pages(volume: &Volume, bytenr: u64, pages: &[&Page]) -> Result<()> {
    let mut done = 0;
    while done < pages.len() {
        let logical = bytenr + done as u64 * PAGE;
        let place = volume.place(logical)?;
        let n = usize::try_from(place.len / PAGE)
            .unwrap_or(usize::MAX)
            .min(pages.len() - done)
            .min(MAX_SEGMENTS);
        if n == 0 {
            return Err(Error::Corrupt);
        }
        let segments: [Segment<'_>; MAX_SEGMENTS] = core::array::from_fn(|i| Segment {
            page: pages[done + i.min(n - 1)],
            offset: 0,
            len: PAGE_SIZE,
        });
        for site in place.copies() {
            let disk = volume.disk(site.disk)?;
            if !disk.holds(site.at, n as u64 * PAGE) {
                return Err(Error::Corrupt);
            }
            let device = disk.device().ok_or(Error::NoDevice)?;
            let max = device.info().max_segments.clamp(1, MAX_SEGMENTS);
            let written = (site.at..)
                .step_by(max * PAGE_SIZE)
                .zip(segments[..n].chunks(max))
                .try_for_each(|(at, part)| device.write(at / SECTOR, part));
            if let Err(e) = written {
                volume.write_failed(site.disk, e.into())?;
            }
        }
        done += n;
    }
    Ok(())
}
