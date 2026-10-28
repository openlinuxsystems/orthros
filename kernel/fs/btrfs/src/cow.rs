// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Changing Btrfs's trees by copying on write, in transactions (ADR 0129).
//!
//! A [`Writer`] holds the running transaction. To change a tree it walks
//! down from the root as a reader does, but copies each block on the way
//! that the transaction did not make, to a place it takes from the free
//! space, and points the block above at the copy; the root's copy goes
//! into the tree's root item. The blocks the transaction made change in
//! place. The places the copies leave are pinned until the commit is
//! durable ([`crate::space`]), and every block made or freed is a change
//! the extent tree gets at the commit ([`crate::commit`]).
//!
//! Inserting splits a full node on the way down, so the block above
//! always has room for a split below it, and splits the leaf if the new
//! item does not fit. Deleting frees a block left empty, and a root node
//! left with one child gives way to it. Any failure once a change has
//! begun leaves the transaction half made: the writer then refuses every
//! change and commit, so the disk keeps the last commit whole.

use radix_vfs::mem::{self, KVec};
use radix_vfs::{Error, Result};

use crate::chunk::kind;
use crate::format::{Key, item, tree};
use crate::grow::Grown;
use crate::space::Space;
use crate::superblock::{compat_ro, incompat};
use crate::tree::{Block, Bounds, Cursor, HEADER_SIZE, ITEM_SIZE, MAX_LEVEL, Root, read_block};
use crate::volume::Volume;

/// A change the extent tree lacks: a tree block made or freed, with its
/// level and the tree that owns it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Change {
    /// The block at `bytenr` was made.
    Made { bytenr: u64, level: u8, owner: u64 },
    /// The block at `bytenr` was freed.
    Freed { bytenr: u64, level: u8, owner: u64 },
}

/// What a walk down a tree is for.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Want {
    /// To insert an item with this many bytes of data.
    Insert(usize),
    /// To delete an item.
    Delete,
    /// To change an item's data in place.
    Change,
}

/// A walk down a tree, every block on it the transaction's: each level's
/// block and slot, `steps[0]` the leaf and `steps[top]` the root.
struct Path {
    tree: u64,
    steps: [Option<(u64, usize)>; MAX_LEVEL as usize],
    top: usize,
}

impl Path {
    fn at(&self, level: usize) -> Result<(u64, usize)> {
        self.steps.get(level).copied().flatten().ok_or(Error::Corrupt)
    }
}

/// The running transaction of a filesystem, and the space it takes from.
pub struct Writer {
    pub(crate) space: Space,
    /// The transaction's number: one more than the last commit's.
    pub(crate) generation: u64,
    /// The changes the extent tree lacks, in order.
    pub(crate) changes: KVec<Change>,
    /// The places this transaction freed that the disk's trees may use.
    pub(crate) pinned: KVec<(u64, u64)>,
    /// Each tree's change in bytes of blocks, for its root item.
    pub(crate) tree_used: KVec<(u64, i64)>,
    /// The change in bytes allocated, for the superblock.
    pub(crate) bytes_used: i64,
    /// The chunks it made.
    pub(crate) grown: Grown,
    /// The incompatible features its changes use, which the commit sets
    /// in the superblock: zstd, once it writes an extent so.
    pub(crate) incompat: u64,
    /// Whether a change failed half made.
    broken: bool,
}

impl Writer {
    /// Starts writing `volume`.
    ///
    /// Fails with [`Error::ReadOnly`] for a device that cannot be written,
    /// and [`Error::NotSupported`] for a filesystem Radix cannot write
    /// yet: one with features it does not write, on more than one device
    /// (Phase 10e), with a log tree Linux left to replay (10c.2), or
    /// without the free-space tree or the skinny extent items every
    /// `mkfs.btrfs` since 2013 makes.
    #[inline(never)]
    pub fn new(volume: &Volume) -> Result<Writer> {
        /// What the writer does not keep up yet.
        const REFUSED: u64 = incompat::RAID56
            | incompat::ZONED
            | incompat::EXTENT_TREE_V2
            | incompat::RAID_STRIPE_TREE
            | incompat::SIMPLE_QUOTA;
        let sb = volume.superblock();
        if volume.device().info().read_only {
            return Err(Error::ReadOnly);
        }
        if sb.compat_ro_flags & !compat_ro::WRITE != 0
            || !sb.has(incompat::SKINNY_METADATA)
            || sb.incompat_flags & REFUSED != 0
            || sb.num_devices != 1
            || sb.log_root != 0
        {
            return Err(Error::NotSupported);
        }
        Writer::check_trees(volume)?;
        let space = Space::load(volume)?;
        Ok(Writer {
            space,
            generation: volume.root_tree().generation + 1,
            changes: mem::vec(),
            pinned: mem::vec(),
            tree_used: mem::vec(),
            bytes_used: 0,
            grown: Grown::new(),
            incompat: 0,
            broken: false,
        })
    }

