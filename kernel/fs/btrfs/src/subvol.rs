// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Subvolumes and snapshots (ADR 0138): a new, empty subvolume, made as
//! Linux's `create_subvol` makes one; a snapshot, as its
//! `create_pending_snapshot` makes one; and removing a subvolume with
//! everything it holds, as its `btrfs_delete_subvolume` and then its
//! cleaner's `btrfs_drop_snapshot` do, here in one transaction.
//!
//! A snapshot is a copy of its source's root block, and every block below
//! it is shared: the first of the two trees to change one copies it
//! ([`crate::cow`]). Dropping a tree walks down it, taking its reference
//! from each block others share and freeing each one only it refers to,
//! with its data. A block the tree owns that others share refers to what
//! is below it by the tree, which is going: first it comes to refer by
//! its address, and so does every block below it the tree owns.

use radix_vfs::mem::KArc;
use radix_vfs::{Error, Result};

use crate::cow::{Via, Writer, below};
use crate::dir::Entries;
use crate::files::{Files, at, put_time, put_u16, put_u32, put_u64};
use crate::format::{Key, RootItem, item, tree, u64_at};
use crate::refs::{Backref, FULL_BACKREF, refs_flags};
use crate::tree::{Block, Bounds, MAX_LEVEL, Root, read_block};

/// Where a root item keeps its fields, after the inode item it starts
/// with.
mod root_at {
    pub const GENERATION: usize = 160;
    pub const ROOT_DIRID: usize = 168;
    pub const BYTENR: usize = 176;
    pub const LAST_SNAPSHOT: usize = 200;
    pub const FLAGS: usize = 208;
    pub const REFS: usize = 216;
    /// The key a drop that stopped goes on from, and its level.
    pub const DROP_PROGRESS: usize = 220;
    pub const LEVEL: usize = 238;
    pub const GENERATION_V2: usize = 239;
    pub const UUID: usize = 247;
    pub const PARENT_UUID: usize = 263;
    pub const RECEIVED_UUID: usize = 279;
    pub const CTRANSID: usize = 295;
    pub const OTRANSID: usize = 303;
    pub const STRANSID: usize = 311;
    pub const RTRANSID: usize = 319;
    pub const CTIME: usize = 327;
    pub const OTIME: usize = 339;
    pub const STIME: usize = 351;
    pub const RTIME: usize = 363;
    /// The whole item, as Linux has written it since 3.5.
    pub const SIZE: usize = 439;
}

/// A subvolume's top directory.
const TOP: u64 = 256;

/// The inode flags of a new subvolume's root item, Linux's
/// `BTRFS_INODE_ROOT_ITEM_INIT`.
const ROOT_ITEM_INIT: u64 = 1 << 31;

/// The UUID tree's item types: a subvolume by its UUID, and by the UUID
/// it was received as.
const UUID_SUBVOL: u8 = 251;
const UUID_RECEIVED: u8 = 252;

/// The file type byte of a directory, which a subvolume's entry has.
const FT_DIR: u8 = 2;

/// A root reference's size before its name: the directory, the entry's
/// index and the name's length.
const ROOT_REF_HEADER: usize = 18;

/// The UUID tree's key for `uuid` as `kind`.
fn uuid_key(uuid: &[u8], kind: u8) -> Key {
    Key::new(u64_at(uuid, 0), kind, u64_at(uuid, 8))
}

/// A block on the way down a tree being dropped.
struct Step {
    block: KArc<Block>,
    /// The next slot to go down.
    slot: usize,
    /// Whether what is below it refers to it by its address.
    full: bool,
    /// How the block above refers to it.
    via: Backref,
}

