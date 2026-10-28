// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! A file's blocks: which of the disk's blocks hold which of its own.
//!
//! On the disk they are kept in an extent tree: each extent maps up to
//! 32768 of the file's blocks in a row to as many of the disk's in a row
//! (or 32767 marked unwritten, which read as zeros: what `fallocate`
//! leaves). The tree's root is in the inode, with room for four entries;
//! below it are blocks of up to `(block size - 12) / 12` entries each,
//! the leaves holding extents and the levels above them the first block
//! and place of each block below. Older files (ext2 and ext3) map each
//! block on its own instead, through up to three levels of indirect
//! blocks: Radix reads those, and gives a file extents the first time it
//! changes it.
//!
//! In memory a file's extents are a sorted list ([`Extents`]), read from
//! the tree once, and changes edit the list. [`store`] writes the tree
//! anew from it: four extents or fewer go in the inode; more go in leaves
//! filled in order, with as few levels above as fit, which is the
//! smallest tree `e2fsck` would ask for. The tree's blocks are kept from
//! one write to the next, and leaves before the first changed extent are
//! not written again, so appending to a large file writes its last leaf
//! and the levels above it.

use radix_vfs::mem::{self, KVec};
use radix_vfs::{Error, Result};

use crate::crc::crc32c;
use crate::inode::BLOCK_LEN;
use crate::sb::Geometry;
use crate::{le16, le32, put16, put32};

/// The most blocks a written extent maps.
pub const MAX_LEN: u32 = 32768;
/// A tree node's magic number.
pub const MAGIC: u16 = 0xf30a;
/// The most levels a tree has, as on Linux.
pub const MAX_DEPTH: u16 = 5;
/// The entries of the root, in the inode.
const ROOT_ENTRIES: u16 = 4;
/// The bytes of a node's header and of each entry.
const ENTRY: usize = 12;

/// A run of a file's blocks.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Extent {
    /// The file's first block of it.
    pub logical: u32,
    pub len: u32,
    /// The disk's first block of it.
    pub physical: u64,
    /// Whether it reads as zeros.
    pub unwritten: bool,
}

impl Extent {
    /// The file's block after it.
    pub fn end(&self) -> u64 {
        u64::from(self.logical) + u64::from(self.len)
    }

    /// The most blocks an extent maps.
    pub fn limit(unwritten: bool) -> u32 {
        if unwritten { MAX_LEN - 1 } else { MAX_LEN }
    }

    /// Whether `next` carries on from it on the file and the disk, so the
    /// two can be one.
    fn joins(&self, next: &Extent) -> bool {
        self.unwritten == next.unwritten
            && self.end() == u64::from(next.logical)
            && self.physical + u64::from(self.len) == next.physical
            && self.len + next.len <= Extent::limit(self.unwritten)
    }
}

/// What a file's block maps to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Found {
    /// The disk's block `physical`, and the next `len - 1` after it.
    Mapped { physical: u64, len: u32, unwritten: bool },
    /// Nothing, for `len` blocks (to the next extent or the end).
    Hole { len: u64 },
}

/// A file's extents, sorted, none overlapping.
pub struct Extents {
    list: KVec<Extent>,
}

impl Default for Extents {
    fn default() -> Self {
        Extents::new()
    }
}

impl Extents {
    pub fn new() -> Extents {
        Extents { list: mem::vec() }
    }

    pub fn as_slice(&self) -> &[Extent] {
        &self.list
    }

    /// The blocks it maps.
    pub fn blocks(&self) -> u64 {
        self.list.iter().map(|e| u64::from(e.len)).sum()
    }

    /// The index of the first extent that ends after block `l`.
    fn first_after(&self, l: u64) -> usize {
        self.list.partition_point(|e| e.end() <= l)
    }

    /// What block `l` maps to.
    pub fn find(&self, l: u32) -> Found {
        let i = self.first_after(u64::from(l));
        match self.list.get(i) {
            Some(e) if e.logical <= l => Found::Mapped {
                physical: e.physical + u64::from(l - e.logical),
                len: e.len - (l - e.logical),
                unwritten: e.unwritten,
            },
            Some(e) => Found::Hole { len: u64::from(e.logical - l) },
            None => Found::Hole { len: (1 << 32) - u64::from(l) },
        }
    }

