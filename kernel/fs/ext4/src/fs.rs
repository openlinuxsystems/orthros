// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! A mounted ext4 volume.
//!
//! # Transactions
//!
//! Every change to the volume's metadata (bitmaps, descriptors, inodes,
//! extent trees, directories) is made in the block cache, where it stays
//! until the next commit, and the blocks it touched are noted in the
//! running transaction. A commit writes them to the journal with a commit
//! block, then home, then flushes: the volume on the disk goes from one
//! commit's state to the next in one step, whenever the power goes. A
//! commit happens at each sync (the writeback thread's, every five
//! seconds), and sooner when the transaction holds a quarter of what the
//! journal can. Without a journal (`^has_journal`) a commit writes the
//! blocks home alone, as Linux does then, and a power cut can leave the
//! volume for `e2fsck` to mend.
//!
//! File data is written in ordered mode, as Linux's default: a file's
//! blocks are taken when its pages are written back (delayed allocation),
//! and its data written to them before the transaction that maps them
//! commits. So a file never shows another's old data after a power cut.
//! Blocks freed in a transaction are not given out again until it
//! commits: until then the disk may still need what they hold.
//!
//! A writable mount sets `needs_recovery`, as Linux does, and a clean
//! unmount clears it once the journal is empty. A volume that needs
//! recovery is replayed at mount (see [`crate::journal`]): into the block
//! cache and then home on a writable mount; into memory alone on a
//! read-only one, which writes nothing to the disk. A writable mount then
//! frees the inodes left on the orphan list: files removed while open
//! when the power went.
//!
//! # Locks
//!
//! A directory operation takes the namespace lock first (`ns`), which
//! also holds its blocks' room. A file's page cache comes next; then the
//! transaction, held by every change; then a node's own lock; then the
//! node table, the nodes kept for the sync, the inodes to free, and the
//! free space counts; then the block cache. Nothing that holds the
//! transaction takes a page cache. A node whose last reference goes only
//! notes, if the file has no name left, that its inode is to be freed:
//! the next commit frees it.

use alloc::sync::Weak;
use core::fmt;
use core::sync::atomic::{AtomicBool, AtomicU32, Ordering};

use radix_block::cache::BlockCache;
use radix_block::{DeviceRef, SECTOR};
use radix_sync::{Mutex, MutexGuard};
use radix_vfs::mem::{self, Alloc, KArc, KVec};
use radix_vfs::{Error, FileSystem, FsInfo, FsKind, Node, NodeRef, Result};

use crate::extent::{self, Extents};
use crate::group::Group;
use crate::inode::{self, Inode};
use crate::journal::Journal;
use crate::node::Ext4Node;
use crate::sb::{self, Geometry, at, incompat};
use crate::{le32, put32};

/// Where a group's inode table is.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Loc {
    pub(crate) inode_table: u64,
}

/// Room for blocks, under the namespace lock.
pub(crate) struct Scratch {
    pub(crate) a: KVec<u8>,
    pub(crate) b: KVec<u8>,
}

/// The running transaction.
pub(crate) struct Txn {
    /// The metadata blocks it changed, sorted.
    pub(crate) blocks: KVec<u64>,
    /// The groups whose bitmaps changed: 1 the block bitmap, 2 the inode
    /// bitmap. Their checksums are set at the commit.
    pub(crate) groups: KVec<(u32, u8)>,
    /// The blocks it freed, as runs, sorted: not to be taken again until
    /// it commits.
    pub(crate) pending: KVec<(u64, u64)>,
    /// The superblock as the commit writes it.
    pub(crate) sb: KVec<u8>,
    /// The orphan list, from its head: inodes with no name left, freed
    /// when their last reference goes.
    pub(crate) orphans: KVec<u32>,
    /// The journal, if the volume has one.
    pub(crate) journal: Option<Journal>,
    /// A block's room.
    pub(crate) buf: KVec<u8>,
    /// The group the next directory under the root goes to.
    pub(crate) dir_group: u32,
}

impl Txn {
    /// Puts inode `ino` at the head of the orphan list; returns the next
    /// one, which its `dtime` must name.
    pub(crate) fn add_orphan(&mut self, ino: u32) -> Result<u32> {
        let next = self.orphans.first().copied().unwrap_or(0);
        mem::insert(&mut self.orphans, 0, ino)?;
        Ok(next)
    }
}

/// How much is free.
pub(crate) struct Space {
    pub(crate) free_blocks: u64,
    pub(crate) free_inodes: u64,
    /// Blocks promised to pages waiting to be written back.
    pub(crate) reserved: u64,
    /// Blocks freed by the running transaction.
    pub(crate) pending: u64,
}

