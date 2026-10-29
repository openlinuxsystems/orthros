// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Committing a transaction, in the order ADR 0129 and its model
//! (`tla/Commit.tla`) check: the extent tree, the free-space tree and the
//! block group items catch up with the blocks made and freed; every block
//! the transaction made is written; the disk's cache is flushed; the
//! superblock that leads to the new trees is written through to the
//! disk; and only then may the places the transaction freed be taken.

use radix_block::cache::BlockCache;
use radix_vfs::mem::{self, KVec};
use radix_vfs::{Error, Result};

use crate::cow::{Change, Writer};
use crate::format::{Key, item, tree, u64_at};
use crate::refs::Backref;
use crate::space::{self, Ranges};
use crate::superblock;
use crate::tree::Cursor;
use crate::volume::{DEV_ITEM_AT, DEV_ITEM_SIZE, DEV_ITEMS, Disk, Volume};

/// The extent item flag of a tree block.
const EXTENT_FLAG_TREE_BLOCK: u64 = 1 << 1;

/// The size of a tree block's skinny extent item with its one inline
/// reference: refs, generation and flags, then the reference's type and
/// its tree.
const TREE_BLOCK_ITEM: usize = 24 + 9;

/// Where the superblock keeps its fields that a commit changes.
mod at {
    pub const BYTENR: usize = 0x30;
    pub const GENERATION: usize = 0x48;
    pub const ROOT: usize = 0x50;
    pub const CHUNK_ROOT: usize = 0x58;
    pub const LOG_ROOT: usize = 0x60;
    pub const BYTES_USED: usize = 0x78;
    pub const CHUNK_ROOT_GENERATION: usize = 0xa4;
    pub const ROOT_LEVEL: usize = 0xc6;
    pub const CHUNK_ROOT_LEVEL: usize = 0xc7;
    pub const LOG_ROOT_LEVEL: usize = 0xc8;
    pub const INCOMPAT: usize = 0xbc;
    pub const TOTAL_BYTES: usize = 0x70;
    pub const NUM_DEVICES: usize = 0x88;
    /// The four backup roots, after the system chunk array.
    pub const BACKUPS: usize = 0xb2b;
    pub const BACKUP_SIZE: usize = 168;
}

fn put_u64(bytes: &mut [u8], at: usize, value: u64) {
    bytes[at..at + 8].copy_from_slice(&value.to_le_bytes());
}

fn add_i64(value: u64, delta: i64) -> Result<u64> {
    value.checked_add_signed(delta).ok_or(Error::Corrupt)
}

impl Writer {
    /// Commits the running transaction, and returns once it is durable;
    /// the next transaction starts. Does nothing if nothing changed.
    pub fn commit(&mut self, volume: &Volume) -> Result<()> {
        self.check_whole()?;
        if !self.is_dirty() {
            return Ok(());
        }
        let result = self.commit_inner(volume);
        if result.is_err() {
            // What is in memory is half committed; the disk keeps the last
            // commit.
            self.break_off();
        }
        result
    }

    fn commit_inner(&mut self, volume: &Volume) -> Result<()> {
        self.write_quotas(volume)?;
        self.catch_up(volume)?;
        let sb = self.write_blocks(volume)?;
        Self::write_superblocks(volume, &sb, true)?;
        volume.set_superblock_bytes(sb);
        for (start, len) in core::mem::replace(&mut self.pinned, mem::vec()) {
            self.space.unpin(start, len)?;
        }
        // The superblock names no log now.
        let nodesize = u64::from(volume.superblock().nodesize);
        self.logging.committed(&mut self.space, nodesize)?;
        self.grown_committed();
        self.generation += 1;
        self.bytes_used = 0;
        Ok(())
    }

