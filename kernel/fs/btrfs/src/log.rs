// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Writing the log of fsyncs, as Linux's `tree-log.c` does (ADR 0154):
//! an `fsync` of a file writes what the running transaction changed of it
//! into a log tree of its subvolume, under a log root tree the superblock
//! names, instead of committing every tree. A mount replays the log
//! ([`crate::replay`]), Radix's or Linux's alike, and the next commit
//! drops it.
//!
//! The log holds each file fsynced since the last commit: its inode item,
//! its names and extended attributes, and its extents in the byte ranges
//! the transaction changed, with a hole item for each part of them that
//! has none, so that a replay drops what the commit had there; and the
//! checksums of the data the transaction wrote that those extents name.
//! A file's names must be as the last commit has them, and the file must
//! be in it: a new file, a rename or a link is made durable by a commit,
//! as are directories, a log that would pass [`MAX_ITEM_BYTES`] and a
//! transaction that made chunks.
//!
//! Each fsync writes the whole log again, in new blocks, as a log tree
//! per subvolume and the log root tree; the disk's cache is flushed, and
//! then the primary superblock, with `log_root` naming the new log, is
//! written through, as Linux writes it. The log's blocks are in no tree,
//! so they are taken from the free space without a change the free-space
//! tree records; those of the log before are free again once the
//! superblock names the new one. From the first log on, data the
//! transaction wrote and frees again waits for the commit, as all freed
//! data does: a log on the disk may still name it.

use radix_vfs::mem::{self, KVec};
use radix_vfs::{Error, Kind, Result};

use crate::chunk::kind;
use crate::cow::Writer;
use crate::extent;
use crate::files::{FILE_EXTENT, FileExtent, extent_type};
use crate::format::{Key, RootItem, item, tree, u64_at};
use crate::inode::Inode;
use crate::space::Space;
use crate::tree::{Block, Bounds, Cursor, HEADER_SIZE, ITEM_SIZE, Root, read_block};
use crate::volume::Volume;

/// The objectid of the log root tree's root items, and the owner of every
/// log tree's blocks (`BTRFS_TREE_LOG_OBJECTID`).
pub(crate) const TREE_LOG: u64 = (-6i64) as u64;

/// The most bytes the log's items may take, with their headers: what four
/// leaves of 16 KiB hold. A larger log commits instead, as each fsync
/// writes the log whole.
pub(crate) const MAX_ITEM_BYTES: usize = 4 * (16384 - HEADER_SIZE);

/// The most ranges of files the transaction changed that are kept; past
/// them, every fsync commits until the next commit.
const MAX_RANGES: usize = 4096;

/// The size of a root item as Linux writes it (`struct btrfs_root_item`).
const ROOT_ITEM_SIZE: usize = 439;

/// Where the superblock keeps the fields an fsync reads or changes.
mod at {
    pub const GENERATION: usize = 0x48;
    pub const ROOT: usize = 0x50;
    pub const CHUNK_ROOT: usize = 0x58;
    pub const LOG_ROOT: usize = 0x60;
    pub const INCOMPAT: usize = 0xbc;
    pub const ROOT_LEVEL: usize = 0xc6;
    pub const LOG_ROOT_LEVEL: usize = 0xc8;
}

/// A byte range a transaction changed of a file: its subvolume, its
/// inode, and the range's start and end.
type Range = (u64, u64, u64, u64);

/// Items of a tree, each its key and its data, in order.
type Items = KVec<(Key, KVec<u8>)>;

/// A file the log holds.
struct Logged {
    subvol: u64,
    ino: u64,
    /// Its items, in order: its inode item, names and extended attributes,
    /// then its extents in the ranges the transaction changed.
    items: Items,
    /// The checksums of the data the transaction wrote that those extents
    /// name: where each run starts, and its checksums.
    sums: KVec<(u64, KVec<u8>)>,
}

impl Logged {
    /// The bytes its items take in a leaf, with their headers.
    fn bytes(&self, per_item: usize, size: usize) -> usize {
        let items: usize = self.items.iter().map(|(_, data)| ITEM_SIZE + data.len()).sum();
        let sums: usize = self
            .sums
            .iter()
            .map(|(_, s)| s.len() + s.len().div_ceil(per_item * size) * ITEM_SIZE)
            .sum();
        items + sums
    }

    fn same(&self, other: &Logged) -> bool {
        self.items == other.items && self.sums == other.sums
    }
}