    /// Fails with [`Error::NotSupported`] if the root tree names a tree
    /// whose blocks may be shared or counted elsewhere: a subvolume that
    /// was ever snapshotted, quotas, or a relocation under way (10c.2 and
    /// later). A frame of its own, as each of a mount's reads has.
    #[inline(never)]
    fn check_trees(volume: &Volume) -> Result<()> {
        /// A tree relocation copies blocks into, for a balance.
        const TREE_RELOC: u64 = (-8i64) as u64;
        let mut cursor = Cursor::seek(volume, &volume.root_tree(), &Key::new(0, 0, 0))?;
        while let Some((key, data)) = cursor.get() {
            if key.kind == item::ROOT_ITEM {
                // last_snapshot, after generation, root_dirid, bytenr,
                // byte_limit and bytes_used, is set on a snapshot's source;
                // a snapshot's key has the transaction that made it.
                let snapshotted = crate::format::u64_at(data, 200) != 0 || key.offset != 0;
                if snapshotted || key.objectid == tree::QUOTA || key.objectid == TREE_RELOC {
                    return Err(Error::NotSupported);
                }
            }
            cursor.advance()?;
        }
        Ok(())
    }

    /// Whether the transaction has anything to commit.
    pub fn is_dirty(&self) -> bool {
        !self.changes.is_empty() || !self.pinned.is_empty() || self.bytes_used != 0
    }

    /// Runs a change, and refuses every later one if it fails half made:
    /// anything but the item being there, or not, when it should not.
    fn guard<R>(&mut self, f: impl FnOnce(&mut Writer) -> Result<R>) -> Result<R> {
        if self.broken {
            return Err(Error::ReadOnly);
        }
        let result = f(self);
        if let Err(e) = result
            && !matches!(e, Error::Exists | Error::NotFound | Error::TooBig)
        {
            self.broken = true;
        }
        result
    }

    /// Refuses every later change and commit: what is in memory is half
    /// made.
    pub(crate) fn break_off(&mut self) {
        self.broken = true;
    }

    /// Fails if a change failed half made.
    pub(crate) fn check_whole(&self) -> Result<()> {
        if self.broken { Err(Error::ReadOnly) } else { Ok(()) }
    }

    /// Inserts an item of `size` bytes with key `key` into tree `tree`,
    /// and lets `fill` write its data. Fails with [`Error::Exists`] if the
    /// tree has an item with that key.
    pub fn insert_with(
        &mut self,
        volume: &Volume,
        tree: u64,
        key: &Key,
        size: usize,
        fill: impl FnOnce(&mut [u8]),
    ) -> Result<()> {
        if size > Block::max_item(volume.superblock().nodesize) {
            return Err(Error::TooBig);
        }
        self.guard(|w| {
            let path = w.search(volume, tree, key, Want::Insert(size))?;
            let (leaf, slot) = path.at(0)?;
            let blocks = volume.blocks();
            if blocks.view(leaf, |b| slot < b.nritems() && b.key(slot) == *key)? {
                return Err(Error::Exists);
            }
            blocks.edit(leaf, |b| fill(b.insert_item(slot, key, size)))?;
            if slot == 0 {
                Self::fix_keys(volume, &path, 1, key)?;
            }
            Ok(())
        })
    }

    /// Inserts the item `key` with data `data` into tree `tree`.
    pub fn insert(&mut self, volume: &Volume, tree: u64, key: &Key, data: &[u8]) -> Result<()> {
        self.insert_with(volume, tree, key, data.len(), |out| out.copy_from_slice(data))
    }