    /// Brings the extent tree, the block group items, the free-space tree
    /// and the root items up to date. Each change made to them changes
    /// blocks too, so this goes on until a round changes nothing.
    fn catch_up(&mut self, volume: &Volume) -> Result<()> {
        loop {
            // A chunk's items come before its group's used bytes and free
            // space are written.
            let mut busy = self.record_chunks(volume)?;
            let changes = core::mem::replace(&mut self.changes, mem::vec());
            busy |= !changes.is_empty();
            for change in changes {
                self.apply(volume, change)?;
            }
            for start in self.space.changed()? {
                busy = true;
                if self.unrecorded(start) {
                    continue;
                }
                let (len, log, delta) = self.space.take_changes(start)?;
                if delta != 0 {
                    self.add_used(volume, start, len, delta)?;
                }
                if !log.is_empty() {
                    self.sync_free_space(volume, start, len, &log)?;
                }
            }
            let used = core::mem::replace(&mut self.tree_used, mem::vec());
            for (id, delta) in used {
                if delta == 0 || id == tree::ROOT || id == tree::CHUNK {
                    continue;
                }
                busy = true;
                self.add_root_used(volume, id, delta)?;
            }
            if !busy {
                return Ok(());
            }
        }
    }

    /// Records a block made or freed in the extent tree.
    fn apply(&mut self, volume: &Volume, change: Change) -> Result<()> {
        match change {
            Change::Made { bytenr, level, owner } => {
                let key = Key::new(bytenr, item::METADATA_ITEM, level.into());
                let generation = self.generation;
                self.insert_with(volume, tree::EXTENT, &key, TREE_BLOCK_ITEM, |data| {
                    data[0..8].copy_from_slice(&1u64.to_le_bytes());
                    data[8..16].copy_from_slice(&generation.to_le_bytes());
                    data[16..24].copy_from_slice(&EXTENT_FLAG_TREE_BLOCK.to_le_bytes());
                    data[24] = item::TREE_BLOCK_REF;
                    data[25..33].copy_from_slice(&owner.to_le_bytes());
                })
            }
            Change::Freed { bytenr, level, owner } => {
                let key = Key::new(bytenr, item::METADATA_ITEM, level.into());
                let root = Self::tree_root(volume, tree::EXTENT)?;
                let alone = match crate::tree::find(volume, &root, &key)? {
                    Some(data) => {
                        data.len() == TREE_BLOCK_ITEM
                            && u64_at(&data, 0) == 1
                            && data[24] == item::TREE_BLOCK_REF
                            && u64_at(&data, 25) == owner
                    }
                    None => return Err(Error::Corrupt),
                };
                // The tree that freed it had its last reference, which
                // may be an item of its own.
                if !alone && self.drop_ref(volume, &key, Backref::Tree(owner))? != 0 {
                    return Err(Error::Corrupt);
                }
                self.delete(volume, tree::EXTENT, &key)
            }
        }
    }

    /// Adds `delta` to the bytes block group `start` has allocated.
    fn add_used(&mut self, volume: &Volume, start: u64, len: u64, delta: i64) -> Result<()> {
        let key = Key::new(start, item::BLOCK_GROUP_ITEM, len);
        let group_tree = self.space.group_tree();
        let mut bad = false;
        self.update(volume, group_tree, &key, |data| match add_i64(u64_at(data, 0), delta) {
            Ok(used) if used <= len => data[0..8].copy_from_slice(&used.to_le_bytes()),
            _ => bad = true,
        })?;
        if bad { Err(Error::Corrupt) } else { Ok(()) }
    }

    /// Adds `delta` to the bytes of blocks tree `id`'s root item counts.
    fn add_root_used(&mut self, volume: &Volume, id: u64, delta: i64) -> Result<()> {
        let cursor = Cursor::seek(volume, &volume.root_tree(), &Key::first(id, item::ROOT_ITEM))?;
        let key = match cursor.key() {
            Some(key) if key.is(id, item::ROOT_ITEM) => key,
            _ => return Err(Error::Corrupt),
        };
        drop(cursor);
        self.update(volume, tree::ROOT, &key, |data| {
            // bytes_used, after generation, root_dirid, bytenr and
            // byte_limit.
            let used = u64_at(data, 192).saturating_add_signed(delta);
            data[192..200].copy_from_slice(&used.to_le_bytes());
        })
    }

