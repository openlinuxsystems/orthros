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
//! A subvolume's blocks from before its last snapshot may be shared with
//! other trees. Copying one of those keeps the references of the blocks
//! below it right, as Linux's `update_ref_for_cow` does
//! ([`Writer::share`]): the copy refers to them by its tree, and a block
//! that other trees still hold refers to them by its address from then
//! on ([`FULL_BACKREF`]). Those changes wait until the change that made
//! the copy is done ([`Delayed`]), as Linux's delayed references do: a
//! walk down one tree never changes another.
//!
//! Inserting splits a full node on the way down, so the block above
//! always has room for a split below it, and splits the leaf if the new
//! item does not fit. Deleting frees a block left empty, and a root node
//! left with one child gives way to it. Any failure once a change has
//! begun leaves the transaction half made: the writer then refuses every
//! change and commit, so the disk keeps the last commit whole.

use radix_vfs::mem::{self, KBox, KVec};
use radix_vfs::{Error, Result};

use crate::chunk::kind;
use crate::format::{Key, item, tree};
use crate::grow::Grown;
use crate::log::Logging;
use crate::refs::{Backref, FULL_BACKREF, refs_flags};
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

/// The extent item key of what slot `slot` of `block` refers to, and its
/// reference from it `via` its tree or address: a child block, or the
/// data extent of a file extent item; `None` for other items and for
/// file extents with no extent (inline, or a hole).
pub(crate) fn below(block: &Block, slot: usize, via: Via) -> Result<Option<(Key, Backref)>> {
    if block.is_leaf() {
        let key = block.key(slot);
        if key.kind != item::EXTENT_DATA {
            return Ok(None);
        }
        let Some((bytenr, len, offset)) = crate::files::disk_extent(block.item(slot))? else {
            return Ok(None);
        };
        let r = match via {
            Via::Tree(root) => {
                Backref::Data { root, ino: key.objectid, offset: key.offset.wrapping_sub(offset) }
            }
            Via::Address(parent) => Backref::SharedData(parent),
        };
        return Ok(Some((Key::new(bytenr, item::EXTENT_ITEM, len), r)));
    }
    let child = block.child(slot);
    let r = match via {
        Via::Tree(root) => Backref::Tree(root),
        Via::Address(parent) => Backref::Shared(parent),
    };
    Ok(Some((Writer::block_key(child.bytenr, child.level), r)))
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
    /// Each subvolume's last snapshot, as read from its root item: its
    /// blocks from up to then may be shared.
    pub(crate) snapshots: KVec<(u64, u64)>,
    /// The reference changes copies of shared blocks left, in order.
    delayed: KVec<Delayed>,
    /// What the log of fsyncs keeps (ADR 0154), boxed so that a writer
    /// moved by value on the mount's path stays small.
    pub(crate) logging: KBox<Logging>,
    /// Whether a change failed half made.
    broken: bool,
}

/// A reference change a copy of a shared block calls for, made once the
/// change that copied it is done.
#[derive(Clone, Copy)]
struct Delayed {
    /// The extent's item.
    key: Key,
    /// The reference added or taken, or `None` to mark the block
    /// [`FULL_BACKREF`].
    r: Option<Backref>,
    add: bool,
}

/// How the blocks or data below a tree block refer to it.
#[derive(Clone, Copy)]
pub(crate) enum Via {
    /// By the block's tree.
    Tree(u64),
    /// By the block's address.
    Address(u64),
}