    /// Where on the disk block `l` would best go: just after the block
    /// before it, if that is mapped.
    pub fn goal(&self, l: u32) -> Option<u64> {
        let i = self.first_after(u64::from(l));
        let prev = self.list.get(i.checked_sub(1)?)?;
        Some(prev.physical + u64::from(l) - u64::from(prev.logical))
    }

    /// Adds `e` at the end of the list, joined to the last extent where it
    /// can be, as the tree is read in order. It must come after the last.
    pub fn append(&mut self, e: Extent) -> Result<()> {
        if let Some(last) = self.list.last_mut() {
            if u64::from(e.logical) < last.end() {
                return Err(Error::Corrupt);
            }
            if last.joins(&e) {
                last.len += e.len;
                return Ok(());
            }
        }
        Ok(mem::push(&mut self.list, e)?)
    }

    /// Unmaps the file's blocks from `l` to `l + n`, giving each run of
    /// the disk's blocks that held them to `freed`; returns the index of
    /// the first extent changed. It fails only before changing anything.
    pub fn punch(&mut self, l: u64, n: u64, freed: &mut dyn FnMut(u64, u64)) -> Result<usize> {
        let end = l.saturating_add(n);
        let first = self.first_after(l);
        // At most one extent is split in two.
        self.list.try_reserve(1).map_err(|_| Error::NoMemory)?;
        let mut i = first;
        while let Some(&e) = self.list.get(i) {
            if u64::from(e.logical) >= end {
                break;
            }
            let s = l.max(u64::from(e.logical));
            let t = end.min(e.end());
            freed(e.physical + (s - u64::from(e.logical)), t - s);
            if s == u64::from(e.logical) && t == e.end() {
                self.list.remove(i);
            } else if s == u64::from(e.logical) {
                let cut = (t - s) as u32;
                self.list[i] = Extent {
                    logical: t as u32,
                    len: e.len - cut,
                    physical: e.physical + u64::from(cut),
                    ..e
                };
                break;
            } else if t == e.end() {
                self.list[i].len = (s - u64::from(e.logical)) as u32;
                i += 1;
            } else {
                self.list[i].len = (s - u64::from(e.logical)) as u32;
                let right = Extent {
                    logical: t as u32,
                    len: (e.end() - t) as u32,
                    physical: e.physical + (t - u64::from(e.logical)),
                    ..e
                };
                // Room was made above.
                mem::insert(&mut self.list, i + 1, right)?;
                break;
            }
        }
        Ok(first)
    }

    /// Maps the hole `e` covers, joining `e` to its neighbours where it
    /// can; returns the index of the first extent changed.
    pub fn insert(&mut self, e: Extent) -> Result<usize> {
        let i = self.first_after(u64::from(e.logical));
        if self.list.get(i).is_some_and(|next| u64::from(next.logical) < e.end()) {
            return Err(Error::Corrupt);
        }
        if let Some(prev) = i.checked_sub(1).map(|p| self.list[p])
            && prev.joins(&e)
        {
            self.list[i - 1].len += e.len;
            if let Some(next) = self.list.get(i).copied()
                && self.list[i - 1].joins(&next)
            {
                self.list[i - 1].len += next.len;
                self.list.remove(i);
            }
            return Ok(i - 1);
        }
        if let Some(next) = self.list.get_mut(i)
            && e.joins(next)
        {
            *next = Extent { len: e.len + next.len, ..e };
            return Ok(i);
        }
        mem::insert(&mut self.list, i, e)?;
        Ok(i)
    }

    /// Marks the file's blocks from `l` to `l + n`, which one unwritten
    /// extent maps, written; returns the index of the first extent
    /// changed. It fails only before changing anything.
    pub fn set_written(&mut self, l: u32, n: u32) -> Result<usize> {
        let i = self.first_after(u64::from(l));
        let e = *self.list.get(i).ok_or(Error::Corrupt)?;
        if !e.unwritten || e.logical > l || u64::from(l) + u64::from(n) > e.end() {
            return Err(Error::Corrupt);
        }
        // The extent becomes up to three.
        self.list.try_reserve(2).map_err(|_| Error::NoMemory)?;
        self.list.remove(i);
        let start = i.saturating_sub(1);
        let offset = u64::from(l - e.logical);
        if l > e.logical {
            self.insert(Extent { len: l - e.logical, ..e })?;
        }
        self.insert(Extent {
            logical: l,
            len: n,
            physical: e.physical + offset,
            unwritten: false,
        })?;
        let end = l + n;
        if u64::from(end) < e.end() {
            self.insert(Extent {
                logical: end,
                len: (e.end() - u64::from(end)) as u32,
                physical: e.physical + offset + u64::from(n),
                unwritten: true,
            })?;
        }
        Ok(start)
    }
}