/// What the running transaction keeps for the log of fsyncs.
pub(crate) struct Logging {
    /// The ranges of files the transaction changed, in order; those of a
    /// file apart.
    changed: KVec<Range>,
    /// Whether ranges were lost, for want of room: the transaction's
    /// fsyncs commit.
    overflow: bool,
    /// The files the log holds.
    files: KVec<Logged>,
    /// The blocks of the log the superblock names, kept from use.
    blocks: KVec<u64>,
}

impl Logging {
    pub(crate) fn new() -> Logging {
        Logging { changed: mem::vec(), overflow: false, files: mem::vec(), blocks: mem::vec() }
    }

    /// Notes that the transaction changed file `ino` of subvolume `subvol`
    /// from byte `start` to `end`.
    pub(crate) fn note(&mut self, subvol: u64, ino: u64, start: u64, end: u64) {
        if self.overflow || start >= end {
            return;
        }
        let ranges = &mut self.changed;
        // The file's first range that ends at `start` or after it, and
        // those after it that the new one touches.
        let first = ranges.partition_point(|r| (r.0, r.1, r.3) < (subvol, ino, start));
        let (mut s, mut e, mut last) = (start, end, first);
        while let Some(r) = ranges.get(last)
            && (r.0, r.1) == (subvol, ino)
            && r.2 <= end
        {
            (s, e, last) = (s.min(r.2), e.max(r.3), last + 1);
        }
        if last > first {
            ranges[first] = (subvol, ino, s, e);
            ranges.copy_within(last.., first + 1);
            ranges.truncate(ranges.len() - (last - first - 1));
        } else if ranges.len() >= MAX_RANGES
            || mem::insert(ranges, first, (subvol, ino, s, e)).is_err()
        {
            self.overflow = true;
        }
    }

    /// The ranges the transaction changed of file `ino` of `subvol`.
    fn changed_of(&self, subvol: u64, ino: u64) -> &[Range] {
        let from = self.changed.partition_point(|r| (r.0, r.1) < (subvol, ino));
        let to = self.changed.partition_point(|r| (r.0, r.1) <= (subvol, ino));
        &self.changed[from..to]
    }

    /// Whether a log was written in the transaction: the disk may name it.
    pub(crate) fn written(&self) -> bool {
        !self.blocks.is_empty()
    }

    /// Forgets the log, whose transaction is committed: its blocks are
    /// free again.
    pub(crate) fn committed(&mut self, space: &mut Space, nodesize: u64) -> Result<()> {
        self.changed.clear();
        self.overflow = false;
        self.files.clear();
        for bytenr in core::mem::replace(&mut self.blocks, mem::vec()) {
            space.release(bytenr, nodesize)?;
        }
        Ok(())
    }
}

impl Writer {
    /// Makes file `ino` of subvolume `subvol` durable as the running
    /// transaction has it, with the log of fsyncs. Returns false, having
    /// written nothing, when a commit must make it durable instead; fails
    /// with [`Error::NoSpace`] when the log has no room, which a commit
    /// may make.
    #[inline(never)]
    pub(crate) fn log_file(&mut self, volume: &Volume, subvol: u64, ino: u64) -> Result<bool> {
        self.check_whole()?;
        if !self.is_dirty() {
            return Ok(true);
        }
        let committed = volume.superblock_bytes()?;
        if self.logging.overflow
            || u64_at(&committed, at::CHUNK_ROOT) != volume.chunk_root().bytenr
            || self.incompat & !u64_at(&committed, at::INCOMPAT) != 0
        {
            return Ok(false);
        }
        let Some(file) = self.gather(volume, &committed, subvol, ino)? else { return Ok(false) };
        let files = &mut self.logging.files;
        match files.iter().position(|f| (f.subvol, f.ino) == (subvol, ino)) {
            // The disk has it so already.
            Some(at) if files[at].same(&file) => return Ok(true),
            Some(at) => files[at] = file,
            None => mem::push(files, file)?,
        }
        let sb = volume.superblock();
        let size = sb.csum_type.size();
        let per_item = sums_per_item(sb.nodesize, size);
        let bytes: usize = self.logging.files.iter().map(|f| f.bytes(per_item, size)).sum();
        if bytes > MAX_ITEM_BYTES {
            return Ok(false);
        }
        self.write_log(volume, &committed)?;
        Ok(true)
    }