    /// Makes the free-space tree's ranges for block group `[start, start
    /// + len)` say what `log`, its changes in order, made of them.
    fn sync_free_space(
        &mut self,
        volume: &Volume,
        start: u64,
        len: u64,
        log: &[(u64, u64, bool)],
    ) -> Result<()> {
        let free_tree = Self::tree_root(volume, tree::FREE_SPACE)?;
        let (mut free, bitmaps) = space::read_free(volume, &free_tree, start, len)?;
        for &(s, e, freed) in log {
            if freed {
                space::add(&mut free, s, e)?;
            } else {
                space::remove(&mut free, s, e)?;
            }
        }
        if bitmaps {
            self.write_bitmaps(volume, start, len, &free)?;
        } else {
            self.write_extents(volume, start, len, &free)?;
        }
        let info = Key::new(start, item::FREE_SPACE_INFO, len);
        let count = free.len() as u32;
        self.update(volume, tree::FREE_SPACE, &info, |data| {
            data[0..4].copy_from_slice(&count.to_le_bytes());
        })
    }

    /// The free-space tree's extents for the group, as it keeps them.
    fn extents_of(volume: &Volume, start: u64, len: u64) -> Result<Ranges> {
        let root = Self::tree_root(volume, tree::FREE_SPACE)?;
        let first = Key::new(start, item::FREE_SPACE_INFO, len);
        let mut cursor = Cursor::seek(volume, &root, &first)?;
        let mut out = mem::vec();
        cursor.advance()?;
        while let Some(key) = cursor.key() {
            if key.objectid >= start + len || key.kind != item::FREE_SPACE_EXTENT {
                break;
            }
            mem::push(&mut out, (key.objectid, key.objectid + key.offset))?;
            cursor.advance()?;
        }
        Ok(out)
    }

    /// Replaces the group's extent items with `free`.
    fn write_extents(
        &mut self,
        volume: &Volume,
        start: u64,
        len: u64,
        free: &Ranges,
    ) -> Result<()> {
        let old = Self::extents_of(volume, start, len)?;
        for &(s, e) in &old {
            if free.binary_search(&(s, e)).is_err() {
                self.delete(
                    volume,
                    tree::FREE_SPACE,
                    &Key::new(s, item::FREE_SPACE_EXTENT, e - s),
                )?;
            }
        }
        for &(s, e) in free {
            if old.binary_search(&(s, e)).is_err() {
                self.insert(
                    volume,
                    tree::FREE_SPACE,
                    &Key::new(s, item::FREE_SPACE_EXTENT, e - s),
                    &[],
                )?;
            }
        }
        Ok(())
    }

    /// Sets the bits of the group's bitmap items from `free`.
    fn write_bitmaps(
        &mut self,
        volume: &Volume,
        start: u64,
        len: u64,
        free: &Ranges,
    ) -> Result<()> {
        let sector = u64::from(volume.superblock().sectorsize);
        let root = Self::tree_root(volume, tree::FREE_SPACE)?;
        let mut bitmaps: KVec<Key> = mem::vec();
        {
            let mut cursor =
                Cursor::seek(volume, &root, &Key::new(start, item::FREE_SPACE_INFO, len))?;
            cursor.advance()?;
            while let Some(key) = cursor.key() {
                if key.objectid >= start + len || key.kind != item::FREE_SPACE_BITMAP {
                    break;
                }
                mem::push(&mut bitmaps, key)?;
                cursor.advance()?;
            }
        }
        for key in bitmaps {
            self.update(volume, tree::FREE_SPACE, &key, |data| {
                data.fill(0);
                let end = key.objectid + key.offset;
                for &(s, e) in free.iter().filter(|r| r.1 > key.objectid && r.0 < end) {
                    let mut at = s.max(key.objectid);
                    while at < e.min(end) {
                        let bit = (at - key.objectid) / sector;
                        if let Some(byte) = data.get_mut((bit / 8) as usize) {
                            *byte |= 1 << (bit % 8);
                        }
                        at += sector;
                    }
                }
            })?;
        }
        Ok(())
    }