/// What a volume's nodes share.
pub(crate) struct Inner {
    pub(crate) geo: Geometry,
    pub(crate) cache: BlockCache,
    pub(crate) read_only: bool,
    pub(crate) ns: Mutex<Scratch>,
    pub(crate) txn: Mutex<Txn>,
    pub(crate) space: Mutex<Space>,
    /// The nodes in use, by inode, sorted.
    nodes: Mutex<KVec<(u32, Weak<Ext4Node, Alloc>)>>,
    /// Nodes with changes in memory, kept alive until they are synced.
    dirty: Mutex<KVec<KArc<Ext4Node>>>,
    /// Inodes with no name whose last reference went: freed at the next
    /// commit.
    pub(crate) doomed: Mutex<KVec<u32>>,
    pub(crate) locs: KVec<Loc>,
    /// Blocks a read-only mount replayed from the journal, sorted.
    overlay: KVec<(u64, KVec<u8>)>,
    /// A change failed halfway: nothing more is written.
    broken: AtomicBool,
    generation: AtomicU32,
    /// Blocks kept back from file data for metadata: 2% of the volume, at
    /// most 4096, as Linux keeps (`s_resv_clusters`).
    pub(crate) margin: u64,
    /// `s_state` as it was at mount, which a clean unmount puts back.
    state: u16,
}

/// A mounted ext4 volume.
pub struct Ext4Fs {
    inner: KArc<Inner>,
    root: KArc<Ext4Node>,
}

/// A zeroed buffer of `len` bytes.
pub(crate) fn zeroed(len: usize) -> Result<KVec<u8>> {
    let mut v = mem::vec_with_capacity(len)?;
    for _ in 0..len {
        let _ = v.push_within_capacity(0u8);
    }
    Ok(v)
}

/// Reads `out.len()` bytes from byte `offset`, through `overlay` first.
fn read_through(
    cache: &BlockCache,
    overlay: &[(u64, KVec<u8>)],
    shift: u32,
    offset: u64,
    out: &mut [u8],
) -> Result<()> {
    if overlay.is_empty() {
        return Ok(cache.read(offset, out)?);
    }
    let bs = 1u64 << shift;
    let mut done = 0;
    while done < out.len() {
        let at = offset + done as u64;
        let block = at >> shift;
        let within = (at % bs) as usize;
        let n = (bs as usize - within).min(out.len() - done);
        match overlay.binary_search_by_key(&block, |(b, _)| *b) {
            Ok(i) => out[done..done + n].copy_from_slice(&overlay[i].1[within..within + n]),
            Err(_) => cache.read(at, &mut out[done..done + n])?,
        }
        done += n;
    }
    Ok(())
}

/// Notes block `block` in the sorted `blocks`.
fn note(blocks: &mut KVec<u64>, block: u64) -> Result<()> {
    if let Err(i) = blocks.binary_search(&block) {
        mem::insert(blocks, i, block)?;
    }
    Ok(())
}

/// The nanoseconds since 1970 now, in seconds.
fn now_secs() -> u32 {
    (radix_vfs::now() / 1_000_000_000) as u32
}

impl Ext4Fs {
    /// Mounts the ext4 volume on `device`; `read_only` refuses every
    /// change, as does a read-only device. A volume with features Radix
    /// reads but does not write mounts only read-only: a writable mount
    /// fails with [`Error::NotSupported`].
    pub fn mount(device: DeviceRef, read_only: bool) -> Result<KArc<Ext4Fs>> {
        let info = device.info();
        let bytes = info.sectors * SECTOR;
        let cache = BlockCache::new(device);
        let mut sbuf = zeroed(sb::SIZE)?;
        cache.read(sb::OFFSET, &mut sbuf)?;
        let geo = Geometry::parse(&sbuf, bytes)?;
        // Blocks are read and written whole: a disk with larger sectors
        // cannot do that.
        if geo.block_size < info.logical_block {
            return Err(Error::NotSupported);
        }
        if !read_only && !geo.writable {
            return Err(Error::NotSupported);
        }
        let read_only = read_only || info.read_only;
        let mut overlay = mem::vec();
        let journal = open_journal(&cache, &geo, read_only, &mut overlay)?;
        // The replay may have changed the superblock.
        read_through(&cache, &overlay, geo.shift, sb::OFFSET, &mut sbuf)?;
        let inner = Inner::new(cache, sbuf, bytes, journal, overlay, read_only)?;
        if !read_only {
            Inner::start(&inner)?;
        }
        let root = {
            let mut ns = inner.ns.lock();
            Inner::node_for(&inner, sb::ROOT, &mut ns.a)?
        };
        if root.kind() != radix_vfs::Kind::Directory {
            return Err(Error::Corrupt);
        }
        Ok(mem::arc(Ext4Fs { inner, root })?)
    }

    /// Whether it gave up writing, after a change failed.
    #[cfg(test)]
    pub(crate) fn broken(&self) -> bool {
        self.inner.broken.load(Ordering::Acquire)
    }
}