    /// What the log is to hold of file `ino` of `subvol`; `None` if a
    /// commit must make it durable.
    #[inline(never)]
    fn gather(
        &self,
        volume: &Volume,
        committed: &[u8],
        subvol: u64,
        ino: u64,
    ) -> Result<Option<Logged>> {
        let now = Writer::tree_root(volume, subvol)?;
        let Some(then) = committed_tree(volume, committed, subvol)? else { return Ok(None) };
        let Some(items) = inode_items(volume, &now, ino)? else { return Ok(None) };
        let Some(old) = inode_items(volume, &then, ino)? else { return Ok(None) };
        let inode = match items.first() {
            Some((key, data)) if key.is(ino, item::INODE_ITEM) => Inode::read(data)?,
            _ => return Ok(None),
        };
        fn names(items: &[(Key, KVec<u8>)]) -> impl Iterator<Item = &(Key, KVec<u8>)> {
            items.iter().filter(|(k, _)| k.kind == item::INODE_REF || k.kind == item::INODE_EXTREF)
        }
        let known = |k: &Key| {
            [item::INODE_ITEM, item::INODE_REF, item::INODE_EXTREF, item::XATTR_ITEM]
                .contains(&k.kind)
        };
        if inode.kind()? != Kind::File
            || inode.nlink == 0
            || !old.first().is_some_and(|(k, _)| k.is(ino, item::INODE_ITEM))
            || !names(&items).eq(names(&old))
            || !items.iter().all(|(k, _)| known(k))
        {
            return Ok(None);
        }
        let mut file = Logged { subvol, ino, items, sums: mem::vec() };
        let sector = u64::from(volume.superblock().sectorsize);
        let size = inode.size.next_multiple_of(sector);
        for &(_, _, start, end) in self.logging.changed_of(subvol, ino) {
            if !self.extents_in(volume, &now, &mut file, (start, end.min(size)), sector)? {
                return Ok(None);
            }
        }
        if inode.has_sums() && !self.sums_of(volume, &mut file)? {
            return Ok(None);
        }
        Ok(Some(file))
    }

    /// Adds the extents of `file` in `[start, end)` to its items, and a
    /// hole item for each part of it no extent covers; false if they would
    /// make the log too large.
    #[inline(never)]
    fn extents_in(
        &self,
        volume: &Volume,
        now: &Root,
        file: &mut Logged,
        (start, end): (u64, u64),
        sector: u64,
    ) -> Result<bool> {
        let ino = file.ino;
        let at = Key::new(ino, item::EXTENT_DATA, start);
        let mut cursor = match Cursor::seek_last(volume, now, &at)? {
            Some(cursor) => cursor,
            None => Cursor::seek(volume, now, &at)?,
        };
        let mut pos = start;
        while pos < end
            && let Some((key, data)) = cursor.get()
            && key <= Key::new(ino, item::EXTENT_DATA, end - 1)
        {
            // The item before the file's first extent may be another's.
            if !key.is(ino, item::EXTENT_DATA) {
                cursor.advance()?;
                continue;
            }
            let extent = FileExtent::read(data)?;
            let stop = key.offset.checked_add(extent.len).ok_or(Error::Corrupt)?;
            let stop = stop.next_multiple_of(sector);
            if stop > pos {
                if key.offset > pos {
                    mem::push(&mut file.items, hole(ino, self.generation, pos, key.offset - pos)?)?;
                }
                // An extent across two ranges is logged once.
                if file.items.last().is_none_or(|(k, _)| *k != key) {
                    mem::push(&mut file.items, (key, mem::copy(data)?))?;
                }
                pos = stop;
            }
            if file.items.len() * ITEM_SIZE > MAX_ITEM_BYTES {
                return Ok(false);
            }
            cursor.advance()?;
        }
        if pos < end {
            mem::push(&mut file.items, hole(ino, self.generation, pos, end - pos)?)?;
        }
        Ok(true)
    }