/// A tree node's header.
#[derive(Clone, Copy, Debug)]
struct Header {
    entries: u16,
    max: u16,
    depth: u16,
}

fn header(b: &[u8]) -> Result<Header> {
    if le16(b, 0) != MAGIC {
        return Err(Error::Corrupt);
    }
    let h = Header { entries: le16(b, 2), max: le16(b, 4), depth: le16(b, 6) };
    if h.entries > h.max || h.depth > MAX_DEPTH {
        return Err(Error::Corrupt);
    }
    Ok(h)
}

fn put_header(b: &mut [u8], h: Header) {
    put16(b, 0, MAGIC);
    put16(b, 2, h.entries);
    put16(b, 4, h.max);
    put16(b, 6, h.depth);
    put32(b, 8, 0);
}

/// The entries a tree block of `len` bytes holds.
pub fn capacity(len: usize) -> u16 {
    ((len - ENTRY) / ENTRY) as u16
}

/// Where a tree block's checksum is.
fn tail_at(max: u16) -> usize {
    ENTRY + ENTRY * usize::from(max)
}

/// Leaf entry `k` of node `b`.
fn leaf_entry(b: &[u8], k: usize) -> Extent {
    let at = ENTRY + ENTRY * k;
    let raw_len = u32::from(le16(b, at + 4));
    let unwritten = raw_len > MAX_LEN;
    Extent {
        logical: le32(b, at),
        len: if unwritten { raw_len - MAX_LEN } else { raw_len },
        physical: (u64::from(le16(b, at + 6)) << 32) | u64::from(le32(b, at + 8)),
        unwritten,
    }
}

fn put_leaf_entry(b: &mut [u8], k: usize, e: &Extent) {
    let at = ENTRY + ENTRY * k;
    put32(b, at, e.logical);
    put16(b, at + 4, (if e.unwritten { e.len + MAX_LEN } else { e.len }) as u16);
    put16(b, at + 6, (e.physical >> 32) as u16);
    put32(b, at + 8, e.physical as u32);
}

/// Index entry `k` of node `b`: the first block it covers, and where.
fn index_entry(b: &[u8], k: usize) -> (u32, u64) {
    let at = ENTRY + ENTRY * k;
    (le32(b, at), u64::from(le32(b, at + 4)) | (u64::from(le16(b, at + 8)) << 32))
}

fn put_index_entry(b: &mut [u8], k: usize, logical: u32, block: u64) {
    let at = ENTRY + ENTRY * k;
    put32(b, at, logical);
    put32(b, at + 4, block as u32);
    put16(b, at + 8, (block >> 32) as u16);
    put16(b, at + 10, 0);
}

/// An empty tree's root.
pub fn empty_root() -> [u8; BLOCK_LEN] {
    let mut root = [0; BLOCK_LEN];
    put_header(&mut root, Header { entries: 0, max: ROOT_ENTRIES, depth: 0 });
    root
}

/// What reading a tree needs: the volume, the inode's checksum seed, a
/// way to read a block, and a block's room.
pub struct Reader<'a> {
    pub geo: &'a Geometry,
    pub seed: u32,
    pub read: &'a mut dyn FnMut(u64, &mut [u8]) -> Result<()>,
    pub buf: &'a mut [u8],
}