impl Files<'_> {
    /// Makes `name` in directory `dir` of this subvolume a new, empty
    /// subvolume; returns its number.
    pub(crate) fn make_subvolume(&mut self, dir: u64, name: &[u8], now: u64) -> Result<u64> {
        let id = Writer::next_tree(self.volume)?;
        let generation = self.w.generation;
        let like = Writer::tree_root(self.volume, self.subvol)?;
        let leaf = self.w.new_root_leaf(self.volume, id, &like)?;
        let nodesize = u64::from(self.volume.superblock().nodesize);
        let mut uuid = [0; 16];
        radix_vfs::random(&mut uuid);
        let key = Key::new(id, item::ROOT_ITEM, 0);
        // Its bytes used are the commit's to count.
        self.w.insert_with(self.volume, tree::ROOT, &key, root_at::SIZE, |d| {
            d.fill(0);
            // The inode item it starts with, as Linux fills it.
            put_u64(d, at::GENERATION, 1);
            put_u64(d, at::SIZE, 3);
            put_u32(d, at::NLINK, 1);
            put_u64(d, at::NBYTES, nodesize);
            put_u32(d, at::MODE, 0o40755);
            put_u64(d, at::FLAGS, ROOT_ITEM_INIT);
            put_u64(d, root_at::GENERATION, generation);
            put_u64(d, root_at::ROOT_DIRID, TOP);
            put_u64(d, root_at::BYTENR, leaf.bytenr);
            put_u32(d, root_at::REFS, 1);
            d[root_at::LEVEL] = leaf.level;
            put_u64(d, root_at::GENERATION_V2, generation);
            d[root_at::UUID..root_at::UUID + 16].copy_from_slice(&uuid);
            put_u64(d, root_at::CTRANSID, generation);
            put_u64(d, root_at::OTRANSID, generation);
            put_time(d, root_at::CTIME, now);
            put_time(d, root_at::OTIME, now);
        })?;
        let mut inner =
            Files { w: &mut *self.w, volume: self.volume, subvol: id, changed: [None; 2] };
        inner.make_inode(TOP, (0o40755, 0), (0, 0), 0, 0, now)?;
        // The top directory's reference to itself, "..", at index 0.
        let mut reference = [0; 12];
        put_u16(&mut reference, 8, 2);
        reference[10..].copy_from_slice(b"..");
        inner.w.insert(self.volume, id, &Key::new(TOP, item::INODE_REF, TOP), &reference)?;
        self.name_subvolume(dir, name, &key, id, now)?;
        self.add_uuid(UUID_SUBVOL, &uuid, id)?;
        Ok(id)
    }

    /// Makes `name` in directory `dir` of this subvolume a snapshot of
    /// subvolume `source`, read-only if `read_only`; returns its number.
    /// The caller committed first, so every block of `source` is on the
    /// disk, and `source`'s entry, if it is in this subvolume, is not in
    /// the snapshot.
    pub(crate) fn snapshot(
        &mut self,
        source: u64,
        dir: u64,
        name: &[u8],
        read_only: bool,
        now: u64,
    ) -> Result<u64> {
        let id = Writer::next_tree(self.volume)?;
        let generation = self.w.generation;
        let key = Writer::root_item_key(self.volume, source)?;
        let mut new = self.get(tree::ROOT, &key)?.ok_or(Error::Corrupt)?;
        if new.len() < root_at::SIZE {
            return Err(Error::NotSupported);
        }
        // Its blocks from up to now may be shared from here on.
        self.w.update(self.volume, tree::ROOT, &key, |d| {
            put_u64(d, root_at::LAST_SNAPSHOT, generation);
        })?;
        self.w.note_snapshot(source, generation)?;
        let root = Writer::tree_root(self.volume, source)?;
        let copy = self.w.copy_root(self.volume, &root, id)?;
        let mut uuid = [0; 16];
        radix_vfs::random(&mut uuid);
        let d = &mut new[..];
        d.copy_within(root_at::UUID..root_at::UUID + 16, root_at::PARENT_UUID);
        d[root_at::UUID..root_at::UUID + 16].copy_from_slice(&uuid);
        put_u64(d, root_at::GENERATION, generation);
        put_u64(d, root_at::BYTENR, copy.bytenr);
        put_u64(d, root_at::LAST_SNAPSHOT, generation);
        put_u32(d, root_at::REFS, 1);
        d[root_at::DROP_PROGRESS..root_at::LEVEL].fill(0);
        d[root_at::LEVEL] = copy.level;
        put_u64(d, root_at::GENERATION_V2, generation);
        let flags = u64_at(d, root_at::FLAGS);
        let flags =
            if read_only { flags | RootItem::READ_ONLY } else { flags & !RootItem::READ_ONLY };
        put_u64(d, root_at::FLAGS, flags);
        if !read_only {
            // A writable snapshot was received from nowhere.
            d[root_at::RECEIVED_UUID..root_at::RECEIVED_UUID + 16].fill(0);
            put_u64(d, root_at::STRANSID, 0);
            put_u64(d, root_at::RTRANSID, 0);
            d[root_at::STIME..root_at::STIME + 12].fill(0);
            d[root_at::RTIME..root_at::RTIME + 12].fill(0);
        }
        put_u64(d, root_at::OTRANSID, generation);
        put_time(d, root_at::OTIME, now);
        // A snapshot's key has the transaction that made it.
        self.w.insert(self.volume, tree::ROOT, &Key::new(id, item::ROOT_ITEM, generation), &new)?;
        self.w.note_snapshot(id, generation)?;
        self.name_subvolume(dir, name, &Key::new(id, item::ROOT_ITEM, u64::MAX), id, now)?;
        self.add_uuid(UUID_SUBVOL, &uuid, id)?;
        Ok(id)
    }

    /// Whether subvolume `id` holds other subvolumes.
    pub(crate) fn has_subvolumes(&self, id: u64) -> Result<bool> {
        Ok(self
            .next(tree::ROOT, &Key::first(id, item::ROOT_REF))?
            .is_some_and(|(key, _)| key.is(id, item::ROOT_REF)))
    }

    /// Removes subvolume `id`, named `name` in directory `dir` of this
    /// subvolume, with everything it holds. The caller committed first,
    /// so the disk has every block of it, and checked that nothing in it
    /// is in use and that it holds no subvolume.
    pub(crate) fn remove_subvolume(
        &mut self,
        dir: u64,
        name: &[u8],
        id: u64,
        now: u64,
    ) -> Result<()> {
        let ref_key = Key::new(self.subvol, item::ROOT_REF, id);
        let data = self.get(tree::ROOT, &ref_key)?.ok_or(Error::Corrupt)?;
        if data.len() < ROOT_REF_HEADER {
            return Err(Error::Corrupt);
        }
        self.cut_dir_item(dir, name)?;
        self.drop_dir_index(dir, name, u64_at(&data, 8), now)?;
        self.w.delete(self.volume, tree::ROOT, &ref_key)?;
        self.w.delete(self.volume, tree::ROOT, &Key::new(id, item::ROOT_BACKREF, self.subvol))?;
        let key = Writer::root_item_key(self.volume, id)?;
        let item = self.get(tree::ROOT, &key)?.ok_or(Error::Corrupt)?;
        if item.len() >= root_at::SIZE {
            self.remove_uuid(UUID_SUBVOL, &item[root_at::UUID..root_at::UUID + 16], id)?;
            let received = &item[root_at::RECEIVED_UUID..root_at::RECEIVED_UUID + 16];
            if received.iter().any(|&b| b != 0) {
                self.remove_uuid(UUID_RECEIVED, received, id)?;
            }
        }
        let top = Writer::tree_root(self.volume, id)?;
        self.drop_tree(id, &top, key.offset)?;
        self.w.delete(self.volume, tree::ROOT, &key)?;
        self.w.forget_tree(id);
        Ok(())
    }

    /// Takes the entry `name` out of directory `dir`, a subvolume's that
    /// the root tree has no reference for, as a snapshot's copy of its
    /// source's entry is, as Linux's `btrfs_unlink_subvol` does.
    pub(crate) fn unlink_stub(&mut self, dir: u64, name: &[u8], now: u64) -> Result<()> {
        let mut key = Key::first(dir, item::DIR_INDEX);
        let index = loop {
            let (found, data) = self.next(self.subvol, &key)?.ok_or(Error::Corrupt)?;
            if !found.is(dir, item::DIR_INDEX) {
                return Err(Error::Corrupt);
            }
            let mut named = false;
            for entry in Entries::of(&data) {
                named |= entry?.name == name;
            }
            if named {
                break found.offset;
            }
            key =
                Key::new(dir, item::DIR_INDEX, found.offset.checked_add(1).ok_or(Error::Corrupt)?);
        };
        self.cut_dir_item(dir, name)?;
        self.drop_dir_index(dir, name, index, now)
    }

    /// Names subvolume `id`, whose entries point at `location`, `name` in
    /// directory `dir`: the directory's entries, and the root tree's
    /// references between the two subvolumes, which keep the directory
    /// and the entry's index.
    fn name_subvolume(
        &mut self,
        dir: u64,
        name: &[u8],
        location: &Key,
        id: u64,
        now: u64,
    ) -> Result<()> {
        let index = self.next_index(dir)?;
        self.add_dir_entries(dir, name, location, FT_DIR, index, now)?;
        let mut data = crate::zeroed(ROOT_REF_HEADER + name.len())?;
        put_u64(&mut data, 0, dir);
        put_u64(&mut data, 8, index);
        put_u16(&mut data, 16, name.len() as u16);
        data[ROOT_REF_HEADER..].copy_from_slice(name);
        self.w.insert(
            self.volume,
            tree::ROOT,
            &Key::new(self.subvol, item::ROOT_REF, id),
            &data,
        )?;
        self.w.insert(
            self.volume,
            tree::ROOT,
            &Key::new(id, item::ROOT_BACKREF, self.subvol),
            &data,
        )
    }

    /// Notes in the UUID tree, if the filesystem has one, that subvolume
    /// `id` has `uuid`, as `kind`.
    fn add_uuid(&mut self, kind: u8, uuid: &[u8], id: u64) -> Result<()> {
        if self.volume.tree(tree::UUID)?.is_none() {
            return Ok(());
        }
        self.append(tree::UUID, &uuid_key(uuid, kind), &id.to_le_bytes())
    }

    /// Takes subvolume `id` from the UUID tree's entry for `uuid` as
    /// `kind`, if there is one, as Linux's `btrfs_uuid_tree_remove` does.
    fn remove_uuid(&mut self, kind: u8, uuid: &[u8], id: u64) -> Result<()> {
        if self.volume.tree(tree::UUID)?.is_none() {
            return Ok(());
        }
        let key = uuid_key(uuid, kind);
        let Some(data) = self.get(tree::UUID, &key)? else { return Ok(()) };
        let ids = data.as_chunks::<8>().0;
        let Some(n) = ids.iter().position(|c| u64::from_le_bytes(*c) == id) else {
            return Ok(());
        };
        self.cut(tree::UUID, &key, data.len(), n * 8, 8)
    }

    /// Drops tree `id`, whose root block is `top` and which transaction
    /// `made` made: frees every block and data extent only it refers to,
    /// and takes its references from those others share, as Linux's
    /// `btrfs_drop_snapshot` does. A walk with a path of its own, not a
    /// call for each level.
    fn drop_tree(&mut self, id: u64, top: &Root, made: u64) -> Result<()> {
        let mut path: [Option<Step>; MAX_LEVEL as usize] = core::array::from_fn(|_| None);
        let mut depth = 0;
        if let Some(step) = self.enter(id, top, Backref::Tree(id), made)? {
            path[0] = Some(step);
            depth = 1;
        }
        while depth > 0 {
            let step = path[depth - 1].as_mut().ok_or(Error::Corrupt)?;
            if step.slot < step.block.nritems() {
                let child = step.block.child(step.slot);
                step.slot += 1;
                let via = if step.full {
                    Backref::Shared(step.block.bytenr())
                } else {
                    Backref::Tree(id)
                };
                if let Some(next) = self.enter(id, &child, via, made)? {
                    *path.get_mut(depth).ok_or(Error::Corrupt)? = Some(next);
                    depth += 1;
                }
                continue;
            }
            let step = path[depth - 1].take().ok_or(Error::Corrupt)?;
            depth -= 1;
            self.free_tree_block(&step.block, step.via)?;
        }
        Ok(())
    }

    /// Comes to the block `root` of tree `id`, which the block above, or
    /// the root item, refers to by `via`. A block others share loses that
    /// reference, after what is below it that the tree owns comes to
    /// refer by address; a leaf only the tree refers to goes with its
    /// data. Returns a node only the tree refers to, to go down.
    fn enter(&mut self, id: u64, root: &Root, via: Backref, made: u64) -> Result<Option<Step>> {
        let key = Writer::block_key(root.bytenr, root.level);
        let data = Writer::extent_item(self.volume, &key)?.ok_or(Error::Corrupt)?;
        let (refs, flags) = refs_flags(&data)?;
        let full = flags & FULL_BACKREF != 0;
        if refs > 1 {
            // Only a block the tree made can be its own.
            if !full && root.generation > made {
                self.refer_by_address(id, root, made)?;
            }
            if self.w.drop_ref(self.volume, &key, via)? == 0 {
                return Err(Error::Corrupt);
            }
            return Ok(None);
        }
        let block = read_block(self.volume, root, &Bounds::default())?;
        // Only its owner refers to what is below a block by its tree.
        if !full && block.owner() != id {
            return Err(Error::Corrupt);
        }
        if !block.is_leaf() {
            return Ok(Some(Step { block, slot: 0, full, via }));
        }
        let from = if full { Via::Address(root.bytenr) } else { Via::Tree(id) };
        for slot in 0..block.nritems() {
            if let Some((key, r)) = below(&block, slot, from)? {
                self.drop_data(&key, r)?;
            }
        }
        self.free_tree_block(&block, via)?;
        Ok(None)
    }

    /// Frees `block`, of a tree being dropped, whose last reference is
    /// `via`.
    fn free_tree_block(&mut self, block: &Block, via: Backref) -> Result<()> {
        let key = Writer::block_key(block.bytenr(), block.level());
        let data = Writer::extent_item(self.volume, &key)?.ok_or(Error::Corrupt)?;
        if !via.only(&data)? && self.w.drop_ref(self.volume, &key, via)? != 0 {
            return Err(Error::Corrupt);
        }
        self.w.delete(self.volume, tree::EXTENT, &key)?;
        self.w.free_dropped(self.volume, block.bytenr(), block.owner(), block.generation())
    }

    /// Makes the block `root`, if tree `id` owns it, and every block below
    /// it the tree made after transaction `made` and owns, refer to what
    /// is below it by its address rather than by the tree, as Linux's
    /// drop does in its `UPDATE_BACKREF` stage: the trees that keep these
    /// blocks must still find what is below them once the tree is gone.
    fn refer_by_address(&mut self, id: u64, root: &Root, made: u64) -> Result<()> {
        let block = read_block(self.volume, root, &Bounds::default())?;
        if block.owner() != id {
            return Ok(());
        }
        self.make_full(id, &block)?;
        if block.is_leaf() {
            return Ok(());
        }
        let mut path: [Option<(KArc<Block>, usize)>; MAX_LEVEL as usize] =
            core::array::from_fn(|_| None);
        path[0] = Some((block, 0));
        let mut depth = 1;
        while depth > 0 {
            let (block, slot) = path[depth - 1].as_mut().ok_or(Error::Corrupt)?;
            if *slot >= block.nritems() {
                path[depth - 1] = None;
                depth -= 1;
                continue;
            }
            let child = block.child(*slot);
            *slot += 1;
            if child.generation <= made {
                continue;
            }
            let key = Writer::block_key(child.bytenr, child.level);
            let data = Writer::extent_item(self.volume, &key)?.ok_or(Error::Corrupt)?;
            let full = refs_flags(&data)?.1 & FULL_BACKREF != 0;
            if full && child.level == 0 {
                continue;
            }
            let next = read_block(self.volume, &child, &Bounds::default())?;
            if next.owner() != id {
                continue;
            }
            if !full {
                self.make_full(id, &next)?;
            }
            if !next.is_leaf() {
                *path.get_mut(depth).ok_or(Error::Corrupt)? = Some((next, 0));
                depth += 1;
            }
        }
        Ok(())
    }

    /// Makes what is below `block`, of tree `id`, refer to it by its
    /// address rather than by the tree, and marks it so.
    fn make_full(&mut self, id: u64, block: &Block) -> Result<()> {
        self.w.refer_below(self.volume, block, Via::Address(block.bytenr()), true)?;
        self.w.refer_below(self.volume, block, Via::Tree(id), false)?;
        let key = Writer::block_key(block.bytenr(), block.level());
        self.w.set_extent_flags(self.volume, &key, FULL_BACKREF)
    }
}
