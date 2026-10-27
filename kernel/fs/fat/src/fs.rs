// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! A mounted FAT volume: its tables, its free space and its nodes.
//!
//! Metadata (the tables, directories, the FSInfo sector) is read and
//! written through a [`BlockCache`] of the device, so changes stay in
//! memory until the volume syncs; file data bypasses it and moves
//! between files' page caches and the disk ([`crate::node`]).
//!
//! # Locks
//!
//! - `ns` (the namespace) is held by every directory operation: looking
//!   names up, listing, creating, removing, renaming, and writing a
//!   node's entry back. FAT keeps a file's size and first cluster in its
//!   directory entry, so all of these touch directories, and one lock
//!   keeps them simple: Phase 7's FAT is for removable media and the EFI
//!   partition, not for heavy concurrent use.
//! - A node's page cache, then its `meta` (where its entry is, its
//!   chain), then `space` (free clusters; every table change), then the
//!   block cache. Only one node's `meta` is held at a time, and `ns` is
//!   taken before any of them, never after.
//! - `nodes` (the node table) and `dirty` (nodes waiting to sync) take
//!   no other lock under them.
//!
//! A node is never dropped with `nodes`, `dirty`, `space` or a `meta`
//! held, since dropping a removed file's last reference frees its
//! clusters.

use alloc::sync::Weak;
use core::fmt;

use radix_block::cache::BlockCache;
use radix_block::{DeviceRef, read_sector};
use radix_sync::{Mutex, MutexGuard};
use radix_vfs::mem::{self, Alloc, KArc, KVec};
use radix_vfs::{Error, FileSystem, FsInfo, FsKind, NodeRef, Result};

use crate::bpb::Geometry;
use crate::chain::Chain;
use crate::dir::Names;
use crate::name::{Short, attr};
use crate::node::FatNode;
use crate::table;

/// The FSInfo sector's signatures (FAT32).
const FSINFO_LEAD: u32 = 0x4161_5252;
const FSINFO_STRUCT: u32 = 0x6141_7272;
const FSINFO_TRAIL: u32 = 0xaa55_0000;

/// The free clusters, as far as they are known.
pub(crate) struct Space {
    /// How many are free; counted when first asked for.
    free: Option<u32>,
    /// Where to look for a free cluster first.
    hint: u32,
    /// Whether the FSInfo sector is behind.
    fsinfo_dirty: bool,
}

/// What a volume's nodes share.
pub(crate) struct Inner {
    pub(crate) geo: Geometry,
    pub(crate) cache: BlockCache,
    pub(crate) read_only: bool,
    /// The namespace lock (see the module documentation), with the room
    /// directory scans put names together in.
    pub(crate) ns: Mutex<Names>,
    space: Mutex<Space>,
    /// The nodes in use, by where their entries are ([`key`]), sorted.
    nodes: Mutex<KVec<(u64, Weak<FatNode, Alloc>)>>,
    /// Nodes with changes in memory, kept alive until they are synced.
    dirty: Mutex<KVec<KArc<FatNode>>>,
}

/// A mounted FAT volume.
pub struct FatFs {
    inner: KArc<Inner>,
    root: KArc<FatNode>,
    /// Its label, trimmed.
    label: [u8; 11],
    label_len: usize,
}

/// The node table's key for the entry at `index` of the directory whose
/// first cluster is `dir` (0: the fixed root directory).
pub(crate) fn key(dir: u32, index: u32) -> u64 {
    (u64::from(dir) << 32) | u64::from(index)
}

fn u32_at(b: &[u8], at: usize) -> u32 {
    u32::from_le_bytes([b[at], b[at + 1], b[at + 2], b[at + 3]])
}

impl FatFs {
    /// Mounts the FAT volume on `device`; `read_only` refuses every
    /// change (as does a read-only device).
    pub fn mount(device: DeviceRef, read_only: bool) -> Result<KArc<FatFs>> {
        let info = device.info();
        let mut boot = [0u8; 512];
        read_sector(&*device, 0, &mut boot)?;
        let geo = Geometry::parse(&boot, info.sectors * radix_block::SECTOR)?;
        // Its sectors are read and written alone, which a disk with larger
        // ones cannot do: Linux refuses such a volume too.
        if geo.sector_size < info.logical_block {
            return Err(Error::NotSupported);
        }
        let inner = mem::arc(Inner {
            geo,
            cache: BlockCache::new(device),
            read_only: read_only || info.read_only,
            ns: Mutex::new(Names::new()),
            space: Mutex::new(Space { free: None, hint: 2, fsinfo_dirty: false }),
            nodes: Mutex::new(mem::vec()),
            dirty: Mutex::new(mem::vec()),
        })?;
        inner.read_fsinfo()?;
        let root = FatNode::root(inner.clone())?;
        // The label in the root directory is the one Windows and mtools
        // set; the boot sector's copy is older, and "NO NAME" is none.
        let mut label = root.volume_label()?.unwrap_or(geo.label);
        if &label == b"NO NAME    " {
            label = [b' '; 11];
        }
        let label_len = label.iter().rposition(|&b| b != b' ').map_or(0, |p| p + 1);
        Ok(mem::arc(FatFs { inner, root, label, label_len })?)
    }