/// Opens the volume's journal, if it has one in an inode, replaying what
/// it holds into `overlay` (a read-only mount) or to the disk.
#[inline(never)]
fn open_journal(
    cache: &BlockCache,
    geo: &Geometry,
    read_only: bool,
    overlay: &mut KVec<(u64, KVec<u8>)>,
) -> Result<Option<Journal>> {
    if geo.journal_inum == 0 {
        // An external journal Radix cannot replay.
        if geo.has_incompat(incompat::RECOVER) {
            return Err(Error::NotSupported);
        }
        return Ok(None);
    }
    let mut journal = load_journal(cache, geo)?;
    if !read_only && !journal.replayable() {
        return Err(Error::NotSupported);
    }
    if geo.has_incompat(incompat::RECOVER) && journal.needs_replay() {
        if !journal.replayable() {
            return Err(Error::NotSupported);
        }
        replay(cache, geo, &mut journal, read_only, overlay)?;
    }
    Ok(Some(journal))
}

/// Reads the journal's inode and superblock.
#[inline(never)]
fn load_journal(cache: &BlockCache, geo: &Geometry) -> Result<Journal> {
    let ino = geo.journal_inum;
    if ino == 0 || ino > geo.inodes {
        return Err(Error::Corrupt);
    }
    let g = (ino - 1) / geo.inodes_per_group;
    let mut desc = [0u8; 64];
    let n = geo.desc_size as usize;
    cache.read(geo.desc_offset(g), &mut desc[..n])?;
    let group = Group::parse(geo, g, &desc[..n])?;
    let at = geo.offset(group.inode_table)
        + u64::from((ino - 1) % geo.inodes_per_group) * u64::from(geo.inode_size);
    let mut raw = zeroed(geo.inode_size as usize)?;
    cache.read(at, &mut raw)?;
    let inode = Inode::parse(geo, ino, &raw)?;
    let mut map = Extents::new();
    let mut tree = mem::vec();
    let mut buf = zeroed(geo.block_size as usize)?;
    let shift = geo.shift;
    if inode.has_extents() {
        let mut read = |block: u64, out: &mut [u8]| Ok(cache.read(block << shift, out)?);
        extent::Reader {
            geo,
            seed: inode::seed(geo, ino, inode.generation),
            read: &mut read,
            buf: &mut buf,
        }
        .load(&inode.block, &mut map, &mut tree)?;
    } else {
        let blocks = inode.size.div_ceil(u64::from(geo.block_size));
        let mut read = |block: u64, from: usize, out: &mut [u32]| {
            let mut bytes = [0u8; 512];
            let len = out.len() * 4;
            cache.read((block << shift) + from as u64 * 4, &mut bytes[..len])?;
            for (k, word) in out.iter_mut().enumerate() {
                *word = le32(&bytes, k * 4);
            }
            Ok(())
        };
        extent::load_map(geo, &inode.block, blocks, &mut read, &mut map, &mut tree)?;
    }
    let first = match map.find(0) {
        extent::Found::Mapped { physical, .. } => physical,
        extent::Found::Hole { .. } => return Err(Error::Corrupt),
    };
    let mut jsb = zeroed(crate::journal::SUPER_SIZE)?;
    cache.read(geo.offset(first), &mut jsb)?;
    Journal::new(map, geo.block_size, &jsb)
}

/// Replays `journal`: into the cache and home, or, `read_only`, into
/// `overlay`.
#[inline(never)]
fn replay(
    cache: &BlockCache,
    geo: &Geometry,
    journal: &mut Journal,
    read_only: bool,
    overlay: &mut KVec<(u64, KVec<u8>)>,
) -> Result<()> {
    let shift = geo.shift;
    let blocks = geo.blocks;
    let mut buf = zeroed(geo.block_size as usize)?;
    let mut read = |block: u64, out: &mut [u8]| Ok(cache.read(block << shift, out)?);
    let mut home = |block: u64, data: &[u8]| {
        if block >= blocks {
            return Err(Error::Corrupt);
        }
        if read_only {
            let mut copy = mem::vec_with_capacity(data.len())?;
            mem::extend(&mut copy, data)?;
            match overlay.binary_search_by_key(&block, |(b, _)| *b) {
                Ok(i) => overlay[i].1 = copy,
                Err(i) => mem::insert(overlay, i, (block, copy))?,
            }
            Ok(())
        } else {
            Ok(cache.write(block << shift, data)?)
        }
    };
    journal.replay(&mut buf, &mut read, &mut home)?;
    if !read_only {
        cache.sync()?;
    }
    Ok(())
}

