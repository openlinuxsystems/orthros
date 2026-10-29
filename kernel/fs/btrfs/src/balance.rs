// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Balancing, and adding and removing disks (ADR 0159), as Linux's
//! `btrfs balance` and `btrfs device` do. A balance moves a block group's
//! extents to other groups, a batch at a time with a commit after each,
//! and takes its chunk out once it is empty. The chunks made meanwhile
//! may take another profile, which converts the filesystem; a disk no
//! chunk may go on has its chunks moved off it, which removes it.
//!
//! A tree block moves as a change below it would move it: each tree that
//! reaches it copies the path down to it ([`Writer::copy_block`]), and
//! the last copy frees it. A data extent's bytes are copied, each sector
//! from a copy its checksum matches, to a new extent, and each file
//! extent item that names it is pointed at the new one, its reference
//! moving with it. Unlike Linux, which moves a block that snapshots share
//! once and keeps it shared, each tree here takes a copy of its own: a
//! balance unshares what snapshots share along the paths it copies.

use radix_block::cache::BlockCache;
use radix_block::mem::CachePage;
use radix_block::{DeviceRef, PAGE_SIZE, Page, SECTOR};
use radix_vfs::mem::{self, KVec};
use radix_vfs::{BALANCE_SINGLE, Balance, Balanced, Error, Result};

use crate::chunk::{MAX_COPIES, Site, kind};
use crate::cow::Writer;
use crate::extent;
use crate::files::{disk_extent, put_u64, write_pages};
use crate::format::{Key, item, tree, u64_at};
use crate::fs::{Inner, Txn};
use crate::grow::{Grown, disks_needed};
use crate::refs::{Backref, backrefs};
use crate::scrub::{SECTORS, read_pages};
use crate::superblock::incompat;
use crate::tree::{Bounds, Cursor, MAX_LEVEL, Root, read_block};
use crate::volume::{DEV_ITEMS, Volume};

const PAGE: u64 = PAGE_SIZE as u64;

/// The bytes of extents a batch moves before its commit.
const BATCH_BYTES: u64 = 64 << 20;

/// The most times a block or extent is looked at again for what still
/// refers to it, each time one less: past it, something is wrong.
const MAX_ROUNDS: usize = 1 << 20;

/// The flag of a data extent's item.
const EXTENT_FLAG_DATA: u64 = 1;

/// Where a file extent item names its extent.
const DISK_BYTENR_AT: usize = 21;

/// Where a device item keeps its ID, size, bytes used and UUID.
const DEV_TOTAL_AT: usize = 8;
const DEV_USED_AT: usize = 16;
const DEV_UUID_AT: usize = 66;
const DEV_ITEM_SIZE: usize = 98;

/// The key type of a disk's error counts in the device tree, Linux's
/// `BTRFS_PERSISTENT_ITEM_KEY`.
const PERSISTENT_ITEM: u8 = 249;

/// The smallest disk added: room for a chunk of each kind past the first
/// MiB, which no chunk takes.
const MIN_DISK: u64 = 64 << 20;