    /// Adds the checksums of the data the transaction wrote that the
    /// extents of `file` name, as Linux's replay looks them up: the whole
    /// extent if it is compressed, the part the item names if not; false
    /// if some sector of it has none.
    #[inline(never)]
    fn sums_of(&self, volume: &Volume, file: &mut Logged) -> Result<bool> {
        let csums = Writer::tree_root(volume, tree::CSUM)?;
        let sector = u64::from(volume.superblock().sectorsize);
        let size = volume.superblock().csum_type.size();
        for (key, data) in &file.items {
            if key.kind != item::EXTENT_DATA || u64_at(data, 0) != self.generation {
                continue;
            }
            let extent = FileExtent::read(data)?;
            if extent.kind != extent_type::REGULAR || extent.disk_bytenr == 0 {
                continue;
            }
            let (from, len) = if data[16] != 0 {
                (extent.disk_bytenr, extent.disk_num_bytes)
            } else {
                (extent.disk_bytenr + extent.offset, extent.len)
            };
            let count = usize::try_from(len / sector).map_err(|_| Error::Corrupt)?;
            let mut sums = crate::zeroed(count * size)?;
            let mut whole = true;
            extent::each_sum(volume, &csums, from, count, &mut |n, sum| match sum {
                Some(sum) => sums[n * size..(n + 1) * size].copy_from_slice(sum),
                None => whole = false,
            })?;
            if !whole {
                return Ok(false);
            }
            mem::push(&mut file.sums, (from, sums))?;
        }
        Ok(true)
    }

    /// Writes the log whole, in new blocks, and the primary superblock
    /// `committed` naming it; the blocks of the log before are free again.
    #[inline(never)]
    fn write_log(&mut self, volume: &Volume, committed: &[u8]) -> Result<()> {
        let like = read_block(volume, &volume.root_tree(), &Bounds::default())?;
        let mut header = [0; HEADER_SIZE];
        header.copy_from_slice(&like.bytes()[..HEADER_SIZE]);
        drop(like);
        let mut made = mem::vec();
        let written = self.write_trees(volume, &header, &mut made).and_then(|root| {
            let mut sb = mem::copy(committed)?;
            sb[at::LOG_ROOT..at::LOG_ROOT + 8].copy_from_slice(&root.bytenr.to_le_bytes());
            sb[at::LOG_ROOT_LEVEL] = root.level;
            // Each disk's, after the log's blocks are durable on every
            // disk.
            Writer::write_superblocks(volume, &sb, false)
        });
        let nodesize = u64::from(volume.superblock().nodesize);
        match written {
            Ok(()) => {
                for bytenr in core::mem::replace(&mut self.logging.blocks, made) {
                    self.space.release(bytenr, nodesize)?;
                }
                Ok(())
            }
            Err(e) => {
                // The disk may name either log: both wait for the commit.
                let blocks = &mut self.logging.blocks;
                blocks.try_reserve(made.len()).map_err(|_| Error::NoMemory)?;
                for bytenr in made {
                    let _ = blocks.push_within_capacity(bytenr);
                }
                Err(e)
            }
        }
    }

    /// Writes each subvolume's log tree and the log root tree that names
    /// them, noting their blocks in `made`; returns the log root tree's
    /// root.
    fn write_trees(
        &mut self,
        volume: &Volume,
        header: &[u8; HEADER_SIZE],
        made: &mut KVec<u64>,
    ) -> Result<Root> {
        let Writer { space, logging, generation, .. } = self;
        let mut out = Out { volume, space, header, generation: *generation, made };
        let mut subvols: KVec<u64> = mem::vec();
        for file in &logging.files {
            if !subvols.contains(&file.subvol) {
                mem::push(&mut subvols, file.subvol)?;
            }
        }
        subvols.sort_unstable();
        let mut roots = mem::vec();
        for &subvol in &subvols {
            let items = log_items(volume, &logging.files, subvol)?;
            let root = out.tree(items.iter().map(|(k, d)| (k, d.as_slice())))?;
            let key = Key::new(TREE_LOG, item::ROOT_ITEM, subvol);
            mem::push(&mut roots, (key, root_item(&root, volume.superblock().nodesize)?))?;
        }
        out.tree(roots.iter().map(|(k, d)| (k, d.as_slice())))
    }
}

/// How many checksums one item holds, as [`crate::files::Files::add_sums`]
/// makes them.
fn sums_per_item(nodesize: u32, size: usize) -> usize {
    (Block::max_item(nodesize) - ITEM_SIZE) / size - 1
}