/// Reads every group's descriptor: where its parts are, checked, and the
/// free counts' sums.
#[inline(never)]
fn load_groups(
    cache: &BlockCache,
    overlay: &[(u64, KVec<u8>)],
    geo: &Geometry,
) -> Result<(KVec<Loc>, u64, u64)> {
    let mut locs = mem::vec_with_capacity(geo.groups as usize)?;
    let (mut free_blocks, mut free_inodes) = (0u64, 0u64);
    let table_blocks = (u64::from(geo.inodes_per_group) * u64::from(geo.inode_size))
        .div_ceil(u64::from(geo.block_size));
    let n = geo.desc_size as usize;
    let mut desc = [0u8; 64];
    for g in 0..geo.groups {
        read_through(cache, overlay, geo.shift, geo.desc_offset(g), &mut desc[..n])?;
        let group = Group::parse(geo, g, &desc[..n])?;
        let inside = |b: u64, len: u64| {
            b >= geo.first_data_block && b.checked_add(len).is_some_and(|e| e <= geo.blocks)
        };
        if !inside(group.block_bitmap, 1)
            || !inside(group.inode_bitmap, 1)
            || !inside(group.inode_table, table_blocks)
            || group.free_blocks > geo.group_blocks(g)
            || group.free_inodes > geo.inodes_per_group
        {
            return Err(Error::Corrupt);
        }
        free_blocks += u64::from(group.free_blocks);
        free_inodes += u64::from(group.free_inodes);
        let _ = locs.push_within_capacity(Loc { inode_table: group.inode_table });
    }
    Ok((locs, free_blocks, free_inodes))
}

impl Drop for Ext4Fs {
    fn drop(&mut self) {
        let _ = self.inner.sync_all();
        let _ = self.inner.finish();
        // What could not be written is lost, but the nodes must not keep
        // each other alive.
        let pinned = core::mem::replace(&mut *self.inner.dirty.lock(), mem::vec());
        drop(pinned);
    }
}

impl FileSystem for Ext4Fs {
    fn kind(&self) -> FsKind {
        FsKind::Ext4
    }

    fn root(&self) -> NodeRef {
        self.root.clone()
    }

    fn info(&self) -> FsInfo {
        let geo = &self.inner.geo;
        let space = self.inner.space.lock();
        FsInfo {
            block_size: geo.block_size,
            blocks: geo.blocks,
            free: space.free_blocks,
            files: u64::from(geo.inodes),
            free_files: space.free_inodes,
        }
    }

    fn read_only(&self) -> bool {
        self.inner.read_only
    }

    fn label(&self, out: &mut dyn fmt::Write) -> fmt::Result {
        let label = &self.inner.geo.label;
        let len = label.iter().position(|&b| b == 0).unwrap_or(label.len());
        for c in core::str::from_utf8(&label[..len]).unwrap_or("").chars() {
            out.write_char(c)?;
        }
        Ok(())
    }

    fn sync(&self) -> Result<()> {
        self.inner.sync_all()
    }

    fn is_dirty(&self) -> bool {
        !self.inner.dirty.lock().is_empty()
            || self.inner.cache.is_dirty()
            || !self.inner.doomed.lock().is_empty()
    }

    fn shrink(&self, max: usize) -> usize {
        let mut dropped = self.inner.cache.shrink(max);
        if dropped < max {
            let Ok(live) = self.inner.live_nodes() else { return dropped };
            for node in &live {
                if dropped >= max {
                    break;
                }
                dropped += node.shrink(max - dropped);
            }
        }
        dropped
    }

    fn light_drops(&self) -> bool {
        true
    }
}

impl Inner {
    /// The state of a volume whose superblock `sb` is read and whose
    /// journal is opened, on a device of `bytes`, with the groups' counts
    /// loaded from it.
    #[inline(never)]
    fn new(
        cache: BlockCache,
        sb: KVec<u8>,
        bytes: u64,
        journal: Option<Journal>,
        overlay: KVec<(u64, KVec<u8>)>,
        read_only: bool,
    ) -> Result<KArc<Inner>> {
        let geo = Geometry::parse(&sb, bytes)?;
        let (locs, free_blocks, free_inodes) = load_groups(&cache, &overlay, &geo)?;
        let margin = (geo.blocks / 50).min(4096);
        let state = crate::le16(&sb, at::STATE);
        let bs = geo.block_size as usize;
        Ok(mem::arc(Inner {
            ns: Mutex::new(Scratch { a: zeroed(bs)?, b: zeroed(bs)? }),
            txn: Mutex::new(Txn {
                blocks: mem::vec(),
                groups: mem::vec(),
                pending: mem::vec(),
                sb,
                orphans: mem::vec(),
                journal,
                buf: zeroed(bs)?,
                dir_group: 0,
            }),
            space: Mutex::new(Space { free_blocks, free_inodes, reserved: 0, pending: 0 }),
            nodes: Mutex::new(mem::vec()),
            dirty: Mutex::new(mem::vec()),
            doomed: Mutex::new(mem::vec()),
            locs,
            overlay,
            broken: AtomicBool::new(false),
            generation: AtomicU32::new((radix_vfs::now() / 1000) as u32),
            margin,
            state,
            geo,
            cache,
            read_only,
        })?)
    }