    /// Changes the data of item `key` of tree `tree` in place. Fails with
    /// [`Error::NotFound`] if there is no such item.
    pub fn update<R>(
        &mut self,
        volume: &Volume,
        tree: u64,
        key: &Key,
        change: impl FnOnce(&mut [u8]) -> R,
    ) -> Result<R> {
        self.guard(|w| {
            let path = w.search(volume, tree, key, Want::Change)?;
            let (leaf, slot) = path.at(0)?;
            let blocks = volume.blocks();
            if !blocks.view(leaf, |b| slot < b.nritems() && b.key(slot) == *key)? {
                return Err(Error::NotFound);
            }
            blocks.edit(leaf, |b| change(b.item_mut(slot)))
        })
    }

    /// Deletes item `key` of tree `tree`. Fails with [`Error::NotFound`]
    /// if there is no such item.
    pub fn delete(&mut self, volume: &Volume, tree: u64, key: &Key) -> Result<()> {
        self.guard(|w| {
            let mut path = w.search(volume, tree, key, Want::Delete)?;
            let (leaf, slot) = path.at(0)?;
            let blocks = volume.blocks();
            if !blocks.view(leaf, |b| slot < b.nritems() && b.key(slot) == *key)? {
                return Err(Error::NotFound);
            }
            blocks.edit(leaf, |b| b.delete_items(slot, 1))?;
            w.after_delete(volume, &mut path, 0)
        })
    }

    /// Changes the size of item `key` of tree `tree` to `size`, keeping
    /// the start of its data; new bytes are zero.
    pub fn resize(&mut self, volume: &Volume, tree: u64, key: &Key, size: usize) -> Result<()> {
        if size > Block::max_item(volume.superblock().nodesize) {
            return Err(Error::TooBig);
        }
        let fits = self.guard(|w| {
            let path = w.search(volume, tree, key, Want::Change)?;
            let (leaf, slot) = path.at(0)?;
            let blocks = volume.blocks();
            let (found, old, free) = blocks.view(leaf, |b| {
                let found = slot < b.nritems() && b.key(slot) == *key;
                (found, if found { b.item_size(slot) } else { 0 }, b.leaf_free())
            })?;
            if !found {
                return Err(Error::NotFound);
            }
            if size <= old || size - old <= free {
                blocks.edit(leaf, |b| b.resize_item(slot, size))?;
                return Ok(None);
            }
            // It does not fit where it is: it moves, through a copy.
            Ok(Some(blocks.view(leaf, |b| mem::copy(b.item(slot)))??))
        })?;
        if let Some(old) = fits {
            self.delete(volume, tree, key)?;
            self.insert_with(volume, tree, key, size, |out| {
                out[..old.len()].copy_from_slice(&old);
            })?;
        }
        Ok(())
    }

    /// The root block of tree `tree`, as the transaction has it.
    pub(crate) fn tree_root(volume: &Volume, tree: u64) -> Result<Root> {
        match tree {
            tree::ROOT => Ok(volume.root_tree()),
            tree::CHUNK => Ok(volume.chunk_root()),
            _ => volume.tree(tree)?.ok_or(Error::Corrupt),
        }
    }

    /// The key of tree `tree`'s root item in the root tree.
    fn root_item_key(volume: &Volume, tree: u64) -> Result<Key> {
        let cursor = Cursor::seek(volume, &volume.root_tree(), &Key::first(tree, item::ROOT_ITEM))?;
        match cursor.key() {
            Some(key) if key.is(tree, item::ROOT_ITEM) => Ok(key),
            _ => Err(Error::Corrupt),
        }
    }