impl Writer {
    /// Starts writing `volume`.
    ///
    /// Fails with [`Error::ReadOnly`] for a disk that cannot be written,
    /// and [`Error::NotSupported`] for a filesystem Radix cannot write
    /// yet: one with features it does not write (RAID5 and RAID6 among
    /// them), or without the free-space tree or the skinny extent items
    /// every `mkfs.btrfs` since 2013 makes.
    #[inline(never)]
    pub fn new(volume: &Volume) -> Result<Writer> {
        /// What the writer does not keep up yet.
        const REFUSED: u64 = incompat::RAID56
            | incompat::ZONED
            | incompat::EXTENT_TREE_V2
            | incompat::RAID_STRIPE_TREE
            | incompat::SIMPLE_QUOTA;
        let sb = volume.superblock();
        if volume.read_only() {
            return Err(Error::ReadOnly);
        }
        if sb.compat_ro_flags & !compat_ro::WRITE != 0
            || !sb.has(incompat::SKINNY_METADATA)
            || sb.incompat_flags & REFUSED != 0
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
            snapshots: mem::vec(),
            delayed: mem::vec(),
            logging: mem::boxed(Logging::new())?,
            broken: false,
        })
    }

    /// Fails with [`Error::NotSupported`] if the root tree names a tree
    /// whose blocks are counted elsewhere or moving: quotas, or a
    /// relocation under way (later). A frame of its own, as each of a
    /// mount's reads has.
    #[inline(never)]
    fn check_trees(volume: &Volume) -> Result<()> {
        /// A tree relocation copies blocks into, for a balance.
        const TREE_RELOC: u64 = (-8i64) as u64;
        let mut cursor = Cursor::seek(volume, &volume.root_tree(), &Key::new(0, 0, 0))?;
        while let Some((key, _)) = cursor.get() {
            if key.kind == item::ROOT_ITEM
                && (key.objectid == tree::QUOTA || key.objectid == TREE_RELOC)
            {
                return Err(Error::NotSupported);
            }
            cursor.advance()?;
        }
        Ok(())
    }

    /// Whether tree `tree` is a subvolume, whose blocks snapshots share.
    pub(crate) fn is_subvolume(tree: u64) -> bool {
        tree == tree::FS || (tree::FIRST_FREE..crate::files::object::LAST_FREE).contains(&tree)
    }

    /// The transaction of subvolume `tree`'s last snapshot: 0 if it has
    /// none.
    pub(crate) fn last_snapshot(&mut self, volume: &Volume, tree: u64) -> Result<u64> {
        if let Some(&(_, last)) = self.snapshots.iter().find(|e| e.0 == tree) {
            return Ok(last);
        }
        let key = Writer::root_item_key(volume, tree)?;
        let data = crate::tree::find(volume, &volume.root_tree(), &key)?.ok_or(Error::Corrupt)?;
        // last_snapshot, after generation, root_dirid, bytenr, byte_limit
        // and bytes_used.
        let last = crate::format::u64_at(data.get(..208).ok_or(Error::Corrupt)?, 200);
        mem::push(&mut self.snapshots, (tree, last))?;
        Ok(last)
    }

    /// Notes that subvolume `tree`'s last snapshot is now at transaction
    /// `generation`, as its root item says.
    pub(crate) fn note_snapshot(&mut self, tree: u64, generation: u64) -> Result<()> {
        match self.snapshots.iter_mut().find(|e| e.0 == tree) {
            Some(entry) => entry.1 = generation,
            None => mem::push(&mut self.snapshots, (tree, generation))?,
        }
        Ok(())
    }

    /// Forgets tree `tree`, dropped: what the commit would note in its
    /// root item, which is gone.
    pub(crate) fn forget_tree(&mut self, tree: u64) {
        self.tree_used.retain(|e| e.0 != tree);
        self.snapshots.retain(|e| e.0 != tree);
    }

    /// The number for a new tree, as Linux's
    /// `btrfs_init_root_free_objectid` finds it: one past the highest
    /// object number of the root tree's items below the last free one.
    pub(crate) fn next_tree(volume: &Volume) -> Result<u64> {
        let last = crate::files::object::LAST_FREE;
        let below = Key::new(last - 1, u8::MAX, u64::MAX);
        let highest = match Cursor::seek_last(volume, &volume.root_tree(), &below)? {
            Some(cursor) => cursor.key().map_or(0, |key| key.objectid),
            None => 0,
        };
        let next = highest.max(tree::FIRST_FREE - 1) + 1;
        if next >= last { Err(Error::NoSpace) } else { Ok(next) }
    }

    /// A copy of the root block `from` as the root of a new tree `owner`,
    /// as Linux's `btrfs_copy_root` makes a snapshot's: what is below it
    /// gains the new tree's references.
    pub(crate) fn copy_root(&mut self, volume: &Volume, from: &Root, owner: u64) -> Result<Root> {
        let old = read_block(volume, from, &Bounds::default())?;
        let bytes = mem::copy(old.bytes())?;
        drop(old);
        let blocks = volume.blocks();
        blocks.reserve_dirty(1)?;
        let bytenr = self.alloc_block(volume, owner, from.level)?;
        let mut block = Block::from_bytes(bytenr, bytes);
        block.set_header(bytenr, self.generation, owner, from.level);
        self.refer_below(volume, &block, Via::Tree(owner), true)?;
        blocks.put_dirty(mem::arc(block)?)?;
        Ok(Root { bytenr, level: from.level, generation: self.generation })
    }

    /// A new, empty leaf as the root of a new tree `owner`, with the
    /// UUIDs of the header of the block `like`.
    pub(crate) fn new_root_leaf(
        &mut self,
        volume: &Volume,
        owner: u64,
        like: &Root,
    ) -> Result<Root> {
        let mut bytes = crate::zeroed(volume.superblock().nodesize as usize)?;
        let old = read_block(volume, like, &Bounds::default())?;
        bytes[..HEADER_SIZE].copy_from_slice(&old.bytes()[..HEADER_SIZE]);
        drop(old);
        let blocks = volume.blocks();
        blocks.reserve_dirty(1)?;
        let bytenr = self.alloc_block(volume, owner, 0)?;
        let mut block = Block::from_bytes(bytenr, bytes);
        block.set_header(bytenr, self.generation, owner, 0);
        block.clear();
        blocks.put_dirty(mem::arc(block)?)?;
        Ok(Root { bytenr, level: 0, generation: self.generation })
    }

    /// Frees the block at `bytenr`, of a tree being dropped, whose extent
    /// item is already gone: after the commit, as every block on the disk.
    pub(crate) fn free_dropped(&mut self, volume: &Volume, bytenr: u64) -> Result<()> {
        let nodesize = u64::from(volume.superblock().nodesize);
        self.bytes_used -= nodesize as i64;
        mem::push(&mut self.pinned, (bytenr, nodesize))?;
        self.space.free_pinned(bytenr, nodesize)
    }

    /// Whether the transaction has anything to commit.
    pub fn is_dirty(&self) -> bool {
        !self.changes.is_empty()
            || !self.pinned.is_empty()
            || self.bytes_used != 0
            || self.logging.written()
    }

    /// Runs a change, then the reference changes its copies of shared
    /// blocks left, and refuses every later change if it fails half made:
    /// anything but the item being there, or not, when it should not.
    fn guard<R>(&mut self, volume: &Volume, f: impl FnOnce(&mut Writer) -> Result<R>) -> Result<R> {
        if self.broken {
            return Err(Error::ReadOnly);
        }
        let mut result = f(self);
        if !self.delayed.is_empty()
            && !matches!(result, Err(e) if !matches!(e, Error::Exists | Error::NotFound | Error::TooBig))
            && let Err(e) = self.run_delayed(volume)
        {
            result = Err(e);
        }
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
        self.guard(volume, |w| {
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
        self.guard(volume, |w| {
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
        self.guard(volume, |w| {
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
        let fits = self.guard(volume, |w| {
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
    pub(crate) fn root_item_key(volume: &Volume, tree: u64) -> Result<Key> {
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
        self.count_tree(tree, delta)
    }

    /// Adds `delta` bytes to tree `tree`'s count of its blocks alone.
    fn count_tree(&mut self, tree: u64, delta: i64) -> Result<()> {
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

    /// Keeps the block `[start, start + len)` of a log being replayed from
    /// use while this mount lasts (ADR 0151).
    pub(crate) fn keep_from_use(&mut self, start: u64, len: u64) -> Result<()> {
        self.space.keep_from_use(start, len)
    }

    /// Takes the data extent `[start, start + len)` that a log being
    /// replayed names and no commit took (ADR 0151).
    pub(crate) fn take_logged(&mut self, start: u64, len: u64) -> Result<()> {
        self.space.claim(start, len)?;
        self.bytes_used += len as i64;
        Ok(())
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
    /// unless a log of fsyncs on the disk may; after the commit otherwise.
    pub(crate) fn free_data(&mut self, start: u64, len: u64, made: bool) -> Result<()> {
        self.bytes_used -= len as i64;
        if made && !self.logging.written() {
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
        let owner = old.owner();
        // A tree's root block is its own; below it, a subvolume's blocks
        // from up to its last snapshot may be shared.
        let shared = if parent.is_some()
            && Writer::is_subvolume(tree)
            && root.generation <= self.last_snapshot(volume, tree)?
        {
            Some(self.refs_of(volume, &Writer::block_key(root.bytenr, root.level))?)
        } else {
            None
        };
        if shared.is_none() && owner != tree {
            return Err(Error::Corrupt);
        }
        let bytes = mem::copy(old.bytes())?;
        drop(old);
        blocks.reserve_dirty(1)?;
        let bytenr = self.alloc_block(volume, tree, root.level)?;
        let mut block = Block::from_bytes(bytenr, bytes);
        block.set_header(bytenr, self.generation, tree, root.level);
        let last = match shared {
            Some((refs, flags)) => self.share(tree, root, owner, &block, refs, flags)?,
            None => true,
        };
        blocks.put_dirty(mem::arc(block)?)?;
        if last {
            self.free_block(volume, tree, root)?;
        } else {
            // Other trees keep it: only this one stops counting it.
            self.count_tree(tree, -i64::from(volume.superblock().nodesize))?;
        }
        let copy = Root { bytenr, level: root.level, generation: self.generation };
        match parent {
            Some((node, slot)) => blocks.edit(node, |b| b.set_child(slot, &copy))?,
            None => self.set_tree_root(volume, tree, copy)?,
        }
        Ok(bytenr)
    }

    /// Keeps the references below the block `old` of tree `tree`, owned
    /// by `owner`, with `refs` references and `flags`, right as tree
    /// `tree` takes `copy` of it in its place; returns whether `old` lost
    /// its last reference. As Linux's `update_ref_for_cow`: a block other
    /// trees keep that its own tree copies refers to what is below it by
    /// its address from then on, so the copy refers to them by the tree,
    /// as `old` did; one copied by another tree leaves them as they are,
    /// and the copy adds the tree's references; and the last copy of a
    /// block that referred by address moves its references to the tree.
    /// Each block keeps others' references, so only the tree's own goes.
    fn share(
        &mut self,
        tree: u64,
        old: &Root,
        owner: u64,
        copy: &Block,
        refs: u64,
        flags: u64,
    ) -> Result<bool> {
        let full = flags & FULL_BACKREF != 0;
        let key = Writer::block_key(old.bytenr, old.level);
        if refs > 1 {
            if owner == tree && !full {
                self.delay_below(copy, Via::Address(old.bytenr), true)?;
                self.delay(key, None, true)?;
            } else {
                self.delay_below(copy, Via::Tree(tree), true)?;
            }
            self.delay(key, Some(Backref::Tree(tree)), false)?;
            return Ok(false);
        }
        if full {
            self.delay_below(copy, Via::Tree(tree), true)?;
            self.delay_below(copy, Via::Address(old.bytenr), false)?;
        } else if owner != tree {
            // Only its owner refers to what is below a block by its tree.
            return Err(Error::Corrupt);
        }
        Ok(true)
    }

    /// The references and flags of the extent item `key`, with the
    /// changes waiting for the change under way.
    #[inline(never)]
    fn refs_of(&self, volume: &Volume, key: &Key) -> Result<(u64, u64)> {
        let data = Writer::extent_item(volume, key)?.ok_or(Error::Corrupt)?;
        let (mut refs, mut flags) = refs_flags(&data)?;
        for d in self.delayed.iter().filter(|d| d.key == *key) {
            match d.r {
                None => flags |= FULL_BACKREF,
                Some(_) if d.add => refs += 1,
                Some(_) => refs = refs.checked_sub(1).ok_or(Error::Corrupt)?,
            }
        }
        Ok((refs, flags))
    }

    /// Leaves for after the change the reference change `r` to the extent
    /// of item `key`: added, or taken; `None` marks it [`FULL_BACKREF`].
    fn delay(&mut self, key: Key, r: Option<Backref>, add: bool) -> Result<()> {
        Ok(mem::push(&mut self.delayed, Delayed { key, r, add })?)
    }

    /// Leaves for after the change the references `block`'s children or
    /// file extents gain, or lose, from it `via` its tree or address.
    fn delay_below(&mut self, block: &Block, via: Via, add: bool) -> Result<()> {
        for slot in 0..block.nritems() {
            if let Some((key, r)) = below(block, slot, via)? {
                self.delay(key, Some(r), add)?;
            }
        }
        Ok(())
    }

    /// Makes the reference changes copies of shared blocks left, in
    /// order. Their own changes, to the extent tree, leave none.
    pub(crate) fn run_delayed(&mut self, volume: &Volume) -> Result<()> {
        let delayed = core::mem::replace(&mut self.delayed, mem::vec());
        for d in &delayed {
            match d.r {
                None => self.set_extent_flags(volume, &d.key, FULL_BACKREF)?,
                Some(r) if d.add => self.add_ref(volume, &d.key, r, 1)?,
                // What a copy above refers to keeps a reference.
                Some(r) => {
                    if self.drop_ref(volume, &d.key, r)? == 0 {
                        return Err(Error::Corrupt);
                    }
                }
            }
        }
        Ok(())
    }

    /// Adds, or takes away, the references `block`'s children or file
    /// extents have from it `via` its tree or its address, as Linux's
    /// `btrfs_inc_ref` and `btrfs_dec_ref` do. What loses a reference
    /// keeps the one from the copy above.
    pub(crate) fn refer_below(
        &mut self,
        volume: &Volume,
        block: &Block,
        via: Via,
        add: bool,
    ) -> Result<()> {
        for slot in 0..block.nritems() {
            let Some((key, r)) = below(block, slot, via)? else { continue };
            if add {
                self.add_ref(volume, &key, r, 1)?;
            } else if self.drop_ref(volume, &key, r)? == 0 {
                // What a copy above refers to keeps a reference.
                return Err(Error::Corrupt);
            }
        }
        Ok(())
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
                let (slot, there) = blocks.view(bytenr, |b| {
                    let slot = b.lower_bound(key);
                    (slot, slot < n && b.key(slot) == *key)
                })?;
                path.steps[0] = Some((bytenr, slot));
                // An insert of a key already there fails, and must not split
                // the leaf first: the split could move the item away from
                // the slot the insert checks.
                if let Want::Insert(size) = want
                    && !there
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
                // The child is copied first, as Linux's `balance_level`
                // does: a root is the transaction's, its root item names
                // the transaction's generation, and a child the change
                // never reached is older.
                let (child, bounds) =
                    blocks.view(block, |b| (b.child(0), Bounds::of_child(b, 0)))?;
                let copy = self.cow(volume, path.tree, &child, &bounds, Some((block, 0)))?;
                let child = blocks.view(copy, Block::root)?;
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