    /// Reads `out.len()` bytes of metadata from byte `offset`.
    pub(crate) fn read(&self, offset: u64, out: &mut [u8]) -> Result<()> {
        read_through(&self.cache, &self.overlay, self.geo.shift, offset, out)
    }

    /// Writes `data` at byte `offset` in the running transaction.
    pub(crate) fn write(&self, txn: &mut Txn, offset: u64, data: &[u8]) -> Result<()> {
        if data.is_empty() {
            return Ok(());
        }
        let first = offset >> self.geo.shift;
        let last = (offset + data.len() as u64 - 1) >> self.geo.shift;
        let mut noted = Ok(());
        for block in first..=last {
            noted = noted.and(note(&mut txn.blocks, block));
        }
        let written = self.cache.write(offset, data).map_err(Error::from).and(noted);
        if written.is_err() {
            self.fail();
        }
        written
    }

    /// Gives up writing: a change failed halfway, and the volume on the
    /// disk is kept as the last commit left it.
    pub(crate) fn fail(&self) {
        self.broken.store(true, Ordering::Release);
    }

    pub(crate) fn writable(&self) -> Result<()> {
        if self.read_only {
            Err(Error::ReadOnly)
        } else if self.broken.load(Ordering::Acquire) {
            Err(Error::Io)
        } else {
            Ok(())
        }
    }

    /// Where inode `ino` is, in bytes.
    pub(crate) fn inode_offset(&self, ino: u32) -> Result<u64> {
        if ino == 0 || ino > self.geo.inodes {
            return Err(Error::Corrupt);
        }
        let ipg = self.geo.inodes_per_group;
        let loc = self.locs.get(((ino - 1) / ipg) as usize).ok_or(Error::Corrupt)?;
        Ok(self.geo.offset(loc.inode_table)
            + u64::from((ino - 1) % ipg) * u64::from(self.geo.inode_size))
    }

    /// A new inode generation.
    pub(crate) fn next_generation(&self) -> u32 {
        self.generation.fetch_add(0x9e37_79b9, Ordering::Relaxed)
    }