    /// Seals and writes every block the transaction made, to each of its
    /// copies, and returns the superblock that leads to them.
    fn write_blocks(&mut self, volume: &Volume) -> Result<KVec<u8>> {
        let csum = volume.superblock().csum_type;
        let blocks = volume.blocks().take_all_dirty();
        for (bytenr, mut block) in blocks {
            let b = mem::KArc::get_mut(&mut block).ok_or(Error::InUse)?;
            b.seal(csum);
            for site in volume.place(bytenr)?.copies() {
                let cache = volume.disk(site.disk)?.cache().ok_or(Error::NoDevice)?;
                cache.write(site.at, b.bytes())?;
            }
            volume.blocks().put_clean(&block);
        }
        let mut sb = volume.superblock_bytes()?;
        let root = volume.root_tree();
        let chunk = volume.chunk_root();
        put_u64(&mut sb, at::GENERATION, self.generation);
        put_u64(&mut sb, at::ROOT, root.bytenr);
        sb[at::ROOT_LEVEL] = root.level;
        put_u64(&mut sb, at::CHUNK_ROOT, chunk.bytenr);
        sb[at::CHUNK_ROOT_LEVEL] = chunk.level;
        put_u64(&mut sb, at::CHUNK_ROOT_GENERATION, chunk.generation);
        // The log of the transaction's fsyncs, or one a mount replayed,
        // is gone.
        put_u64(&mut sb, at::LOG_ROOT, 0);
        sb[at::LOG_ROOT_LEVEL] = 0;
        let used = add_i64(u64_at(&sb, at::BYTES_USED), self.bytes_used)?;
        put_u64(&mut sb, at::BYTES_USED, used);
        let features = (u64_at(&sb, at::INCOMPAT) | self.incompat) & !self.incompat_off;
        put_u64(&mut sb, at::INCOMPAT, features);
        self.grow_superblock(&mut sb)?;
        Self::write_backup(volume, &mut sb)?;
        let sum = csum.sum(&sb[crate::csum::CSUM_SIZE..]);
        sb[..crate::csum::CSUM_SIZE].copy_from_slice(&sum);
        Ok(sb)
    }

    /// Fills the next of the superblock's four backup roots, as Linux
    /// does: the one after the newest.
    fn write_backup(volume: &Volume, sb: &mut [u8]) -> Result<()> {
        let gen_of = |i: usize| u64_at(sb, at::BACKUPS + i * at::BACKUP_SIZE + 8);
        let newest = (0..4).max_by_key(|&i| gen_of(i)).unwrap_or(0);
        let slot = (newest + 1) % 4;
        let b = at::BACKUPS + slot * at::BACKUP_SIZE;
        let total = u64_at(sb, at::TOTAL_BYTES);
        let used = u64_at(sb, at::BYTES_USED);
        let devices = u64_at(sb, at::NUM_DEVICES);
        sb[b..b + at::BACKUP_SIZE].fill(0);
        let root = volume.root_tree();
        let chunk = volume.chunk_root();
        let mut put_root = |at: usize, level_at: usize, r: crate::tree::Root| {
            put_u64(sb, b + at, r.bytenr);
            put_u64(sb, b + at + 8, r.generation);
            sb[b + level_at] = r.level;
        };
        put_root(0, 152, root);
        put_root(16, 153, chunk);
        let trees = [
            (32, 154, tree::EXTENT),
            (48, 155, tree::FS),
            (64, 156, tree::DEV),
            (80, 157, tree::CSUM),
        ];
        for (at, level_at, id) in trees {
            if let Some(r) = volume.tree(id)? {
                put_root(at, level_at, r);
            }
        }
        put_u64(sb, b + 96, total);
        put_u64(sb, b + 104, used);
        put_u64(sb, b + 112, devices);
        Ok(())
    }