    /// Points tree `tree` at a new root block: the superblock's fields
    /// for the root and chunk trees, the root item for the others.
    fn set_tree_root(&mut self, volume: &Volume, tree: u64, root: Root) -> Result<()> {
        match tree {
            tree::ROOT => volume.set_root_tree(root),
            tree::CHUNK => volume.set_chunk_root(root),
            _ => {
                let key = Writer::root_item_key(volume, tree)?;
                let generation = self.generation;
                self.update(volume, tree::ROOT, &key, |data| {
                    // After the inode item: generation, root_dirid, bytenr;
                    // the level at 238 and generation_v2 at 239.
                    data[160..168].copy_from_slice(&generation.to_le_bytes());
                    data[176..184].copy_from_slice(&root.bytenr.to_le_bytes());
                    data[238] = root.level;
                    if data.len() >= 247 {
                        data[239..247].copy_from_slice(&generation.to_le_bytes());
                    }
                })?;
            }
        }
        Ok(())
    }

    /// Adds `delta` bytes to tree `tree`'s count of its blocks.
    fn count(&mut self, tree: u64, delta: i64) -> Result<()> {
        self.bytes_used += delta;
        match self.tree_used.iter_mut().find(|e| e.0 == tree) {
            Some(entry) => entry.1 += delta,
            None => mem::push(&mut self.tree_used, (tree, delta))?,
        }
        Ok(())
    }

    /// Takes a place for a new block at `level` of tree `tree`.
    fn alloc_block(&mut self, volume: &Volume, tree: u64, level: u8) -> Result<u64> {
        let nodesize = u64::from(volume.superblock().nodesize);
        let want = if tree == tree::CHUNK { kind::SYSTEM } else { kind::METADATA };
        mem::push(&mut self.changes, Change::Made { bytenr: 0, level, owner: tree })?;
        // A block never crosses a stripe of 64 KiB: it is aligned to its size.
        let bytenr = match self.take(volume, want, nodesize, nodesize) {
            Ok(bytenr) => bytenr,
            Err(e) => {
                self.changes.pop();
                return Err(e);
            }
        };
        if let Some(Change::Made { bytenr: at, .. }) = self.changes.last_mut() {
            *at = bytenr;
        }
        self.count(tree, nodesize as i64)?;
        Ok(bytenr)
    }

    /// Frees the block `root` of tree `tree`: at once if the transaction
    /// made it, after the commit otherwise.
    fn free_block(&mut self, volume: &Volume, tree: u64, root: &Root) -> Result<()> {
        let nodesize = u64::from(volume.superblock().nodesize);
        self.count(tree, -(nodesize as i64))?;
        if root.generation == self.generation && volume.blocks().take_dirty(root.bytenr).is_some() {
            let made = Change::Made { bytenr: root.bytenr, level: root.level, owner: tree };
            match self.changes.iter().position(|c| *c == made) {
                Some(at) => {
                    self.changes.remove(at);
                }
                None => mem::push(
                    &mut self.changes,
                    Change::Freed { bytenr: root.bytenr, level: root.level, owner: tree },
                )?,
            }
            return self.space.free_now(root.bytenr, nodesize);
        }
        mem::push(
            &mut self.changes,
            Change::Freed { bytenr: root.bytenr, level: root.level, owner: tree },
        )?;
        mem::push(&mut self.pinned, (root.bytenr, nodesize))?;
        self.space.free_pinned(root.bytenr, nodesize)
    }

    /// Takes `len` bytes for file data, whole sectors; commits first if
    /// only what the last commit freed has room. It is called between
    /// changes, never inside one.
    pub(crate) fn alloc_data(&mut self, volume: &Volume, len: u64) -> Result<u64> {
        let sector = u64::from(volume.superblock().sectorsize);
        let at = match self.take(volume, kind::DATA, len, sector) {
            Err(Error::NoSpace) if self.space.pinned_bytes(kind::DATA) >= len => {
                self.commit(volume)?;
                self.take(volume, kind::DATA, len, sector)?
            }
            taken => taken?,
        };
        self.bytes_used += len as i64;
        Ok(at)
    }

    /// Takes `size` bytes aligned to `align` from a block group of `want`,
    /// or from a new chunk when every such group is full.
    fn take(&mut self, volume: &Volume, want: u64, size: u64, align: u64) -> Result<u64> {
        match self.space.take(want, size, align) {
            Err(Error::NoSpace) => {
                self.new_chunk(volume, want, size.next_multiple_of(align))?;
                self.space.take(want, size, align)
            }
            taken => taken,
        }
    }