    /// The running transaction, for a change: committed first if it holds
    /// a quarter of what the journal can.
    pub(crate) fn begin(&self) -> Result<MutexGuard<'_, Txn>> {
        self.writable()?;
        let mut txn = self.txn.lock();
        self.writable()?;
        if let Some(journal) = &txn.journal
            && txn.blocks.len() > journal.capacity() / 4
        {
            self.commit(&mut txn)?;
        }
        Ok(txn)
    }

    /// Fails with [`Error::NoSpace`] unless `blocks` blocks and `inodes`
    /// inodes are free for metadata, committing first if blocks freed by
    /// the transaction would make the room.
    pub(crate) fn room(&self, txn: &mut Txn, blocks: u64, inodes: u64) -> Result<()> {
        for attempt in 0..2 {
            {
                let s = self.space.lock();
                if s.free_blocks.saturating_sub(s.pending + s.reserved) >= blocks
                    && s.free_inodes >= inodes
                {
                    return Ok(());
                }
                if attempt == 1 || s.pending == 0 || s.free_inodes < inodes {
                    return Err(Error::NoSpace);
                }
            }
            self.commit(txn)?;
        }
        Err(Error::NoSpace)
    }

    /// Promises `blocks` blocks to file data about to be written back
    /// ([`radix_vfs::cache::Backing::reserve_pages`]), keeping back the
    /// margin for metadata.
    pub(crate) fn reserve(&self, blocks: u64) -> Result<()> {
        for attempt in 0..2 {
            {
                let mut s = self.space.lock();
                let free = s.free_blocks.saturating_sub(s.pending + s.reserved + self.margin);
                if free >= blocks {
                    s.reserved += blocks;
                    return Ok(());
                }
                if attempt == 1 || s.pending == 0 {
                    return Err(Error::NoSpace);
                }
            }
            let mut txn = self.begin()?;
            self.commit(&mut txn)?;
        }
        Err(Error::NoSpace)
    }

    /// Gives back blocks [`Inner::reserve`] promised.
    pub(crate) fn release(&self, blocks: u64) {
        let mut s = self.space.lock();
        s.reserved = s.reserved.saturating_sub(blocks);
    }

    /// Sets up a writable mount: the journal empty with the features
    /// Linux would give it, `needs_recovery` set (or, without a journal,
    /// the volume marked not clean), then the orphans freed.
    #[inline(never)]
    fn start(this: &KArc<Inner>) -> Result<()> {
        {
            let mut txn = this.txn.lock();
            let Txn { sb, journal, .. } = &mut *txn;
            if let Some(journal) = journal {
                journal
                    .set_features(this.geo.has_incompat(incompat::BIT64), this.geo.metadata_csum());
                journal.start = 0;
                this.write_journal_super(journal)?;
                sb::set_recover(sb, true);
            } else {
                sb::set_state(sb, this.state & !sb::VALID_FS);
            }
            put32(sb, at::MTIME, now_secs());
            sb::seal(&this.geo, sb);
            this.write_super_now(sb)?;
        }
        Inner::free_orphans(this)
    }

    /// Writes the journal's superblock now, as its whole first block.
    fn write_journal_super(&self, journal: &mut Journal) -> Result<()> {
        let block = journal.physical(0)?;
        let mut buf = zeroed(self.geo.block_size as usize)?;
        let bytes = journal.super_bytes();
        buf[..bytes.len()].copy_from_slice(bytes);
        Ok(self.cache.write_through(self.geo.offset(block), &buf)?)
    }

    /// Writes the superblock `sb` now, with the rest of its block.
    fn write_super_now(&self, sb: &[u8]) -> Result<()> {
        let block = sb::OFFSET >> self.geo.shift;
        let start = self.geo.offset(block);
        let mut buf = zeroed(self.geo.block_size as usize)?;
        self.cache.read(start, &mut buf)?;
        let within = (sb::OFFSET - start) as usize;
        buf[within..within + sb::SIZE].copy_from_slice(sb);
        Ok(self.cache.write_through(start, &buf)?)
    }

    /// After the last sync of a clean unmount: the journal marked empty
    /// and `needs_recovery` cleared, or the volume marked clean.
    fn finish(&self) -> Result<()> {
        if self.writable().is_err() {
            return Ok(());
        }
        let mut txn = self.txn.lock();
        if !txn.blocks.is_empty() {
            return Err(Error::Io);
        }
        let Txn { sb, journal, .. } = &mut *txn;
        if let Some(journal) = journal {
            journal.start = 0;
            self.write_journal_super(journal)?;
            sb::set_recover(sb, false);
        } else {
            sb::set_state(sb, self.state);
        }
        sb::seal(&self.geo, sb);
        self.write_super_now(sb)
    }

    /// Frees the inodes on the orphan list, at a writable mount, as
    /// Linux's `ext4_orphan_cleanup` does: those with no links left go,
    /// and the others are cut to their size.
    #[inline(never)]
    fn free_orphans(this: &KArc<Inner>) -> Result<()> {
        let mut txn = this.begin()?;
        let mut ino = le32(&txn.sb, at::LAST_ORPHAN);
        let mut raw = zeroed(this.geo.inode_size as usize)?;
        let mut list = mem::vec();
        while ino != 0 {
            if list.len() >= this.geo.inodes as usize || list.contains(&ino) {
                return Err(Error::Corrupt);
            }
            mem::push(&mut list, ino)?;
            this.read(this.inode_offset(ino)?, &mut raw)?;
            ino = Inode::parse(&this.geo, ino, &raw)?.dtime;
        }
        txn.orphans = list;
        while let Some(&ino) = txn.orphans.first() {
            this.read(this.inode_offset(ino)?, &mut raw)?;
            let inode = Inode::parse(&this.geo, ino, &raw)?;
            if inode.links == 0 {
                this.free_file(&mut txn, ino)?;
            } else {
                drop(txn);
                let node = {
                    let mut ns = this.ns.lock();
                    Inner::node_for(this, ino, &mut ns.a)?
                };
                txn = this.begin()?;
                node.cut_orphan(&mut txn)?;
                this.unlink_orphan(&mut txn, ino)?;
            }
        }
        this.commit(&mut txn)
    }

    /// Frees inode `ino`, which has no links, and its blocks; it leaves the
    /// orphan list.
    pub(crate) fn free_file(&self, txn: &mut Txn, ino: u32) -> Result<()> {
        let at = self.inode_offset(ino)?;
        let mut raw = zeroed(self.geo.inode_size as usize)?;
        self.read(at, &mut raw)?;
        let mut inode = Inode::parse(&self.geo, ino, &raw)?;
        if inode.links != 0 {
            return self.unlink_orphan(txn, ino);
        }
        let kind = inode.kind()?;
        let has_blocks = matches!(kind, radix_vfs::Kind::File | radix_vfs::Kind::Directory)
            || kind == radix_vfs::Kind::Symlink && !inode.fast_link(&self.geo);
        if has_blocks {
            let mut extents = Extents::new();
            let mut tree = mem::vec();
            self.load_blocks(ino, &inode, &mut txn.buf, &mut extents, &mut tree)?;
            for e in extents.as_slice() {
                self.free_blocks(txn, e.physical, u64::from(e.len))?;
            }
            for &block in &tree {
                self.free_blocks(txn, block, 1)?;
            }
        }
        if inode.file_acl != 0 {
            self.drop_attributes(txn, inode.file_acl)?;
        }
        self.free_inode(txn, ino, kind == radix_vfs::Kind::Directory)?;
        self.unlink_orphan(txn, ino)?;
        inode.size = 0;
        inode.sectors = 0;
        inode.file_acl = 0;
        inode.dtime = now_secs().max(1);
        if inode.has_extents() {
            inode.block = extent::empty_root();
        } else if has_blocks {
            inode.block = [0; inode::BLOCK_LEN];
        }
        inode.encode(&self.geo, ino, &mut raw);
        self.write(txn, at, &raw)
    }

    /// Reads the blocks inode `ino`'s data is in: its extents, and the
    /// blocks of its tree or block map.
    pub(crate) fn load_blocks(
        &self,
        ino: u32,
        inode: &Inode,
        buf: &mut [u8],
        extents: &mut Extents,
        tree: &mut KVec<u64>,
    ) -> Result<()> {
        if inode.flags & (inode::flags::INLINE_DATA | inode::flags::EA_INODE) != 0 {
            return Err(Error::NotSupported);
        }
        let shift = self.geo.shift;
        if inode.has_extents() {
            let mut read = |block: u64, out: &mut [u8]| self.read(block << shift, out);
            extent::Reader {
                geo: &self.geo,
                seed: inode::seed(&self.geo, ino, inode.generation),
                read: &mut read,
                buf,
            }
            .load(&inode.block, extents, tree)
        } else {
            let blocks = inode.size.div_ceil(u64::from(self.geo.block_size));
            let mut read = |block: u64, from: usize, out: &mut [u32]| {
                let mut bytes = [0u8; 512];
                let len = out.len() * 4;
                self.read((block << shift) + from as u64 * 4, &mut bytes[..len])?;
                for (k, word) in out.iter_mut().enumerate() {
                    *word = le32(&bytes, k * 4);
                }
                Ok(())
            };
            extent::load_map(&self.geo, &inode.block, blocks, &mut read, extents, tree)
        }
    }

    /// Drops a file's reference to its block of extended attributes,
    /// freeing the block with the last.
    pub(crate) fn drop_attributes(&self, txn: &mut Txn, block: u64) -> Result<()> {
        if block < self.geo.first_data_block || block >= self.geo.blocks {
            return Err(Error::Corrupt);
        }
        let bs = self.geo.block_size as usize;
        let at = self.geo.offset(block);
        let mut head = [0u8; 32];
        self.read(at, &mut head)?;
        if le32(&head, 0) != 0xea02_0000 {
            return Err(Error::Corrupt);
        }
        let refs = le32(&head, 4);
        if refs <= 1 {
            return self.free_blocks(txn, block, 1);
        }
        let Txn { buf, .. } = txn;
        self.read(at, &mut buf[..bs])?;
        put32(buf, 4, refs - 1);
        if self.geo.metadata_csum() {
            put32(buf, 0x10, 0);
            let mut sum = crate::crc::crc32c(self.geo.seed, &block.to_le_bytes());
            sum = crate::crc::crc32c(sum, &buf[..bs]);
            put32(buf, 0x10, sum);
        }
        let copy = core::mem::replace(&mut txn.buf, mem::vec());
        let written = self.write(txn, at, &copy[..bs]);
        txn.buf = copy;
        written
    }

    /// Takes inode `ino` off the orphan list, pointing the one before it
    /// at the one after.
    pub(crate) fn unlink_orphan(&self, txn: &mut Txn, ino: u32) -> Result<()> {
        let Some(pos) = txn.orphans.iter().position(|&o| o == ino) else { return Ok(()) };
        txn.orphans.remove(pos);
        if pos == 0 {
            return Ok(());
        }
        let prev = txn.orphans[pos - 1];
        let next = txn.orphans.get(pos).copied().unwrap_or(0);
        if let Some(node) = self.live(prev) {
            return node.set_next_orphan(txn, next);
        }
        let at = self.inode_offset(prev)?;
        let mut raw = zeroed(self.geo.inode_size as usize)?;
        self.read(at, &mut raw)?;
        let mut inode = Inode::parse(&self.geo, prev, &raw)?;
        inode.dtime = next;
        inode.encode(&self.geo, prev, &mut raw);
        self.write(txn, at, &raw)
    }

    /// Commits the running transaction: frees the inodes whose last
    /// reference went, then writes every block it changed to the journal
    /// and home. A failure gives up writing.
    pub(crate) fn commit(&self, txn: &mut Txn) -> Result<()> {
        self.writable()?;
        let done = self.commit_now(txn);
        if done.is_err() {
            self.fail();
        }
        done
    }

    fn commit_now(&self, txn: &mut Txn) -> Result<()> {
        loop {
            let Some(ino) = self.doomed.lock().pop() else { break };
            self.free_file(txn, ino)?;
        }
        if txn.blocks.is_empty() {
            return Ok(());
        }
        self.seal_groups(txn)?;
        {
            let space = self.space.lock();
            sb::set_free(&self.geo, &mut txn.sb, space.free_blocks, space.free_inodes as u32);
        }
        let head = txn.orphans.first().copied().unwrap_or(0);
        put32(&mut txn.sb, at::LAST_ORPHAN, head);
        put32(&mut txn.sb, at::WTIME, now_secs());
        sb::seal(&self.geo, &mut txn.sb);
        let sb = core::mem::replace(&mut txn.sb, mem::vec());
        let written = self.write(txn, sb::OFFSET, &sb);
        txn.sb = sb;
        written?;
        let shift = self.geo.shift;
        let Txn { blocks, journal, .. } = txn;
        if let Some(journal) = journal {
            let mut read = |block: u64, out: &mut [u8]| Ok(self.cache.read(block << shift, out)?);
            journal.write(&**self.cache.device(), blocks, &mut read, radix_vfs::now())?;
        }
        self.cache.sync()?;
        txn.blocks.clear();
        txn.groups.clear();
        txn.pending.clear();
        self.space.lock().pending = 0;
        Ok(())
    }

    /// Writes every change to the disk: each node's data and inode, then
    /// commits.
    pub(crate) fn sync_all(&self) -> Result<()> {
        if self.read_only {
            return Ok(());
        }
        let pinned = core::mem::replace(&mut *self.dirty.lock(), mem::vec());
        let mut failed = None;
        for node in &pinned {
            node.unpinned();
            if let Err(err) = node.flush() {
                failed.get_or_insert(err);
                // It stays dirty, to be tried again.
                node.pin();
            }
        }
        drop(pinned);
        let mut txn = self.txn.lock();
        self.commit(&mut txn)?;
        match failed {
            Some(err) => Err(err),
            None => Ok(()),
        }
    }

    /// The node for inode `ino`: the one in use, or a new one read from
    /// the disk with `buf` for room. The caller holds `ns`.
    pub(crate) fn node_for(this: &KArc<Inner>, ino: u32, buf: &mut [u8]) -> Result<KArc<Ext4Node>> {
        if let Some(node) = this.live(ino) {
            return Ok(node);
        }
        let node = Ext4Node::load(this.clone(), ino, buf)?;
        this.add_node(&node)?;
        Ok(node)
    }

    /// The node in use for inode `ino`.
    pub(crate) fn live(&self, ino: u32) -> Option<KArc<Ext4Node>> {
        let nodes = self.nodes.lock();
        let i = nodes.binary_search_by_key(&ino, |(k, _)| *k).ok()?;
        nodes[i].1.upgrade()
    }

    /// Puts `node` in the table.
    pub(crate) fn add_node(&self, node: &KArc<Ext4Node>) -> Result<()> {
        let ino = node.ino();
        let weak = KArc::downgrade(node);
        let mut nodes = self.nodes.lock();
        match nodes.binary_search_by_key(&ino, |(k, _)| *k) {
            Ok(i) => nodes[i].1 = weak,
            Err(_) => {
                if nodes.len() == nodes.capacity() {
                    // Room is made by forgetting nodes no longer in use
                    // first; the table only grows past twice the nodes in
                    // use.
                    nodes.retain(|(_, weak)| weak.strong_count() > 0);
                }
                let i = nodes.binary_search_by_key(&ino, |(k, _)| *k).unwrap_or_else(|i| i);
                mem::insert(&mut nodes, i, (ino, weak))?;
            }
        }
        Ok(())
    }

    /// The nodes in use, for shrinking their caches.
    fn live_nodes(&self) -> Result<KVec<KArc<Ext4Node>>> {
        let nodes = self.nodes.lock();
        let mut live = mem::vec_with_capacity(nodes.len())?;
        for (_, weak) in nodes.iter() {
            if let Some(node) = weak.upgrade() {
                let _ = live.push_within_capacity(node);
            }
        }
        Ok(live)
    }

    /// Keeps `node` alive until the next sync: it has changes in memory.
    pub(crate) fn pin(&self, node: KArc<Ext4Node>) -> core::result::Result<(), KArc<Ext4Node>> {
        let mut dirty = self.dirty.lock();
        if dirty.try_reserve(1).is_err() {
            return Err(node);
        }
        let _ = dirty.push_within_capacity(node);
        Ok(())
    }
}