    /// Makes the blocks written durable on every disk, then writes each
    /// disk its superblock `sb`, with the disk's own device item from the
    /// chunk tree. A disk that fails is left out, while the others still
    /// hold every chunk ([`Volume::write_failed`]). Without `mirrors`, each
    /// disk gets its superblock alone, as Linux's log of fsyncs writes it.
    #[inline(never)]
    pub(crate) fn write_superblocks(volume: &Volume, sb: &[u8], mirrors: bool) -> Result<()> {
        for (i, disk) in volume.disks().iter().enumerate() {
            if let Some(Err(e)) = disk.cache().map(BlockCache::sync) {
                volume.write_failed(i, e.into())?;
            }
        }
        let csum = volume.superblock().csum_type;
        let chunk_root = volume.chunk_root();
        for (i, disk) in volume.disks().iter().enumerate() {
            // A disk behind keeps the superblock it came back with, which
            // marks it behind at the next mount too, until a scrub has
            // caught it up (ADR 0153).
            if !disk.present() || disk.behind() {
                continue;
            }
            let mut copy = mem::copy(sb)?;
            let key = Key::new(DEV_ITEMS, item::DEV_ITEM, disk.devid);
            let cursor = Cursor::seek(volume, &chunk_root, &key)?;
            match cursor.get() {
                Some((found, data)) if found == key && data.len() >= DEV_ITEM_SIZE => {
                    copy[DEV_ITEM_AT..DEV_ITEM_AT + DEV_ITEM_SIZE]
                        .copy_from_slice(&data[..DEV_ITEM_SIZE]);
                }
                _ => return Err(Error::Corrupt),
            }
            let sum = csum.sum(&copy[crate::csum::CSUM_SIZE..]);
            copy[..crate::csum::CSUM_SIZE].copy_from_slice(&sum);
            if let Err(e) = Self::write_supers(volume, disk, &copy, mirrors) {
                volume.write_failed(i, e)?;
            }
        }
        Ok(())
    }

    /// Writes the superblock `sb` to `disk`, and its copies at 64 MiB and
    /// 256 GiB that fit on it, each with its own address and checksum,
    /// after the blocks they lead to are durable, as Linux writes them:
    /// all at once, the superblock durable when this returns and its
    /// copies at the next flush. Without `mirrors`, the superblock alone.
    fn write_supers(volume: &Volume, disk: &Disk, sb: &[u8], mirrors: bool) -> Result<()> {
        let csum = volume.superblock().csum_type;
        let cache = disk.cache().ok_or(Error::NoDevice)?;
        let mut copies = [None, None];
        for (copy, mirror) in copies.iter_mut().zip([64u64 << 20, 256 << 30]) {
            if !mirrors || !disk.holds(mirror, superblock::SIZE as u64) {
                break;
            }
            let mut bytes = mem::copy(sb)?;
            put_u64(&mut bytes, at::BYTENR, mirror);
            let sum = csum.sum(&bytes[crate::csum::CSUM_SIZE..]);
            bytes[..crate::csum::CSUM_SIZE].copy_from_slice(&sum);
            *copy = Some((mirror, bytes));
        }
        let mut pieces = [(superblock::OFFSET, sb, true); 3];
        let mut n = 1;
        for (mirror, bytes) in copies.iter().flatten() {
            pieces[n] = (*mirror, bytes, false);
            n += 1;
        }
        Ok(cache.write_now_all(&pieces[..n])?)
    }
}