    /// Where the volume's parts are.
    pub fn geometry(&self) -> &Geometry {
        &self.inner.geo
    }
}

impl Drop for FatFs {
    fn drop(&mut self) {
        let _ = self.inner.sync_all();
        // What could not be written is lost, but the nodes must not keep
        // each other alive.
        let pinned = core::mem::replace(&mut *self.inner.dirty.lock(), mem::vec());
        drop(pinned);
    }
}

impl FileSystem for FatFs {
    fn kind(&self) -> FsKind {
        FsKind::Fat
    }

    fn root(&self) -> NodeRef {
        self.root.clone()
    }

    fn info(&self) -> FsInfo {
        let geo = &self.inner.geo;
        FsInfo {
            block_size: geo.cluster_size,
            blocks: u64::from(geo.clusters),
            free: u64::from(self.inner.free_clusters().unwrap_or(0)),
            ..FsInfo::default()
        }
    }

    fn read_only(&self) -> bool {
        self.inner.read_only
    }

    fn label(&self, out: &mut dyn fmt::Write) -> fmt::Result {
        for &byte in &self.label[..self.label_len] {
            out.write_char(if byte.is_ascii() { byte as char } else { '_' })?;
        }
        Ok(())
    }

    fn sync(&self) -> Result<()> {
        self.inner.sync_all()
    }

    fn is_dirty(&self) -> bool {
        !self.inner.dirty.lock().is_empty()
            || self.inner.cache.is_dirty()
            || self.inner.space.lock().fsinfo_dirty
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
}

impl Inner {
    /// Table entry `n` (of the first table).
    pub(crate) fn entry(&self, n: u32) -> Result<u32> {
        let width = self.geo.width;
        let mut bytes = [0u8; 4];
        let at = self.geo.fat_start + table::offset(width, n);
        self.cache.read(at, &mut bytes[..table::span(width)])?;
        Ok(table::decode(width, n, bytes))
    }

    /// Sets table entry `n` to `value`, in every copy of the table. The
    /// guard shows the caller holds `space`.
    fn set_entry(&self, _space: &mut MutexGuard<'_, Space>, n: u32, value: u32) -> Result<()> {
        let width = self.geo.width;
        let span = table::span(width);
        for copy in 0..u64::from(self.geo.fats) {
            let at = self.geo.fat_start + copy * self.geo.fat_size + table::offset(width, n);
            let mut old = [0u8; 4];
            self.cache.read(at, &mut old[..span])?;
            let new = table::encode(width, n, old, value);
            self.cache.write(at, &new[..span])?;
        }
        Ok(())
    }

    /// Calls `f` with each table entry from `from` to before `to`, and
    /// its value, until it returns false. Reads the table a sector's
    /// worth at a time.
    fn scan_table(&self, from: u32, to: u32, mut f: impl FnMut(u32, u32) -> bool) -> Result<()> {
        const CHUNK: u32 = 128;
        let width = self.geo.width;
        let span = table::span(width);
        let mut buf = [0u8; 4 * CHUNK as usize];
        let mut n = from;
        while n < to {
            let count = (to - n).min(CHUNK);
            let start = table::offset(width, n);
            let end = table::offset(width, n + count - 1) + span as u64;
            let len = (end - start) as usize;
            self.cache.read(self.geo.fat_start + start, &mut buf[..len])?;
            for m in n..n + count {
                let at = (table::offset(width, m) - start) as usize;
                let mut bytes = [0u8; 4];
                bytes[..span].copy_from_slice(&buf[at..at + span]);
                if !f(m, table::decode(width, m, bytes)) {
                    return Ok(());
                }
            }
            n += count;
        }
        Ok(())
    }

    /// The first free cluster from `near` on, wrapping around.
    fn find_free(&self, near: u32) -> Result<Option<u32>> {
        let end = self.geo.clusters + 2;
        let near = if (2..end).contains(&near) { near } else { 2 };
        let look = |from, to| {
            let mut found = None;
            self.scan_table(from, to, |n, value| {
                if value == 0 {
                    found = Some(n);
                }
                found.is_none()
            })?;
            Ok::<_, Error>(found)
        };
        match look(near, end)? {
            Some(found) => Ok(Some(found)),
            None => look(2, near),
        }
    }