/// Moves the chunks `plan` names, converting them if it says so, as
/// Linux's `btrfs balance start` does; returns how many chunks it looked
/// at and how many it moved. Each chunk is moved alone, and stays where
/// it was if moving it fails: a balance that fails part way leaves the
/// chunks it moved moved.
///
/// Fails with [`Error::ReadOnly`] on a read-only mount,
/// [`Error::InvalidArgument`] for a profile Btrfs does not have or one
/// that needs more disks than are there, [`Error::NotSupported`] for
/// RAID5 and RAID6 and for a convert of a filesystem whose chunks mix
/// data and metadata, and [`Error::NoSpace`] when the other chunks and
/// the disks have no room for what a chunk holds.
pub(crate) fn balance(inner: &Inner, plan: &Balance) -> Result<Balanced> {
    let volume = &*inner.volume;
    if !inner.writable() {
        return Err(Error::ReadOnly);
    }
    let args = [plan.data, plan.metadata, plan.system];
    let kinds = [kind::DATA, kind::METADATA, kind::SYSTEM];
    let disks = volume.disks().iter().filter(|d| d.present()).count();
    let mut targets = [None; 3];
    for (at, a) in args.iter().enumerate() {
        if !a.on || a.convert == 0 {
            continue;
        }
        let profile = match a.convert {
            BALANCE_SINGLE => 0,
            p if p & !kind::PROFILE == 0 && p.is_power_of_two() => p,
            _ => return Err(Error::InvalidArgument),
        };
        if profile & (kind::RAID5 | kind::RAID6) != 0 {
            return Err(Error::NotSupported);
        }
        let needs = disks_needed(profile).ok_or(Error::InvalidArgument)?;
        if disks < needs {
            return Err(Error::InvalidArgument);
        }
        targets[at] = Some(kinds[at] | profile);
    }
    let groups = inner.reading().ok_or(Error::ReadOnly)?.writer.space.groups()?;
    let mixed = |flags: u64| flags & kind::DATA != 0 && flags & kind::METADATA != 0;
    if targets.iter().any(Option::is_some) && groups.iter().any(|g| mixed(g.2)) {
        return Err(Error::NotSupported);
    }
    {
        let mut txn = inner.reading().ok_or(Error::ReadOnly)?;
        txn.writer.grown.targets = targets;
        let many = kind::RAID1C3 | kind::RAID1C4;
        if targets.iter().flatten().any(|t| t & many != 0) {
            txn.writer.incompat |= incompat::RAID1C34;
        }
    }
    let mut done = Balanced::default();
    let mut result = Ok(());
    for &(start, _, flags) in &groups {
        done.considered += 1;
        let at = Grown::target_at(flags);
        let (a, target) = (args[at], targets[at]);
        let converted = target.is_some_and(|t| t & kind::PROFILE == flags & kind::PROFILE);
        if !a.on || (a.soft && converted) {
            continue;
        }
        result = relocate(inner, start);
        if result.is_err() {
            break;
        }
        done.moved += 1;
    }
    if let Some(mut txn) = inner.reading() {
        txn.writer.grown.targets = [None; 3];
    }
    result.map(|()| done)
}

/// Adds `device` as a disk of the filesystem, as Linux's `btrfs device
/// add` does, and returns its device ID: its device item goes in the
/// chunk tree, and the commit writes it its superblock. Its old
/// superblocks are wiped first, so that it is not taken for another
/// filesystem's after a power cut.
///
/// Fails with [`Error::ReadOnly`] on a read-only mount, [`Error::InUse`]
/// when `device` is one of the filesystem's disks, [`Error::NoSpace`]
/// when it is smaller than [`MIN_DISK`], and [`Error::NotSupported`]
/// when its blocks are larger than the filesystem's sectors or the
/// filesystem has as many disks as it can since the mount.
pub(crate) fn add_disk(inner: &Inner, device: &DeviceRef) -> Result<u64> {
    let volume = &*inner.volume;
    if !inner.writable() {
        return Err(Error::ReadOnly);
    }
    if volume.disks().iter().filter_map(|d| d.device()).any(|d| DeviceRef::ptr_eq(d, device)) {
        return Err(Error::InUse);
    }
    let info = device.info();
    let sector = u64::from(volume.superblock().sectorsize);
    if info.logical_block > volume.superblock().sectorsize {
        return Err(Error::NotSupported);
    }
    if info.read_only {
        return Err(Error::ReadOnly);
    }
    let total = info.sectors * SECTOR / sector * sector;
    if total < MIN_DISK {
        return Err(Error::NoSpace);
    }
    crate::replace::wipe(&BlockCache::new(device.clone()))?;
    inner.alone(false, |txn| {
        // Another disk's device item, with the new one's ID, size and
        // UUID: the rest, its sector size and the filesystem's UUID, they
        // share.
        let like = volume.disks().iter().find(|d| !d.gone()).ok_or(Error::Corrupt)?.devid;
        let mut dev = dev_item(volume, like)?;
        let (index, devid, uuid) = volume.add_disk(device)?;
        put_u64(&mut dev, 0, devid);
        put_u64(&mut dev, DEV_TOTAL_AT, total);
        put_u64(&mut dev, DEV_USED_AT, 0);
        dev[DEV_UUID_AT..DEV_UUID_AT + 16].copy_from_slice(&uuid);
        let key = Key::new(DEV_ITEMS, item::DEV_ITEM, devid);
        let added = txn.writer.insert(volume, tree::CHUNK, &key, &dev);
        if let Err(e) = added.and_then(|()| txn.writer.device_added(devid, total)) {
            let _ = volume.remove_disk(index);
            return Err(e);
        }
        Ok(devid)
    })
}