impl Reader<'_> {
    /// Reads the tree whose root is `root` into `extents`, and the blocks
    /// it is kept in into `tree`.
    pub fn load(&mut self, root: &[u8], extents: &mut Extents, tree: &mut KVec<u64>) -> Result<()> {
        let h = header(root)?;
        if h.max > ROOT_ENTRIES {
            return Err(Error::Corrupt);
        }
        self.node(root, h, extents, tree)
    }

    fn node(
        &mut self,
        b: &[u8],
        h: Header,
        extents: &mut Extents,
        tree: &mut KVec<u64>,
    ) -> Result<()> {
        if h.depth == 0 {
            for k in 0..usize::from(h.entries) {
                let e = leaf_entry(b, k);
                if e.len == 0
                    || e.physical < self.geo.first_data_block
                    || e.physical
                        .checked_add(u64::from(e.len))
                        .is_none_or(|end| end > self.geo.blocks)
                {
                    return Err(Error::Corrupt);
                }
                extents.append(e)?;
            }
            return Ok(());
        }
        // The node's entries are kept while the levels below are read.
        let mut children = mem::vec_with_capacity(usize::from(h.entries))?;
        for k in 0..usize::from(h.entries) {
            let _ = children.push_within_capacity(index_entry(b, k).1);
        }
        for child in children {
            if child < self.geo.first_data_block || child >= self.geo.blocks {
                return Err(Error::Corrupt);
            }
            mem::push(tree, child)?;
            (self.read)(child, self.buf)?;
            let ch = header(self.buf)?;
            let room = capacity(self.buf.len());
            if ch.depth + 1 != h.depth || ch.max == 0 || ch.max > room {
                return Err(Error::Corrupt);
            }
            if self.geo.metadata_csum() {
                let tail = tail_at(ch.max);
                if tail + 4 > self.buf.len()
                    || crc32c(self.seed, &self.buf[..tail]) != le32(self.buf, tail)
                {
                    return Err(Error::Corrupt);
                }
            }
            // Read the child's entries out before the room is reused.
            let len = tail_at(ch.entries);
            let mut copy = mem::vec_with_capacity(len)?;
            mem::extend(&mut copy, &self.buf[..len])?;
            self.node(&copy, ch, extents, tree)?;
        }
        Ok(())
    }
}

/// Reads block pointers from a block: as many as the slice holds, from
/// the given one.
pub type ReadPointers<'a> = dyn FnMut(u64, usize, &mut [u32]) -> Result<()> + 'a;

/// Reads an old block map, rooted at `root`, of a file of `blocks` blocks
/// into `extents`, and its indirect blocks into `tree`.
pub fn load_map(
    geo: &Geometry,
    root: &[u8],
    blocks: u64,
    read: &mut ReadPointers<'_>,
    extents: &mut Extents,
    tree: &mut KVec<u64>,
) -> Result<()> {
    let per = u64::from(geo.block_size / 4);
    let mut l = 0u64;
    for k in 0..12 {
        if l >= blocks {
            return Ok(());
        }
        map_one(geo, l, u64::from(le32(root, k * 4)), extents)?;
        l += 1;
    }
    for level in 1..=3u32 {
        let span = per.pow(level);
        if l >= blocks {
            return Ok(());
        }
        let ptr = u64::from(le32(root, (11 + level as usize) * 4));
        if ptr != 0 {
            walk_map(geo, ptr, level, l, blocks, read, extents, tree)?;
        }
        l += span;
    }
    Ok(())
}

fn map_one(geo: &Geometry, l: u64, physical: u64, extents: &mut Extents) -> Result<()> {
    if physical == 0 {
        return Ok(());
    }
    if physical >= geo.blocks || l >= 1 << 32 {
        return Err(Error::Corrupt);
    }
    extents.append(Extent { logical: l as u32, len: 1, physical, unwritten: false })
}

#[allow(clippy::too_many_arguments)]
fn walk_map(
    geo: &Geometry,
    block: u64,
    level: u32,
    first: u64,
    blocks: u64,
    read: &mut ReadPointers<'_>,
    extents: &mut Extents,
    tree: &mut KVec<u64>,
) -> Result<()> {
    if block < geo.first_data_block || block >= geo.blocks {
        return Err(Error::Corrupt);
    }
    mem::push(tree, block)?;
    let per = (geo.block_size / 4) as usize;
    let span = (per as u64).pow(level - 1);
    let mut ptrs = [0u32; 128];
    let mut k = 0;
    while k < per {
        let n = (per - k).min(ptrs.len());
        read(block, k, &mut ptrs[..n])?;
        for (j, &ptr) in ptrs[..n].iter().enumerate() {
            let l = first + (k + j) as u64 * span;
            if l >= blocks {
                return Ok(());
            }
            if level == 1 {
                map_one(geo, l, u64::from(ptr), extents)?;
            } else if ptr != 0 {
                walk_map(geo, u64::from(ptr), level - 1, l, blocks, read, extents, tree)?;
            }
        }
        k += n;
    }
    Ok(())
}

/// What writing a tree does with the volume: takes a block for it, gives
/// one back, writes one.
pub trait TreeIo {
    fn take(&mut self) -> Result<u64>;
    fn give(&mut self, block: u64) -> Result<()>;
    fn write(&mut self, block: u64, data: &[u8]) -> Result<()>;
}