    /// Takes a free cluster, ending a chain, and links it after `last`:
    /// right after it on the disk when that is free, so that files stay
    /// in one piece.
    pub(crate) fn extend(&self, last: Option<u32>) -> Result<u32> {
        let mut space = self.space.lock();
        let near = last.map_or(space.hint, |c| c + 1);
        let Some(cluster) = self.find_free(near)? else {
            space.free = Some(0);
            return Err(Error::NoSpace);
        };
        let end = self.geo.width.end();
        self.set_entry(&mut space, cluster, end)?;
        if let Some(last) = last
            && let Err(err) = self.set_entry(&mut space, last, cluster)
        {
            let _ = self.set_entry(&mut space, cluster, 0);
            return Err(err);
        }
        space.free = space.free.map(|free| free.saturating_sub(1));
        space.hint = cluster + 1;
        space.fsinfo_dirty = true;
        Ok(cluster)
    }

    /// Frees the chain from `first`, following the table. `discard`
    /// drops the clusters from the block cache: a directory's, whose
    /// entries must not be written over what is put there next.
    pub(crate) fn free_chain(&self, first: u32, discard: bool) -> Result<()> {
        if first == 0 {
            return Ok(());
        }
        let mut space = self.space.lock();
        let mut cluster = first;
        // A chain is no longer than the volume; a longer one is a loop.
        for _ in 0..self.geo.clusters {
            if !self.geo.valid(cluster) {
                return Err(Error::Corrupt);
            }
            let next = self.entry(cluster)?;
            if next == 0 {
                // Free already: the chain was broken.
                break;
            }
            self.free_one(&mut space, cluster, discard)?;
            if self.geo.width.is_end(next) {
                break;
            }
            cluster = next;
        }
        Ok(())
    }

    /// Cuts `chain`, whose first cluster is `*first`, to `keep` clusters,
    /// freeing the rest.
    pub(crate) fn cut(
        &self,
        chain: &mut Chain,
        first: &mut u32,
        keep: u32,
        discard: bool,
    ) -> Result<()> {
        if keep >= chain.len() {
            return Ok(());
        }
        let mut space = self.space.lock();
        if let Some(last) = keep.checked_sub(1).and_then(|i| chain.get(i)) {
            let end = self.geo.width.end();
            self.set_entry(&mut space, last, end)?;
        } else {
            *first = 0;
        }
        for cluster in chain.clusters_from(keep) {
            self.free_one(&mut space, cluster, discard)?;
        }
        chain.truncate(keep);
        Ok(())
    }

    fn free_one(
        &self,
        space: &mut MutexGuard<'_, Space>,
        cluster: u32,
        discard: bool,
    ) -> Result<()> {
        self.set_entry(space, cluster, 0)?;
        if discard {
            let size = self.geo.cluster_size as usize;
            self.cache.discard(self.geo.cluster_offset(cluster), size);
        }
        space.free = space.free.map(|free| free.saturating_add(1));
        space.fsinfo_dirty = true;
        Ok(())
    }

    /// The chain from `first`, read from the table.
    pub(crate) fn load_chain(&self, first: u32) -> Result<Chain> {
        let mut chain = Chain::new();
        if first == 0 {
            return Ok(chain);
        }
        let mut cluster = first;
        loop {
            if !self.geo.valid(cluster) || chain.len() >= self.geo.clusters {
                return Err(Error::Corrupt);
            }
            chain.push(cluster)?;
            let next = self.entry(cluster)?;
            if self.geo.width.is_end(next) {
                return Ok(chain);
            }
            if next == 0 {
                return Err(Error::Corrupt);
            }
            cluster = next;
        }
    }

    /// Zeroes cluster `cluster` in the block cache: a new directory's.
    pub(crate) fn zero_cluster(&self, cluster: u32) -> Result<()> {
        let size = self.geo.cluster_size as usize;
        Ok(self.cache.fill(self.geo.cluster_offset(cluster), size, 0)?)
    }

    /// How many clusters are free, counting them the first time.
    fn free_clusters(&self) -> Result<u32> {
        let mut space = self.space.lock();
        if let Some(free) = space.free {
            return Ok(free);
        }
        let mut free = 0;
        self.scan_table(2, self.geo.clusters + 2, |_, value| {
            if value == 0 {
                free += 1;
            }
            true
        })?;
        space.free = Some(free);
        Ok(free)
    }

    /// Reads the FSInfo sector's hints (FAT32): the free count and where
    /// a free cluster is.
    fn read_fsinfo(&self) -> Result<()> {
        let Some(at) = self.geo.fsinfo else { return Ok(()) };
        let mut sector = [0u8; 512];
        self.cache.read(at, &mut sector)?;
        if !fsinfo_valid(&sector) {
            return Ok(());
        }
        let mut space = self.space.lock();
        let free = u32_at(&sector, 488);
        if free <= self.geo.clusters {
            space.free = Some(free);
        }
        let next = u32_at(&sector, 492);
        if self.geo.valid(next) {
            space.hint = next;
        }
        Ok(())
    }