/// Moves every chunk off the disk of device ID `devid`, there or
/// missing, then takes it out of the filesystem, as Linux's `btrfs device
/// remove` does: its device item goes, the commit counts one disk less,
/// and its superblocks, if it is there, are wiped.
///
/// Fails with [`Error::ReadOnly`] on a read-only mount,
/// [`Error::NotFound`] when no disk has that ID, [`Error::InvalidArgument`]
/// when the disks left are too few for a profile the filesystem uses, or
/// none, and [`Error::NoSpace`] when they have no room for its chunks.
pub(crate) fn remove_disk(inner: &Inner, devid: u64) -> Result<()> {
    let volume = &*inner.volume;
    if !inner.writable() {
        return Err(Error::ReadOnly);
    }
    let disks = volume.disks();
    let index = disks.iter().position(|d| d.devid == devid && !d.gone()).ok_or(Error::NotFound)?;
    let left = disks.iter().filter(|d| d.devid != devid && d.present()).count();
    {
        let mut txn = inner.reading().ok_or(Error::ReadOnly)?;
        for (_, _, flags) in txn.writer.space.groups()? {
            let needs = disks_needed(flags & kind::PROFILE).ok_or(Error::NotSupported)?;
            if left < needs.max(1) {
                return Err(Error::InvalidArgument);
            }
        }
        txn.writer.keep_chunks_off(volume, devid, true)?;
    }
    let moved = move_off(inner, devid);
    let removed = moved.and_then(|()| {
        inner.alone(false, |txn| {
            let key = Key::new(DEV_ITEMS, item::DEV_ITEM, devid);
            let total = u64_at(&dev_item(volume, devid)?, DEV_TOTAL_AT);
            txn.writer.delete(volume, tree::CHUNK, &key)?;
            match txn.writer.delete(volume, tree::DEV, &Key::new(0, PERSISTENT_ITEM, devid)) {
                Ok(()) | Err(Error::NotFound) => {}
                Err(e) => return Err(e),
            }
            txn.writer.device_removed(devid, total);
            Ok(volume.remove_disk(index))
        })
    });
    match removed {
        // Wiped once the commit is on the other disks; a disk failing
        // may refuse it.
        Ok(cache) => {
            if let Some(cache) = cache {
                let _ = crate::replace::wipe(cache);
            }
            Ok(())
        }
        Err(e) => {
            if let Some(mut txn) = inner.reading() {
                let _ = txn.writer.keep_chunks_off(volume, devid, false);
            }
            Err(e)
        }
    }
}

/// Moves each chunk with a stripe on disk `devid` to new ones, which
/// never go on it.
fn move_off(inner: &Inner, devid: u64) -> Result<()> {
    let volume = &*inner.volume;
    loop {
        let on = |c: &crate::chunk::Chunk| c.stripes.iter().any(|s| s.devid == devid);
        let starts = volume.with_chunks(|map| -> Result<KVec<u64>> {
            let mut starts = mem::vec();
            for c in map.chunks().iter().filter(|c| on(c)) {
                mem::push(&mut starts, c.start)?;
            }
            Ok(starts)
        })?;
        if starts.is_empty() {
            return Ok(());
        }
        for start in starts {
            relocate(inner, start)?;
        }
    }
}

/// The device item of disk `devid`, as the transaction has it.
fn dev_item(volume: &Volume, devid: u64) -> Result<KVec<u8>> {
    let key = Key::new(DEV_ITEMS, item::DEV_ITEM, devid);
    let data = crate::tree::find(volume, &volume.chunk_root(), &key)?.ok_or(Error::Corrupt)?;
    if data.len() < DEV_ITEM_SIZE {
        return Err(Error::Corrupt);
    }
    Ok(data)
}