    /// Frees the data extent `[start, start + len)`: at once if this
    /// transaction wrote it (`made`), so no tree on the disk names it,
    /// after the commit otherwise.
    pub(crate) fn free_data(&mut self, start: u64, len: u64, made: bool) -> Result<()> {
        self.bytes_used -= len as i64;
        if made {
            return self.space.free_now(start, len);
        }
        mem::push(&mut self.pinned, (start, len))?;
        self.space.free_pinned(start, len)
    }

    /// A new, empty block at `level` of tree `tree`, with the UUIDs of
    /// `like`'s header.
    fn new_block(&mut self, volume: &Volume, tree: u64, level: u8, like: u64) -> Result<Block> {
        let nodesize = volume.superblock().nodesize as usize;
        let mut bytes = crate::zeroed(nodesize)?;
        volume
            .blocks()
            .view(like, |b| bytes[..HEADER_SIZE].copy_from_slice(&b.bytes()[..HEADER_SIZE]))?;
        let bytenr = self.alloc_block(volume, tree, level)?;
        let mut block = Block::from_bytes(bytenr, bytes);
        block.set_header(bytenr, self.generation, tree, level);
        block.clear();
        Ok(block)
    }

    /// Makes `root`, a block of tree `tree`, the transaction's: copies it
    /// unless the transaction made it, and points `parent`'s slot, or the
    /// tree, at the copy. Returns where the block now is.
    fn cow(
        &mut self,
        volume: &Volume,
        tree: u64,
        root: &Root,
        bounds: &Bounds,
        parent: Option<(u64, usize)>,
    ) -> Result<u64> {
        let blocks = volume.blocks();
        if root.generation == self.generation && blocks.is_dirty(root.bytenr) {
            return Ok(root.bytenr);
        }
        let old = read_block(volume, root, bounds)?;
        // A block another tree owns is shared with a snapshot (10c.2).
        if old.owner() != tree {
            return Err(Error::NotSupported);
        }
        let bytes = mem::copy(old.bytes())?;
        drop(old);
        blocks.reserve_dirty(1)?;
        let bytenr = self.alloc_block(volume, tree, root.level)?;
        let mut block = Block::from_bytes(bytenr, bytes);
        block.set_header(bytenr, self.generation, tree, root.level);
        blocks.put_dirty(mem::arc(block)?)?;
        self.free_block(volume, tree, root)?;
        let copy = Root { bytenr, level: root.level, generation: self.generation };
        match parent {
            Some((node, slot)) => blocks.edit(node, |b| b.set_child(slot, &copy))?,
            None => self.set_tree_root(volume, tree, copy)?,
        }
        Ok(bytenr)
    }

    /// Walks down tree `tree` to where `key` is or would be, making every
    /// block on the way the transaction's, and making room for `want`.
    fn search(&mut self, volume: &Volume, tree: u64, key: &Key, want: Want) -> Result<Path> {
        if tree == tree::ROOT {
            // A change to the root tree may move another tree's root.
            volume.forget_trees();
        }
        let nodesize = volume.superblock().nodesize;
        let blocks = volume.blocks();
        let root = Self::tree_root(volume, tree)?;
        let mut path = Path { tree, steps: [None; MAX_LEVEL as usize], top: root.level.into() };
        let mut bytenr = self.cow(volume, tree, &root, &Bounds::default(), None)?;
        let mut level = path.top;
        loop {
            let (leaf, n) = blocks.view(bytenr, |b| (b.is_leaf(), b.nritems()))?;
            if leaf {
                let slot = blocks.view(bytenr, |b| b.lower_bound(key))?;
                path.steps[0] = Some((bytenr, slot));
                if let Want::Insert(size) = want
                    && blocks.view(bytenr, Block::leaf_free)? < size + ITEM_SIZE
                {
                    if path.top == 0 {
                        self.grow(volume, &mut path)?;
                    }
                    self.split_leaf(volume, &mut path, key, size)?;
                }
                return Ok(path);
            }
            path.steps[level] = Some((bytenr, 0));
            if want != Want::Delete && want != Want::Change && n + 2 > Block::max_ptrs(nodesize) {
                if level == path.top {
                    self.grow(volume, &mut path)?;
                }
                bytenr = self.split_node(volume, &mut path, level, key)?;
            }
            let (slot, child, bounds) = blocks.view(bytenr, |b| {
                let slot = b.lower_bound(key);
                let slot = if slot < b.nritems() && b.key(slot) == *key {
                    slot
                } else {
                    slot.saturating_sub(1)
                };
                (slot, b.child(slot), Bounds::of_child(b, slot))
            })?;
            path.steps[level] = Some((bytenr, slot));
            bytenr = self.cow(volume, tree, &child, &bounds, Some((bytenr, slot)))?;
            level -= 1;
        }
    }