    /// Writes the free count and the hint back to the FSInfo sector.
    fn write_fsinfo(&self) -> Result<()> {
        let mut space = self.space.lock();
        if !space.fsinfo_dirty {
            return Ok(());
        }
        // Clean only once it is written, so that a write that fails is
        // tried again at the next sync.
        let mut sector = [0u8; 512];
        if let Some(at) = self.geo.fsinfo {
            self.cache.read(at, &mut sector)?;
            if fsinfo_valid(&sector) {
                let free = space.free.unwrap_or(u32::MAX);
                let hint = if self.geo.valid(space.hint) { space.hint } else { u32::MAX };
                self.cache.write(at + 488, &free.to_le_bytes())?;
                self.cache.write(at + 492, &hint.to_le_bytes())?;
            }
        }
        space.fsinfo_dirty = false;
        Ok(())
    }

    /// The node for the entry `found` of the directory whose first
    /// cluster is `dir`: the one in use, or a new one. The caller holds
    /// `ns`.
    pub(crate) fn node_for(
        this: &KArc<Inner>,
        dir: u32,
        index: u32,
        offset: u64,
        short: &Short,
    ) -> Result<KArc<FatNode>> {
        let key = key(dir, index);
        let mut nodes = this.nodes.lock();
        let at = nodes.binary_search_by_key(&key, |(k, _)| *k);
        if let Ok(i) = at
            && let Some(node) = nodes[i].1.upgrade()
        {
            return Ok(node);
        }
        let node = FatNode::load(this.clone(), dir, index, offset, short)?;
        let weak = KArc::downgrade(&node);
        match at {
            Ok(i) => nodes[i].1 = weak,
            Err(_) => {
                if nodes.len() == nodes.capacity() {
                    // Room is made by forgetting nodes no longer in use
                    // first; the table only grows past twice the nodes
                    // in use.
                    nodes.retain(|(_, weak)| weak.strong_count() > 0);
                }
                let i = nodes.binary_search_by_key(&key, |(k, _)| *k).unwrap_or_else(|i| i);
                if mem::insert(&mut nodes, i, (key, weak)).is_err() {
                    drop(nodes);
                    drop(node);
                    return Err(Error::NoMemory);
                }
            }
        }
        Ok(node)
    }

    /// The node in use for `key`, taken out of the table: its entry is
    /// going. The caller holds `ns`.
    pub(crate) fn take_node(&self, key: u64) -> Option<KArc<FatNode>> {
        let weak = {
            let mut nodes = self.nodes.lock();
            let i = nodes.binary_search_by_key(&key, |(k, _)| *k).ok()?;
            nodes.remove(i).1
        };
        weak.upgrade()
    }

    /// Moves `node` from `old` to `new` in the table, after a rename. The
    /// caller holds `ns`.
    pub(crate) fn rekey(&self, old: u64, new: u64, node: &KArc<FatNode>) -> Result<()> {
        let mut nodes = self.nodes.lock();
        if let Ok(i) = nodes.binary_search_by_key(&old, |(k, _)| *k) {
            nodes.remove(i);
        }
        let weak = KArc::downgrade(node);
        match nodes.binary_search_by_key(&new, |(k, _)| *k) {
            Ok(i) => nodes[i].1 = weak,
            Err(i) => mem::insert(&mut nodes, i, (new, weak))?,
        }
        Ok(())
    }

    /// The nodes in use, for shrinking their caches.
    fn live_nodes(&self) -> Result<KVec<KArc<FatNode>>> {
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
    pub(crate) fn pin(&self, node: KArc<FatNode>) -> core::result::Result<(), KArc<FatNode>> {
        let mut dirty = self.dirty.lock();
        if dirty.try_reserve(1).is_err() {
            return Err(node);
        }
        let _ = dirty.push_within_capacity(node);
        Ok(())
    }

    /// Writes every change to the disk: the nodes' data and entries, the
    /// FSInfo sector and the tables, then flushes the device.
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
        self.write_fsinfo()?;
        self.cache.sync()?;
        match failed {
            Some(err) => Err(err),
            None => Ok(()),
        }
    }
}

fn fsinfo_valid(sector: &[u8; 512]) -> bool {
    u32_at(sector, 0) == FSINFO_LEAD
        && u32_at(sector, 484) == FSINFO_STRUCT
        && u32_at(sector, 508) == FSINFO_TRAIL
}

/// Whether a short entry is a volume label.
pub(crate) fn is_label(short: &Short) -> bool {
    short.attr & 0x3f != attr::LONG_NAME && short.attr & attr::VOLUME_ID != 0
}