/// Moves what block group `start` holds to other groups, then takes its
/// chunk out. The group is closed first, so nothing new goes in it, and
/// opened again if the move fails.
fn relocate(inner: &Inner, start: u64) -> Result<()> {
    let volume = &*inner.volume;
    // The commit first: what the running transaction put in the group is
    // then the disk's, and moves as the rest does.
    inner.alone(false, |txn| txn.writer.space.close(start, true))?;
    let mut moved = Ok(());
    loop {
        match inner.alone(false, |txn| move_batch(txn, volume, start)) {
            Ok(true) => break,
            Ok(false) => {}
            Err(e) => {
                moved = Err(e);
                break;
            }
        }
    }
    match moved.and_then(|()| inner.alone(false, |txn| take_out(txn, volume, start))) {
        Ok(()) => Ok(()),
        Err(e) => {
            if let Some(mut txn) = inner.reading() {
                let _ = txn.writer.space.close(start, false);
            }
            Err(e)
        }
    }
}

/// What a block group holds: a tree block, at a level, or a data extent,
/// of a length.
#[derive(Clone, Copy)]
enum Extent {
    Block(u64, u8),
    Data(u64, u64),
}

/// Moves a batch of the extents block group `start` holds; returns
/// whether it held none.
#[inline(never)]
fn move_batch(txn: &mut Txn, volume: &Volume, start: u64) -> Result<bool> {
    let end = start + group_len(txn, start)?;
    let mut batch = mem::vec();
    {
        let root = Writer::tree_root(volume, tree::EXTENT)?;
        let mut cursor = Cursor::seek(volume, &root, &Key::new(start, 0, 0))?;
        let mut bytes = 0;
        while let Some((key, data)) = cursor.get() {
            if key.objectid >= end || bytes >= BATCH_BYTES {
                break;
            }
            match key.kind {
                item::METADATA_ITEM => {
                    let level = u8::try_from(key.offset).map_err(|_| Error::Corrupt)?;
                    mem::push(&mut batch, Extent::Block(key.objectid, level))?;
                    bytes += u64::from(volume.superblock().nodesize);
                }
                item::EXTENT_ITEM
                    if data.len() >= 24 && u64_at(data, 16) & EXTENT_FLAG_DATA != 0 =>
                {
                    mem::push(&mut batch, Extent::Data(key.objectid, key.offset))?;
                    bytes += key.offset;
                }
                // A tree block's item that is not skinny is refused
                // before a write starts.
                item::EXTENT_ITEM => return Err(Error::Corrupt),
                _ => {}
            }
            cursor.advance()?;
        }
    }
    if batch.is_empty() {
        return Ok(true);
    }
    for extent in batch {
        // Room for the extent's first change, or none starts: a change
        // that runs out of room half made would leave the filesystem
        // read-only.
        txn.writer.make_room(volume, false)?;
        match extent {
            Extent::Block(bytenr, level) => move_block(&mut txn.writer, volume, bytenr, level)?,
            Extent::Data(bytenr, len) => move_data(txn, volume, bytenr, len)?,
        }
    }
    Ok(false)
}

/// The length of block group `start`.
fn group_len(txn: &Txn, start: u64) -> Result<u64> {
    let groups = txn.writer.space.groups()?;
    let group = groups.iter().find(|g| g.0 == start).ok_or(Error::Corrupt)?;
    Ok(group.1)
}

/// Takes out the chunk of block group `start`, which holds nothing now,
/// once the commits have settled what it freed.
fn take_out(txn: &mut Txn, volume: &Volume, start: u64) -> Result<()> {
    if !txn.writer.space.settled(start)? {
        return Err(Error::Corrupt);
    }
    let len = group_len(txn, start)?;
    let key = Key::new(start, item::BLOCK_GROUP_ITEM, len);
    let root = Writer::tree_root(volume, txn.writer.space.group_tree())?;
    let group = crate::tree::find(volume, &root, &key)?.ok_or(Error::Corrupt)?;
    if group.len() < 8 || u64_at(&group, 0) != 0 {
        return Err(Error::Corrupt);
    }
    txn.writer.remove_chunk(volume, start)
}