/// What writing a tree needs: the volume, the inode's checksum seed, the
/// volume's side of it, and a block's room.
pub struct Writer<'a> {
    pub geo: &'a Geometry,
    pub seed: u32,
    pub io: &'a mut dyn TreeIo,
    pub buf: &'a mut [u8],
}

/// How many blocks each level of a tree of `n` extents has, from the
/// leaves up, in `levels`; returns how many levels.
fn shape(n: usize, per: usize, levels: &mut [usize; MAX_DEPTH as usize]) -> Result<usize> {
    if n <= usize::from(ROOT_ENTRIES) {
        return Ok(0);
    }
    let mut count = n.div_ceil(per);
    let mut depth = 0;
    loop {
        if depth == levels.len() {
            return Err(Error::TooBig);
        }
        levels[depth] = count;
        depth += 1;
        if count <= usize::from(ROOT_ENTRIES) {
            return Ok(depth);
        }
        count = count.div_ceil(per);
    }
}

impl Writer<'_> {
    /// Writes the tree of `extents` into `root` and blocks, reusing those
    /// in `tree` (leaves first, then each level above), which it leaves
    /// holding the new tree's; returns how many leaves it has. Leaves
    /// before the one holding extent `from`, of the `old_leaves` the tree
    /// had, hold what they held, and are not written. On an error the
    /// caller gives up the volume's changes (the tree may be half
    /// written).
    pub fn store(
        &mut self,
        extents: &[Extent],
        from: usize,
        old_leaves: usize,
        root: &mut [u8],
        tree: &mut KVec<u64>,
    ) -> Result<usize> {
        let per = usize::from(capacity(self.buf.len()));
        let mut levels = [0; MAX_DEPTH as usize];
        let depth = shape(extents.len(), per, &mut levels)?;
        let total: usize = levels[..depth].iter().sum();
        while tree.len() > total {
            if let Some(block) = tree.pop() {
                self.io.give(block)?;
            }
        }
        while tree.len() < total {
            let block = self.io.take()?;
            mem::push(tree, block)?;
        }
        if depth == 0 {
            root.fill(0);
            put_header(root, Header { entries: extents.len() as u16, max: ROOT_ENTRIES, depth: 0 });
            for (k, e) in extents.iter().enumerate() {
                put_leaf_entry(root, k, e);
            }
            return Ok(0);
        }
        // The leaves.
        let leaves = levels[0];
        // Those at blocks that held something else are written too.
        for leaf in (from / per).min(old_leaves).min(leaves)..leaves {
            let part = &extents[leaf * per..((leaf + 1) * per).min(extents.len())];
            self.buf.fill(0);
            put_header(self.buf, Header { entries: part.len() as u16, max: per as u16, depth: 0 });
            for (k, e) in part.iter().enumerate() {
                put_leaf_entry(self.buf, k, e);
            }
            self.seal(per as u16)?;
            self.io.write(tree[leaf], self.buf)?;
        }
        // The levels above, each entry the first block of the one below.
        let mut below = 0;
        let mut span = per;
        for level in 1..depth {
            let start = below + levels[level - 1];
            for j in 0..levels[level] {
                let children = levels[level - 1] - j * per;
                let n = children.min(per);
                self.buf.fill(0);
                put_header(
                    self.buf,
                    Header { entries: n as u16, max: per as u16, depth: level as u16 },
                );
                for k in 0..n {
                    let child = j * per + k;
                    put_index_entry(
                        self.buf,
                        k,
                        extents[child * span].logical,
                        tree[below + child],
                    );
                }
                self.seal(per as u16)?;
                self.io.write(tree[start + j], self.buf)?;
            }
            below = start;
            span *= per;
        }
        root.fill(0);
        let top = levels[depth - 1];
        put_header(root, Header { entries: top as u16, max: ROOT_ENTRIES, depth: depth as u16 });
        for k in 0..top {
            put_index_entry(root, k, extents[k * span].logical, tree[below + k]);
        }
        Ok(leaves)
    }

    fn seal(&mut self, max: u16) -> Result<()> {
        if self.geo.metadata_csum() {
            let tail = tail_at(max);
            if tail + 4 > self.buf.len() {
                return Err(Error::Corrupt);
            }
            let sum = crc32c(self.seed, &self.buf[..tail]);
            put32(self.buf, tail, sum);
        }
        Ok(())
    }
}