    /// Puts a new root above the tree's root, with it as its one child.
    fn grow(&mut self, volume: &Volume, path: &mut Path) -> Result<()> {
        let top = path.top;
        if top + 1 >= MAX_LEVEL as usize {
            return Err(Error::NoSpace);
        }
        let (old, _) = path.at(top)?;
        let blocks = volume.blocks();
        let (child, first) = blocks.view(old, |b| {
            let first = if b.nritems() > 0 { b.key(0) } else { Key::new(0, 0, 0) };
            (b.root(), first)
        })?;
        blocks.reserve_dirty(1)?;
        let mut root = self.new_block(volume, path.tree, (top + 1) as u8, old)?;
        root.insert_ptr(0, &first, &child);
        let new = root.root();
        blocks.put_dirty(mem::arc(root)?)?;
        path.top = top + 1;
        path.steps[top + 1] = Some((new.bytenr, 0));
        self.set_tree_root(volume, path.tree, new)
    }

    /// Splits node `level` of the path in two, its upper half going to a
    /// new node after it; returns the half where `key` belongs, and moves
    /// the path's slot above to it.
    fn split_node(
        &mut self,
        volume: &Volume,
        path: &mut Path,
        level: usize,
        key: &Key,
    ) -> Result<u64> {
        let (node, _) = path.at(level)?;
        let (parent, pslot) = path.at(level + 1)?;
        let blocks = volume.blocks();
        blocks.reserve_dirty(1)?;
        let mut right = self.new_block(volume, path.tree, level as u8, node)?;
        blocks.edit(node, |b| {
            let n = b.nritems();
            let mid = n / 2;
            right.append_ptrs(b, mid, n - mid);
            b.delete_ptrs(mid, n - mid);
        })?;
        let right_key = right.key(0);
        let right_root = right.root();
        blocks.put_dirty(mem::arc(right)?)?;
        blocks.edit(parent, |b| b.insert_ptr(pslot + 1, &right_key, &right_root))?;
        if *key >= right_key {
            path.steps[level + 1] = Some((parent, pslot + 1));
            path.steps[level] = Some((right_root.bytenr, 0));
            Ok(right_root.bytenr)
        } else {
            Ok(node)
        }
    }