/// The trees that reach the tree block at `bytenr`, at `level`: those
/// that refer to it, and those that reach the blocks that do.
#[inline(never)]
fn trees_reaching(volume: &Volume, bytenr: u64, level: u8) -> Result<KVec<u64>> {
    let mut trees = mem::vec();
    let mut todo = mem::vec();
    mem::push(&mut todo, (bytenr, level))?;
    let mut seen = 0;
    while let Some((bytenr, level)) = todo.pop() {
        seen += 1;
        if level >= MAX_LEVEL || seen > MAX_ROUNDS {
            return Err(Error::Corrupt);
        }
        let key = Writer::block_key(bytenr, level);
        let Some(data) = Writer::extent_item(volume, &key)? else { continue };
        for (r, _) in backrefs(volume, &key, &data)? {
            match r {
                Backref::Tree(id) if !trees.contains(&id) => mem::push(&mut trees, id)?,
                Backref::Tree(_) => {}
                Backref::Shared(parent) => mem::push(&mut todo, (parent, level + 1))?,
                Backref::Data { .. } | Backref::SharedData(_) => return Err(Error::Corrupt),
            }
        }
    }
    Ok(trees)
}

/// Moves the tree block at `bytenr`, at `level`: each tree that reaches
/// it copies it, until the last copy frees it.
#[inline(never)]
fn move_block(w: &mut Writer, volume: &Volume, bytenr: u64, level: u8) -> Result<()> {
    let key = Writer::block_key(bytenr, level);
    let mut first = None;
    for _ in 0..MAX_ROUNDS {
        if w.freed(bytenr) {
            return Ok(());
        }
        let Some(data) = Writer::extent_item(volume, &key)? else { return Ok(()) };
        // The block's first key leads each tree's walk down to it. Its
        // item's generation is its header's, as Linux's scrub takes it.
        let first = match first {
            Some(key) => key,
            None => {
                let root = Root { bytenr, level, generation: u64_at(&data, 8) };
                let block = read_block(volume, &root, &Bounds::default())?;
                let key = if block.nritems() > 0 { block.key(0) } else { Key::new(0, 0, 0) };
                *first.insert(key)
            }
        };
        let mut copied = false;
        for tree in trees_reaching(volume, bytenr, level)? {
            w.make_room(volume, false)?;
            if tree == tree::CHUNK {
                w.make_system_room(volume)?;
            }
            if w.copy_block(volume, tree, &first, level, bytenr)? {
                copied = true;
                break;
            }
        }
        // Something refers to it that no tree reaches.
        if !copied {
            return Err(Error::Corrupt);
        }
    }
    Err(Error::Corrupt)
}

/// Moves the data extent of `len` bytes at `bytenr`: copies it, and its
/// checksums, to a new extent, then points each file extent item that
/// names it at the copy, until nothing refers to it and it is freed.
#[inline(never)]
fn move_data(txn: &mut Txn, volume: &Volume, bytenr: u64, len: u64) -> Result<()> {
    let key = Key::new(bytenr, item::EXTENT_ITEM, len);
    if Writer::extent_item(volume, &key)?.is_none() {
        return Ok(());
    }
    let sums = sums_of(volume, bytenr, len)?;
    let new = txn.writer.alloc_data(volume, len)?;
    if let Err(e) = copy_data(volume, bytenr, new, len, &sums) {
        txn.writer.free_data(new, len, true)?;
        return Err(e);
    }
    let mut made = false;
    for _ in 0..MAX_ROUNDS {
        let Some(data) = Writer::extent_item(volume, &key)? else { return Ok(()) };
        let refs = backrefs(volume, &key, &data)?;
        let Some(&(r, count)) = refs.first() else { return Err(Error::Corrupt) };
        let items = match r {
            Backref::Data { root, ino, offset } => {
                file_items(volume, root, ino, offset, bytenr, count)?
            }
            Backref::SharedData(leaf) => leaf_items(volume, leaf, bytenr)?,
            Backref::Tree(_) | Backref::Shared(_) => return Err(Error::Corrupt),
        };
        if items.is_empty() {
            return Err(Error::Corrupt);
        }
        for (tree, item) in items {
            // The first change has the room its extent took; the copy is
            // the disk's once it has a reference.
            if made {
                txn.writer.make_room(volume, false)?;
            }
            switch(txn, volume, tree, &item, (bytenr, len), new, &sums, &mut made)?;
        }
    }
    Err(Error::Corrupt)
}