/// Subvolume `subvol`'s tree as the commit `committed` has it; `None` if
/// it has no such subvolume.
fn committed_tree(volume: &Volume, committed: &[u8], subvol: u64) -> Result<Option<Root>> {
    let root_tree = Root {
        bytenr: u64_at(committed, at::ROOT),
        level: committed[at::ROOT_LEVEL],
        generation: u64_at(committed, at::GENERATION),
    };
    let cursor = Cursor::seek(volume, &root_tree, &Key::first(subvol, item::ROOT_ITEM))?;
    let Some((key, data)) = cursor.get() else { return Ok(None) };
    if !key.is(subvol, item::ROOT_ITEM) {
        return Ok(None);
    }
    let item = RootItem::read(data).ok_or(Error::Corrupt)?;
    Ok(Some(Root { bytenr: item.bytenr, level: item.level, generation: item.generation }))
}

/// Inode `ino`'s items of the tree `root` before its extents, in order;
/// `None` if they would make the log too large.
fn inode_items(volume: &Volume, root: &Root, ino: u64) -> Result<Option<Items>> {
    let mut items = mem::vec();
    let mut bytes = 0;
    let mut cursor = Cursor::seek(volume, root, &Key::new(ino, 0, 0))?;
    while let Some((key, data)) = cursor.get()
        && key.objectid == ino
        && key.kind < item::EXTENT_DATA
    {
        bytes += ITEM_SIZE + data.len();
        if bytes > MAX_ITEM_BYTES {
            return Ok(None);
        }
        mem::push(&mut items, (key, mem::copy(data)?))?;
        cursor.advance()?;
    }
    Ok(Some(items))
}

/// A hole item of file `ino` from byte `pos` on, `len` bytes long, of
/// transaction `generation`.
fn hole(ino: u64, generation: u64, pos: u64, len: u64) -> Result<(Key, KVec<u8>)> {
    let mut data = crate::zeroed(FILE_EXTENT)?;
    data[0..8].copy_from_slice(&generation.to_le_bytes());
    data[8..16].copy_from_slice(&len.to_le_bytes());
    data[20] = extent_type::REGULAR;
    data[45..53].copy_from_slice(&len.to_le_bytes());
    Ok((Key::new(ino, item::EXTENT_DATA, pos), data))
}

/// The items of subvolume `subvol`'s log tree, in order: those of its
/// files, then their checksums, the runs that meet joined, in items no
/// larger than the checksum tree's.
fn log_items(volume: &Volume, files: &[Logged], subvol: u64) -> Result<Items> {
    let sb = volume.superblock();
    let (size, sector) = (sb.csum_type.size(), u64::from(sb.sectorsize));
    let mut all: KVec<(Key, &[u8])> = mem::vec();
    let mut runs: KVec<(u64, &[u8])> = mem::vec();
    for file in files.iter().filter(|f| f.subvol == subvol) {
        for (key, data) in &file.items {
            mem::push(&mut all, (*key, data.as_slice()))?;
        }
        for (start, sums) in &file.sums {
            mem::push(&mut runs, (*start, sums.as_slice()))?;
        }
    }
    // Sorted by their places in `all`: a sort of larger entries takes a
    // frame over the budget.
    let mut order: KVec<u32> = mem::vec();
    for n in 0..all.len() as u32 {
        mem::push(&mut order, n)?;
    }
    order.sort_unstable_by_key(|&n| all[n as usize].0);
    runs.sort_unstable_by_key(|run| run.0);
    let mut items = mem::vec();
    for n in order {
        let (key, data) = all[n as usize];
        mem::push(&mut items, (key, mem::copy(data)?))?;
    }
    let per_item = sums_per_item(sb.nodesize, size);
    let mut joined: KVec<(u64, KVec<u8>)> = mem::vec();
    for (start, sums) in runs {
        let end = start + (sums.len() / size) as u64 * sector;
        match joined.last_mut() {
            Some((at, have)) if *at + (have.len() / size) as u64 * sector >= start => {
                let have_end = *at + (have.len() / size) as u64 * sector;
                if end > have_end {
                    let skip = ((have_end - start) / sector) as usize * size;
                    mem::extend(have, &sums[skip..])?;
                }
            }
            _ => mem::push(&mut joined, (start, mem::copy(sums)?))?,
        }
    }
    for (start, sums) in &joined {
        for (n, chunk) in sums.chunks(per_item * size).enumerate() {
            let at = start + (n * per_item) as u64 * sector;
            let key = Key::new(tree::EXTENT_CSUM, item::EXTENT_CSUM, at);
            mem::push(&mut items, (key, mem::copy(chunk)?))?;
        }
    }
    Ok(items)
}