    /// Splits the path's leaf so that an item of `size` bytes with key
    /// `key` fits at its slot, and moves the path to where it goes.
    fn split_leaf(
        &mut self,
        volume: &Volume,
        path: &mut Path,
        key: &Key,
        size: usize,
    ) -> Result<()> {
        let (leaf, slot) = path.at(0)?;
        let (parent, pslot) = path.at(1)?;
        let blocks = volume.blocks();
        let nodesize = volume.superblock().nodesize as usize;
        let room = nodesize - HEADER_SIZE;
        // The best split of the items with the new one among them: both
        // halves fit, and are as near alike as can be.
        let best = blocks.view(leaf, |b| {
            let n = b.nritems();
            let size_of = |v: usize| -> usize {
                ITEM_SIZE
                    + match v.cmp(&slot) {
                        core::cmp::Ordering::Less => b.item_size(v),
                        core::cmp::Ordering::Equal => size,
                        core::cmp::Ordering::Greater => b.item_size(v - 1),
                    }
            };
            let total: usize = (0..=n).map(size_of).sum();
            let mut best: Option<(usize, usize)> = None;
            let mut prefix = size_of(0);
            for m in 1..=n {
                if prefix <= room && total - prefix <= room {
                    let score = (2 * prefix).abs_diff(total);
                    if best.is_none_or(|(_, s)| score < s) {
                        best = Some((m, score));
                    }
                }
                prefix += size_of(m);
            }
            (best.map(|(m, _)| m), n)
        })?;
        let (best, n) = best;
        blocks.reserve_dirty(2)?;
        match best {
            Some(m) => {
                // The new item goes left of the split if it is among the
                // first `m`.
                let (at, goes_right) = if m <= slot { (m, true) } else { (m - 1, false) };
                let right = self.split_leaf_at(volume, path, at, key)?;
                if goes_right {
                    path.steps[1] = Some((parent, pslot + 1));
                    path.steps[0] = Some((right, slot - at));
                }
            }
            None if slot == 0 => {
                // Too big to share a leaf with its neighbors: the leaf's
                // items move right, and it holds the new one alone.
                self.split_leaf_at(volume, path, 0, key)?;
            }
            None if slot == n => {
                let right = self.split_leaf_at(volume, path, n, key)?;
                path.steps[1] = Some((parent, pslot + 1));
                path.steps[0] = Some((right, 0));
            }
            None => {
                self.split_leaf_at(volume, path, slot, key)?;
                // And a leaf of its own between the two halves.
                let mid = self.new_block(volume, path.tree, 0, leaf)?;
                let mid_root = mid.root();
                blocks.put_dirty(mem::arc(mid)?)?;
                blocks.edit(parent, |b| b.insert_ptr(pslot + 1, key, &mid_root))?;
                path.steps[1] = Some((parent, pslot + 1));
                path.steps[0] = Some((mid_root.bytenr, 0));
            }
        }
        Ok(())
    }

    /// Moves the items from `at` of the path's leaf to a new leaf after
    /// it, and returns where it is. An empty new leaf is keyed by `key`,
    /// the item about to go there.
    fn split_leaf_at(&mut self, volume: &Volume, path: &Path, at: usize, key: &Key) -> Result<u64> {
        let (leaf, _) = path.at(0)?;
        let (parent, pslot) = path.at(1)?;
        let blocks = volume.blocks();
        let mut right = self.new_block(volume, path.tree, 0, leaf)?;
        blocks.edit(leaf, |b| {
            let n = b.nritems();
            right.append_items(b, at, n - at);
            b.delete_items(at, n - at);
        })?;
        let right_key = if right.nritems() > 0 { right.key(0) } else { *key };
        let right_root = right.root();
        blocks.put_dirty(mem::arc(right)?)?;
        blocks.edit(parent, |b| b.insert_ptr(pslot + 1, &right_key, &right_root))?;
        Ok(right_root.bytenr)
    }

    /// Sets the keys above `level` of the path to `key`, the new first key
    /// of the block below: up the path while that block is its parent's
    /// first.
    fn fix_keys(volume: &Volume, path: &Path, level: usize, key: &Key) -> Result<()> {
        for l in level..=path.top {
            let (node, slot) = path.at(l)?;
            volume.blocks().edit(node, |b| b.set_key(slot, key))?;
            if slot != 0 {
                break;
            }
        }
        Ok(())
    }

    /// After items or key pointers left block `level` of the path: frees
    /// it if it is empty, fixes the keys above it, and lets a root node
    /// with one child give way to it.
    fn after_delete(&mut self, volume: &Volume, path: &mut Path, level: usize) -> Result<()> {
        let blocks = volume.blocks();
        let (block, slot) = path.at(level)?;
        let (n, root, first) = blocks.view(block, |b| {
            let n = b.nritems();
            (n, b.root(), if n > 0 { Some(b.key(0)) } else { None })
        })?;
        if level == path.top {
            if level > 0 && n == 0 {
                // Every leaf went: the tree is one empty leaf again.
                let leaf = self.new_block(volume, path.tree, 0, block)?;
                let leaf_root = leaf.root();
                blocks.put_dirty(mem::arc(leaf)?)?;
                self.set_tree_root(volume, path.tree, leaf_root)?;
                self.free_block(volume, path.tree, &root)?;
                path.steps[level] = None;
                path.top = 0;
                path.steps[0] = Some((leaf_root.bytenr, 0));
            } else if level > 0 && n == 1 {
                let child = blocks.view(block, |b| b.child(0))?;
                self.set_tree_root(volume, path.tree, child)?;
                self.free_block(volume, path.tree, &root)?;
                path.steps[level] = None;
                path.top -= 1;
            }
            return Ok(());
        }
        let (parent, pslot) = path.at(level + 1)?;
        let Some(first) = first else {
            blocks.edit(parent, |b| b.delete_ptrs(pslot, 1))?;
            self.free_block(volume, path.tree, &root)?;
            path.steps[level] = None;
            return self.after_delete(volume, path, level + 1);
        };
        if slot == 0 {
            Self::fix_keys(volume, path, level + 1, &first)?;
        }
        if let Some(gone) = self.merge(volume, path, level)? {
            path.steps[level + 1] = Some((parent, gone));
            return self.after_delete(volume, path, level + 1);
        }
        Ok(())
    }