/// The file extent items of file `ino` of tree `root` that name the data
/// extent at `bytenr` with `offset` their key less their offset into it:
/// up to `count`, as the reference counts them.
fn file_items(
    volume: &Volume,
    root: u64,
    ino: u64,
    offset: u64,
    bytenr: u64,
    count: u32,
) -> Result<KVec<(u64, Key)>> {
    let mut items = mem::vec();
    let top = Writer::tree_root(volume, root)?;
    // An item's key is at least the offset its reference names.
    let mut cursor = Cursor::seek(volume, &top, &Key::new(ino, item::EXTENT_DATA, offset))?;
    while let Some((key, data)) = cursor.get() {
        if !key.is(ino, item::EXTENT_DATA) || items.len() >= count as usize {
            break;
        }
        if let Some((at, _, into)) = disk_extent(data)?
            && at == bytenr
            && key.offset.wrapping_sub(into) == offset
        {
            mem::push(&mut items, (root, key))?;
        }
        cursor.advance()?;
    }
    Ok(items)
}

/// The file extent items of the leaf at `leaf` that name the data extent
/// at `bytenr`, with the first tree that reaches the leaf, which is to
/// change them.
fn leaf_items(volume: &Volume, leaf: u64, bytenr: u64) -> Result<KVec<(u64, Key)>> {
    let mut items = mem::vec();
    let Some(&tree) = trees_reaching(volume, leaf, 0)?.first() else { return Ok(items) };
    let data = Writer::extent_item(volume, &Writer::block_key(leaf, 0))?.ok_or(Error::Corrupt)?;
    let root = Root { bytenr: leaf, level: 0, generation: u64_at(&data, 8) };
    let block = read_block(volume, &root, &Bounds::default())?;
    for slot in 0..block.nritems() {
        let key = block.key(slot);
        if key.kind == item::EXTENT_DATA
            && disk_extent(block.item(slot))?.is_some_and(|(at, _, _)| at == bytenr)
        {
            mem::push(&mut items, (tree, key))?;
        }
    }
    Ok(items)
}

/// Points the file extent item `key` of tree `tree` at the copy `new` of
/// the data extent `(bytenr, len)`, and moves its reference there: the
/// first makes the copy's item, and its checksums `sums`, which `made`
/// then notes. The extent goes with its last reference.
#[allow(clippy::too_many_arguments)]
fn switch(
    txn: &mut Txn,
    volume: &Volume,
    tree: u64,
    key: &Key,
    (bytenr, len): (u64, u64),
    new: u64,
    sums: &[u8],
    made: &mut bool,
) -> Result<()> {
    let mut files = txn.files(volume, tree);
    let data = files.get(tree, key)?.ok_or(Error::Corrupt)?;
    let Some((at, _, into)) = disk_extent(&data)? else { return Err(Error::Corrupt) };
    if at != bytenr {
        return Err(Error::Corrupt);
    }
    files.w.update(volume, tree, key, |d| put_u64(d, DISK_BYTENR_AT, new))?;
    let offset = key.offset.wrapping_sub(into);
    let r = Backref::Data { root: tree, ino: key.objectid, offset };
    if *made {
        files.w.add_ref(volume, &Key::new(new, item::EXTENT_ITEM, len), r, 1)?;
    } else {
        // The copy keeps the owner simple quotas count the extent to.
        let old = Writer::extent_item(volume, &Key::new(bytenr, item::EXTENT_ITEM, len))?
            .ok_or(Error::Corrupt)?;
        let owner = crate::quota::owner_of(&old).filter(|_| crate::quota::simple(volume));
        files.new_owned_extent(new, len, key.objectid, offset, owner)?;
        files.add_sums(new, sums)?;
        *made = true;
    }
    files.drop_data(&Key::new(bytenr, item::EXTENT_ITEM, len), r)
}