/// The root item of a log tree whose root block is `root`, as Linux's
/// `btrfs_add_log_tree` makes it: its inode item a directory's.
fn root_item(root: &Root, nodesize: u32) -> Result<KVec<u8>> {
    let mut data = crate::zeroed(ROOT_ITEM_SIZE)?;
    let put = |data: &mut [u8], at: usize, value: u64| {
        data[at..at + 8].copy_from_slice(&value.to_le_bytes());
    };
    // The inode item: generation 1, size 3, one link, a node's bytes, and
    // the mode of a directory.
    put(&mut data, 0, 1);
    put(&mut data, 16, 3);
    put(&mut data, 24, nodesize.into());
    data[40..44].copy_from_slice(&1u32.to_le_bytes());
    data[52..56].copy_from_slice(&0o40755u32.to_le_bytes());
    // generation, bytenr, level and generation_v2.
    put(&mut data, 160, root.generation);
    put(&mut data, 176, root.bytenr);
    data[238] = root.level;
    put(&mut data, 239, root.generation);
    Ok(data)
}

/// Where the blocks of the log go.
struct Out<'a> {
    volume: &'a Volume,
    space: &'a mut Space,
    /// The header every block starts from: the filesystem's UUIDs.
    header: &'a [u8; HEADER_SIZE],
    generation: u64,
    /// The blocks made.
    made: &'a mut KVec<u64>,
}

impl Out<'_> {
    /// Writes a tree of `items`, in order, and returns its root: leaves
    /// filled in turn, and nodes above them until one holds the rest.
    fn tree<'i>(&mut self, items: impl Iterator<Item = (&'i Key, &'i [u8])>) -> Result<Root> {
        let mut below: KVec<(Key, u64)> = mem::vec();
        let mut leaf = self.block(0)?;
        for (key, data) in items {
            if leaf.leaf_free() < ITEM_SIZE + data.len() && leaf.nritems() > 0 {
                let first = leaf.key(0);
                mem::push(&mut below, (first, self.write(leaf)?))?;
                leaf = self.block(0)?;
            }
            if leaf.leaf_free() < ITEM_SIZE + data.len() {
                return Err(Error::Corrupt);
            }
            let slot = leaf.nritems();
            leaf.insert_item(slot, key, data.len()).copy_from_slice(data);
        }
        let first = if leaf.nritems() > 0 { leaf.key(0) } else { Key::new(0, 0, 0) };
        mem::push(&mut below, (first, self.write(leaf)?))?;
        let max = Block::max_ptrs(self.volume.superblock().nodesize);
        let mut level = 0;
        while below.len() > 1 {
            level += 1;
            let mut above = mem::vec();
            for group in below.chunks(max) {
                let mut node = self.block(level)?;
                for (slot, (key, bytenr)) in group.iter().enumerate() {
                    let child =
                        Root { bytenr: *bytenr, level: level - 1, generation: self.generation };
                    node.insert_ptr(slot, key, &child);
                }
                mem::push(&mut above, (group[0].0, self.write(node)?))?;
            }
            below = above;
        }
        Ok(Root { bytenr: below[0].1, level, generation: self.generation })
    }

    /// A new, empty block of the log at `level`, at a place taken for it.
    fn block(&mut self, level: u8) -> Result<Block> {
        let nodesize = self.volume.superblock().nodesize;
        let mut bytes = crate::zeroed(nodesize as usize)?;
        bytes[..HEADER_SIZE].copy_from_slice(self.header);
        self.made.try_reserve(1).map_err(|_| Error::NoMemory)?;
        // A block never crosses a stripe of 64 KiB: it is aligned to its size.
        let bytenr =
            self.space.take_unrecorded(kind::METADATA, nodesize.into(), nodesize.into())?;
        let _ = self.made.push_within_capacity(bytenr);
        let mut block = Block::from_bytes(bytenr, bytes);
        block.set_header(bytenr, self.generation, TREE_LOG, level);
        block.clear();
        Ok(block)
    }

    /// Seals `block` and writes it to each of its copies, in the disk's
    /// cache; returns where it is.
    fn write(&mut self, mut block: Block) -> Result<u64> {
        block.seal(self.volume.superblock().csum_type);
        let bytenr = block.bytenr();
        for site in self.volume.place(bytenr)?.copies() {
            let cache = self.volume.disk(site.disk)?.cache().ok_or(Error::NoDevice)?;
            cache.write(site.at, block.bytes())?;
        }
        Ok(bytenr)
    }
}