    /// Merges block `level` of the path with a neighbor if it is under a
    /// quarter full and they fit in one block, as Linux keeps its trees:
    /// into the neighbor before it, or the one after it into it. Returns
    /// the slot of the block above whose block went.
    fn merge(&mut self, volume: &Volume, path: &mut Path, level: usize) -> Result<Option<usize>> {
        let blocks = volume.blocks();
        let nodesize = volume.superblock().nodesize;
        let room = nodesize as usize - HEADER_SIZE;
        let max_ptrs = Block::max_ptrs(nodesize);
        let (block, _) = path.at(level)?;
        let (parent, pslot) = path.at(level + 1)?;
        // What a block holds, and whether `extra` of it would fit in it.
        let size = |b: &Block| if b.is_leaf() { b.leaf_used() } else { b.nritems() };
        let fits = |b: &Block, extra: usize| {
            if b.is_leaf() { b.leaf_free() >= extra } else { b.nritems() + extra <= max_ptrs }
        };
        let (mine, small, me) = blocks.view(block, |b| {
            let small =
                if b.is_leaf() { b.leaf_used() < room / 4 } else { b.nritems() < max_ptrs / 4 };
            (size(b), small, b.root())
        })?;
        if !small {
            return Ok(None);
        }
        let (left, right) = blocks.view(parent, |p| {
            let n = p.nritems();
            let left = (pslot > 0).then(|| (p.child(pslot - 1), Bounds::of_child(p, pslot - 1)));
            let right =
                (pslot + 1 < n).then(|| (p.child(pslot + 1), Bounds::of_child(p, pslot + 1)));
            (left, right)
        })?;
        if let Some((left, bounds)) = left
            && fits(&*read_block(volume, &left, &bounds)?, mine)
        {
            let into = self.cow(volume, path.tree, &left, &bounds, Some((parent, pslot - 1)))?;
            Self::move_all(volume, block, into)?;
            blocks.edit(parent, |p| p.delete_ptrs(pslot, 1))?;
            self.free_block(volume, path.tree, &me)?;
            path.steps[level] = None;
            return Ok(Some(pslot));
        }
        if let Some((right, bounds)) = right {
            let theirs = size(&*read_block(volume, &right, &bounds)?);
            if blocks.view(block, |b| fits(b, theirs))? {
                let from =
                    self.cow(volume, path.tree, &right, &bounds, Some((parent, pslot + 1)))?;
                let gone = blocks.view(from, Block::root)?;
                Self::move_all(volume, from, block)?;
                blocks.edit(parent, |p| p.delete_ptrs(pslot + 1, 1))?;
                self.free_block(volume, path.tree, &gone)?;
                return Ok(Some(pslot + 1));
            }
        }
        Ok(None)
    }

    /// Appends every item or key pointer of the transaction's block
    /// `from` to its block `into`, which has room for them, and empties
    /// `from`.
    fn move_all(volume: &Volume, from: u64, into: u64) -> Result<()> {
        let blocks = volume.blocks();
        let mut src = blocks.take_dirty(from).ok_or(Error::Corrupt)?;
        let moved = blocks.edit(into, |b| {
            let n = src.nritems();
            if src.is_leaf() { b.append_items(&src, 0, n) } else { b.append_ptrs(&src, 0, n) }
        });
        if let Some(b) = mem::KArc::get_mut(&mut src) {
            b.clear();
        }
        blocks.put_dirty(src)?;
        moved
    }
}