/// The checksums of the data extent of `len` bytes at `bytenr`, a sector
/// each; none for one with none (`nodatasum`).
#[inline(never)]
fn sums_of(volume: &Volume, bytenr: u64, len: u64) -> Result<KVec<u8>> {
    let mut sums = mem::vec();
    let Some(csums) = volume.tree(tree::CSUM)? else { return Ok(sums) };
    let size = volume.superblock().csum_type.size();
    let sectors = usize::try_from(len / PAGE).map_err(|_| Error::Corrupt)?;
    sums = crate::zeroed(sectors * size)?;
    let mut have = 0;
    extent::each_sum(volume, &csums, bytenr, sectors, &mut |sector, sum| {
        if let Some(sum) = sum {
            sums[sector * size..(sector + 1) * size].copy_from_slice(sum);
            have += 1;
        }
    })?;
    match have {
        0 => Ok(mem::vec()),
        n if n == sectors => Ok(sums),
        // Linux sums all of an extent, or none of it.
        _ => Err(Error::Corrupt),
    }
}

/// Copies the data extent of `len` bytes at `from` to `to`, a run of
/// sectors at a time, each sector from a copy that reads and matches its
/// checksum in `sums`, or, with none, from a disk that missed no commit
/// if one reads. Fails with [`Error::Io`] when no copy of a sector does.
#[inline(never)]
fn copy_data(volume: &Volume, from: u64, to: u64, len: u64, sums: &[u8]) -> Result<()> {
    let csum = volume.superblock().csum_type;
    let size = csum.size();
    let mut pages: KVec<CachePage> = mem::vec();
    let mut at = 0;
    while at < len {
        let place = volume.place(from + at)?;
        let n = ((len - at).min(place.len) / PAGE).min(SECTORS as u64) as usize;
        if n == 0 {
            return Err(Error::Corrupt);
        }
        let copies = place.copies();
        while pages.len() < SECTORS * copies.len() {
            mem::push(&mut pages, CachePage::new()?)?;
        }
        let failed = read_copies(volume, copies, &mut pages, n);
        let first = usize::try_from(at / PAGE).map_err(|_| Error::Corrupt)?;
        let mut chosen = [0usize; SECTORS];
        for (sector, pick) in chosen.iter_mut().enumerate().take(n) {
            let read = |copy: usize| failed[copy] & (1 << sector) == 0;
            let page = |copy: usize| &pages[copy * SECTORS + sector];
            let good = if sums.is_empty() {
                let behind = |copy: usize| volume.disks()[copies[copy].disk].behind();
                (0..copies.len())
                    .find(|&c| read(c) && !behind(c))
                    .or_else(|| (0..copies.len()).find(|&c| read(c)))
            } else {
                let sum = &sums[(first + sector) * size..(first + sector + 1) * size];
                (0..copies.len()).find(|&c| read(c) && csum.matches(page(c).bytes(), sum))
            };
            *pick = copy_of(good)? * SECTORS + sector;
        }
        let run: [&Page; SECTORS] = core::array::from_fn(|i| &*pages[chosen[i.min(n - 1)]]);
        write_pages(volume, to + at, &run[..n])?;
        at += n as u64 * PAGE;
    }
    Ok(())
}

/// The copy found good, or [`Error::Io`].
fn copy_of(good: Option<usize>) -> Result<usize> {
    good.ok_or(Error::Io)
}

/// Reads `n` sectors from each of `copies` into its run of `pages`;
/// returns each copy's sectors that failed to read, a bit each.
fn read_copies(
    volume: &Volume,
    copies: &[Site],
    pages: &mut [CachePage],
    n: usize,
) -> [u32; MAX_COPIES] {
    let mut failed = [0u32; MAX_COPIES];
    for (copy, &site) in copies.iter().enumerate() {
        let run = &mut pages[copy * SECTORS..copy * SECTORS + n];
        if read_pages(volume, site, run).is_err() {
            // Each sector alone, to find which fail.
            for (sector, page) in run.iter_mut().enumerate() {
                let spot = Site { at: site.at + sector as u64 * PAGE, ..site };
                if read_pages(volume, spot, core::slice::from_mut(page)).is_err() {
                    failed[copy] |= 1 << sector;
                }
            }
        }
    }
    failed
}
