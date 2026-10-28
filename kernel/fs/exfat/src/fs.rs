// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! A mounted exFAT volume: its bitmap, its table, its up-case table and
//! its nodes.
//!
//! Metadata (the table, the bitmap, directories) is read and written
//! through a [`BlockCache`] of the device, so changes stay in memory until
//! the volume syncs; file data bypasses it and moves between files' page
//! caches and the disk ([`crate::node`]).
//!
//! The allocation bitmap, a file in the cluster heap that the root
//! directory names, has a bit for each cluster, set when it is in use;
//! the table links the clusters of files in pieces, as FAT's does, and
//! says nothing of the others. Radix gives a file the clusters after its
//! last when they are free, so that it stays one run without table
//! entries, and writes the table for it only once it cannot.
//!
//! exFAT has no journal. The boot sector's volume flags have a bit that
//! says the volume may not be consistent: Radix sets it (on the disk, at
//! once) before its first change after a mount or a sync, and clears it
//! once a sync has written every change and nothing changed meanwhile,
//! so that after a power cut `fsck.exfat` and other systems know to
//! check the volume, as with Linux's driver.
//!
//! # Locks
//!
//! - `ns` (the namespace) is held by every directory operation: looking
//!   names up, listing, creating, removing, renaming, and writing a
//!   node's entries back, since exFAT keeps a file's size and first
//!   cluster in its directory's entries. One lock keeps them simple, as
//!   in Radix's FAT.
//! - A node's page cache, then its `meta` (where its entries are, its
//!   chain), then `space` (free clusters; every bitmap and table change),
//!   then the block cache. Only one node's `meta` is held at a time, and
//!   `ns` is taken before any of them, never after.
//! - `nodes` (the node table) and `dirty` (nodes waiting to sync) take
//!   no other lock under them.
//!
//! A node is never dropped with `nodes`, `dirty`, `space` or a `meta`
//! held, since dropping a removed file's last reference frees its
//! clusters.

use alloc::sync::Weak;
use core::fmt;

use radix_block::cache::BlockCache;
use radix_block::mem::CachePage;
use radix_block::{BlockDevice, DeviceRef, read_sector};
use radix_fat::chain::Chain;
use radix_sync::Mutex;
use radix_vfs::mem::{self, Alloc, KArc, KBox, KVec};
use radix_vfs::{Error, FileSystem, FsInfo, FsKind, NodeRef, Result};

use crate::boot::{self, FLAGS_AT, Geometry, REGION_SECTORS};
use crate::dir::{self, Found, Names, Runs};
use crate::entry::{self, kind};
use crate::node::ExfatNode;
use crate::upcase::Upcase;

/// The table entry that ends a chain.
pub(crate) const END: u32 = 0xffff_ffff;

/// The free clusters, as far as they are known, and the dirty bit.
pub(crate) struct Space {
    /// How many are free; counted when first asked for.
    free: Option<u32>,
    /// Where to look for a free cluster first.
    hint: u32,
    /// Whether the boot sector on the disk says the volume is dirty.
    marked: bool,
}

/// What a volume's nodes share.
pub(crate) struct Inner {
    pub(crate) geo: Geometry,
    pub(crate) cache: BlockCache,
    pub(crate) read_only: bool,
    /// The namespace lock (see the module documentation), with the room
    /// directory scans put names together in.
    pub(crate) ns: Mutex<KBox<Names>>,
    space: Mutex<Space>,
    /// The nodes in use, by where their entries are ([`key`]), sorted.
    nodes: Mutex<KVec<(u64, Weak<ExfatNode, Alloc>)>>,
    /// Nodes with changes in memory, kept alive until they are synced.
    dirty: Mutex<KVec<KArc<ExfatNode>>>,
    /// The volume's up-case table, which decides which names are the
    /// same.
    pub(crate) upcase: Upcase,
    /// The clusters of the allocation bitmap in use.
    bitmap: Chain,
    /// Where the table in use starts: the second one's, on a TexFAT
    /// volume whose flags say so.
    fat: u64,
}

/// A mounted exFAT volume.
pub struct ExfatFs {
    inner: KArc<Inner>,
    root: KArc<ExfatNode>,
    /// Its label, in UTF-16.
    label: [u16; 11],
    label_len: usize,
}

/// The node table's key for the set at `index` of the directory whose
/// first cluster is `dir`.
pub(crate) fn key(dir: u32, index: u32) -> u64 {
    (u64::from(dir) << 32) | u64::from(index)
}

/// What the root directory says of the volume.
struct Root {
    /// The allocation bitmap's first cluster and length.
    bitmap: Option<(u32, u64)>,
    /// The up-case table's first cluster, length and checksum.
    upcase: Option<(u32, u64, u32)>,
    /// The label's units, and how many.
    label: [u16; 11],
    label_len: usize,
}

impl ExfatFs {
    /// Mounts the exFAT volume on `device`; `read_only` refuses every
    /// change, as does a read-only device. A volume with two tables
    /// (TexFAT), or whose medium reported a failure, Radix reads but does
    /// not write: a writable mount of one fails with
    /// [`Error::NotSupported`], as a Btrfs that Radix cannot write does
    /// (ADR 0130).
    pub fn mount(device: DeviceRef, read_only: bool) -> Result<KArc<ExfatFs>> {
        let info = device.info();
        let geo = read_geometry(&*device, info.sectors * radix_block::SECTOR)?;
        // Its sectors are read and written alone, which a disk with larger
        // ones cannot do: Linux refuses such a volume too.
        if geo.sector_size < info.logical_block {
            return Err(Error::NotSupported);
        }
        let cache = BlockCache::new(device);
        check_region(&cache, &geo)?;
        let active = geo.fats == 2 && geo.flags & boot::flags::ACTIVE_FAT != 0;
        let fat = geo.fat_start + if active { geo.fat_size } else { 0 };
        let root_chain = follow(&cache, &geo, fat, geo.root_cluster)?;
        let root = read_root(&cache, &geo, &root_chain, active)?;
        let (bitmap_first, bitmap_len) = root.bitmap.ok_or(Error::Corrupt)?;
        let bitmap = follow(&cache, &geo, fat, bitmap_first)?;
        let bitmap_room = u64::from(bitmap.len()) * u64::from(geo.cluster_size);
        if bitmap_len < u64::from(geo.clusters).div_ceil(8) || bitmap_room < bitmap_len {
            return Err(Error::Corrupt);
        }
        let upcase = load_upcase(&cache, &geo, fat, root.upcase.ok_or(Error::Corrupt)?)?;
        if !read_only && (geo.fats != 1 || geo.flags & boot::flags::MEDIA_FAILURE != 0) {
            return Err(Error::NotSupported);
        }
        let read_only = read_only || info.read_only;
        let marked = geo.flags & boot::flags::DIRTY != 0;
        let inner = mem::arc(Inner {
            geo,
            cache,
            read_only,
            ns: Mutex::new(names()?),
            space: Mutex::new(Space { free: None, hint: 2, marked }),
            nodes: Mutex::new(mem::vec()),
            dirty: Mutex::new(mem::vec()),
            upcase,
            bitmap,
            fat,
        })?;
        let root_node = ExfatNode::root(inner.clone(), root_chain)?;
        Ok(mem::arc(ExfatFs {
            inner,
            root: root_node,
            label: root.label,
            label_len: root.label_len,
        })?)
    }

    /// Where the volume's parts are.
    pub fn geometry(&self) -> &Geometry {
        &self.inner.geo
    }
}

/// The geometry the boot sector of `device`, of `bytes` bytes, gives.
/// Each step of a mount that reads the disk has a frame of its own: a
/// mount runs in a system call, on a 16 KiB stack.
#[inline(never)]
fn read_geometry(device: &dyn BlockDevice, bytes: u64) -> Result<Geometry> {
    let mut boot = [0u8; 512];
    read_sector(device, 0, &mut boot)?;
    Geometry::parse(&boot, bytes)
}

/// Checks the main boot region's checksum: the last of its twelve
/// sectors repeats the checksum of the eleven before it.
#[inline(never)]
fn check_region(cache: &BlockCache, geo: &Geometry) -> Result<()> {
    let ss = u64::from(geo.sector_size);
    let mut buf = [0u8; 512];
    let mut sum = 0;
    let mut at = 0;
    while at < (REGION_SECTORS - 1) * ss {
        cache.read(at, &mut buf)?;
        sum = boot::checksum(sum, at, &buf);
        at += 512;
    }
    while at < REGION_SECTORS * ss {
        cache.read(at, &mut buf)?;
        if buf.as_chunks::<4>().0.iter().any(|word| u32::from_le_bytes(*word) != sum) {
            return Err(Error::Corrupt);
        }
        at += 512;
    }
    Ok(())
}

/// Table entry `n` of the table at `fat`.
fn fat_entry(cache: &BlockCache, fat: u64, n: u32) -> Result<u32> {
    let mut bytes = [0u8; 4];
    cache.read(fat + u64::from(n) * 4, &mut bytes)?;
    Ok(u32::from_le_bytes(bytes))
}

/// The chain from `first`, by the table at `fat`.
fn follow(cache: &BlockCache, geo: &Geometry, fat: u64, first: u32) -> Result<Chain> {
    let mut chain = Chain::new();
    let mut cluster = first;
    loop {
        // A chain is no longer than the volume; a longer one is a loop.
        if !geo.valid(cluster) || chain.len() >= geo.clusters {
            return Err(Error::Corrupt);
        }
        chain.push(cluster)?;
        match fat_entry(cache, fat, cluster)? {
            END => return Ok(chain),
            next => cluster = next,
        }
    }
}

/// Finds the bitmap, the up-case table and the label in the root
/// directory, `chain`; `second` takes the second bitmap (TexFAT).
#[inline(never)]
fn read_root(cache: &BlockCache, geo: &Geometry, chain: &Chain, second: bool) -> Result<Root> {
    let mut root = Root { bitmap: None, upcase: None, label: [0; 11], label_len: 0 };
    dir::walk(cache, geo, chain, 0, |_, _, bytes| {
        match bytes[0] {
            kind::END => return false,
            kind::BITMAP if (bytes[1] & 1 != 0) == second => {
                root.bitmap = Some((entry::first_cluster(bytes), entry::data_length(bytes)));
            }
            kind::UPCASE => {
                let found = (
                    entry::first_cluster(bytes),
                    entry::data_length(bytes),
                    entry::table_checksum(bytes),
                );
                root.upcase = Some(found);
            }
            kind::LABEL => {
                root.label_len = usize::from(bytes[1]).min(11);
                for (i, unit) in root.label.iter_mut().enumerate() {
                    *unit = u16::from_le_bytes([bytes[2 + 2 * i], bytes[3 + 2 * i]]);
                }
            }
            _ => {}
        }
        true
    })?;
    Ok(root)
}

/// Loads the up-case table `(first, len, checksum)`.
#[inline(never)]
fn load_upcase(
    cache: &BlockCache,
    geo: &Geometry,
    fat: u64,
    (first, len, checksum): (u32, u64, u32),
) -> Result<Upcase> {
    let chain = follow(cache, geo, fat, first)?;
    let cs = u64::from(geo.cluster_size);
    if u64::from(chain.len()) * cs < len {
        return Err(Error::Corrupt);
    }
    // The pieces are read 512 bytes at a time from the table's start, so
    // none crosses a cluster.
    Upcase::load(len, checksum, |at, buf| {
        let cluster = chain.get((at / cs) as u32).ok_or(Error::Corrupt)?;
        Ok(cache.read(geo.cluster_offset(cluster) + at % cs, buf)?)
    })
}

impl Drop for ExfatFs {
    fn drop(&mut self) {
        let _ = self.inner.sync_all();
        // What could not be written is lost, but the nodes must not keep
        // each other alive.
        let pinned = core::mem::replace(&mut *self.inner.dirty.lock(), mem::vec());
        drop(pinned);
    }
}

impl FileSystem for ExfatFs {
    fn kind(&self) -> FsKind {
        FsKind::Exfat
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
        for c in char::decode_utf16(self.label[..self.label_len].iter().copied()) {
            out.write_char(c.unwrap_or(char::REPLACEMENT_CHARACTER))?;
        }
        Ok(())
    }

    fn sync(&self) -> Result<()> {
        self.inner.sync_all()
    }

    fn is_dirty(&self) -> bool {
        !self.inner.dirty.lock().is_empty() || self.inner.cache.is_dirty()
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

/// The bytes of the bitmap read at a time: a sector's worth, which never
/// crosses one of its clusters.
const BITMAP_CHUNK: u32 = 512;

impl Inner {
    /// The chain from `first`, by the table.
    pub(crate) fn follow(&self, first: u32) -> Result<Chain> {
        follow(&self.cache, &self.geo, self.fat, first)
    }

    /// Writes the table entries of clusters `start..start + count`: each
    /// links to the next, and the last to `then`. The caller holds
    /// `space`.
    fn link(&self, space: &mut Space, start: u32, count: u32, then: u32) -> Result<()> {
        const PER: u32 = 128;
        self.mark(space)?;
        let mut buf = [0u8; 4 * PER as usize];
        let mut done = 0;
        while done < count {
            let n = (count - done).min(PER);
            for i in 0..n {
                let cluster = start + done + i;
                let next = if done + i + 1 == count { then } else { cluster + 1 };
                buf[4 * i as usize..4 * i as usize + 4].copy_from_slice(&next.to_le_bytes());
            }
            let at = self.fat + u64::from(start + done) * 4;
            self.cache.write(at, &buf[..4 * n as usize])?;
            done += n;
        }
        Ok(())
    }

    /// Where byte `byte` of the bitmap is on the device.
    fn bitmap_at(&self, byte: u32) -> Result<u64> {
        let cs = self.geo.cluster_size;
        let cluster = self.bitmap.get(byte / cs).ok_or(Error::Corrupt)?;
        Ok(self.geo.cluster_offset(cluster) + u64::from(byte % cs))
    }

    /// Calls `f` with each byte of the bitmap from the one holding bit
    /// `from` to the one holding bit `to - 1`, and the number of its
    /// first bit, until it returns false.
    fn scan_bits(&self, from: u32, to: u32, mut f: impl FnMut(u32, u8) -> bool) -> Result<()> {
        let mut buf = [0u8; BITMAP_CHUNK as usize];
        let mut byte = from / 8;
        let end = to.div_ceil(8);
        while byte < end {
            let n = (BITMAP_CHUNK - byte % BITMAP_CHUNK).min(end - byte);
            self.cache.read(self.bitmap_at(byte)?, &mut buf[..n as usize])?;
            for (i, &bits) in buf[..n as usize].iter().enumerate() {
                if !f((byte + i as u32) * 8, bits) {
                    return Ok(());
                }
            }
            byte += n;
        }
        Ok(())
    }

    /// The first free cluster's bit in `from..to`.
    fn first_free(&self, from: u32, to: u32) -> Result<Option<u32>> {
        let mut found = None;
        self.scan_bits(from, to, |first, bits| {
            if bits != 0xff {
                for bit in 0..8 {
                    let n = first + bit;
                    if (from..to).contains(&n) && bits & (1 << bit) == 0 {
                        found = Some(n);
                        break;
                    }
                }
            }
            found.is_none()
        })?;
        Ok(found)
    }

    /// How many free clusters' bits there are in a row from bit `from`,
    /// up to `max`.
    fn free_run(&self, from: u32, max: u32) -> Result<u32> {
        let to = from.saturating_add(max).min(self.geo.clusters);
        let mut run = 0;
        self.scan_bits(from, to, |first, bits| {
            for bit in 0..8 {
                let n = first + bit;
                if n < from {
                    continue;
                }
                if n >= to || bits & (1 << bit) != 0 {
                    return false;
                }
                run += 1;
            }
            true
        })?;
        Ok(run)
    }

    /// Sets (`used`) or clears the bits of clusters `start..start +
    /// count`, each of which must be the other way.
    fn set_bits(&self, space: &mut Space, start: u32, count: u32, used: bool) -> Result<()> {
        if count == 0 {
            return Ok(());
        }
        if !self.geo.valid(start) || !self.geo.valid(start + (count - 1)) {
            return Err(Error::Corrupt);
        }
        self.mark(space)?;
        let mut buf = [0u8; BITMAP_CHUNK as usize];
        let (from, to) = (start - 2, start - 2 + count);
        let mut byte = from / 8;
        while byte < to.div_ceil(8) {
            let n = (BITMAP_CHUNK - byte % BITMAP_CHUNK).min(to.div_ceil(8) - byte);
            let at = self.bitmap_at(byte)?;
            self.cache.read(at, &mut buf[..n as usize])?;
            for (i, bits) in buf[..n as usize].iter_mut().enumerate() {
                for bit in 0..8 {
                    let k = (byte + i as u32) * 8 + bit;
                    if (from..to).contains(&k) {
                        if (*bits & (1 << bit) != 0) == used {
                            return Err(Error::Corrupt);
                        }
                        *bits ^= 1 << bit;
                    }
                }
            }
            self.cache.write(at, &buf[..n as usize])?;
            byte += n;
        }
        space.free = space
            .free
            .map(|free| if used { free.saturating_sub(count) } else { free.saturating_add(count) });
        Ok(())
    }

    /// Takes up to `max` free clusters in a row from cluster `start`, if
    /// it is free; returns how many.
    fn take_at(&self, space: &mut Space, start: u32, max: u32) -> Result<u32> {
        if !self.geo.valid(start) {
            return Ok(0);
        }
        let n = self.free_run(start - 2, max)?;
        self.set_bits(space, start, n, true)?;
        if n > 0 {
            space.hint = start + n;
        }
        Ok(n)
    }

    /// Takes up to `max` free clusters in a row, the first free from the
    /// hint on; returns the first and how many.
    fn take_near(&self, space: &mut Space, max: u32) -> Result<(u32, u32)> {
        let clusters = self.geo.clusters;
        let near = space.hint.saturating_sub(2).min(clusters);
        let bit = match self.first_free(near, clusters)? {
            Some(bit) => bit,
            None => match self.first_free(0, near)? {
                Some(bit) => bit,
                None => {
                    space.free = Some(0);
                    return Err(Error::NoSpace);
                }
            },
        };
        let n = self.take_at(space, bit + 2, max)?;
        if n == 0 {
            return Err(Error::Corrupt);
        }
        Ok((bit + 2, n))
    }

    /// Grows `chain`, whose first cluster is `*first` and which needs no
    /// table entries if `*contiguous`, to `needed` clusters: with the
    /// clusters after its last while they are free, and once they are
    /// not, with the table written for it. `zero` zeroes the new
    /// clusters in the block cache: a directory's. On failure the chain
    /// is as it was.
    pub(crate) fn grow(
        &self,
        chain: &mut Chain,
        first: &mut u32,
        contiguous: &mut bool,
        needed: u32,
        zero: bool,
    ) -> Result<()> {
        if chain.len() >= needed {
            return Ok(());
        }
        let mut space = self.space.lock();
        let had = (chain.len(), *first, *contiguous);
        let grown = self.grow_locked(&mut space, chain, first, contiguous, needed, zero);
        if grown.is_err() {
            // Cut back, which ends the chain at its last cluster again.
            let (len, was_first, was_contiguous) = had;
            let _ = self.cut_locked(&mut space, chain, first, contiguous, len, zero);
            *first = was_first;
            *contiguous = was_contiguous;
        }
        grown
    }

    fn grow_locked(
        &self,
        space: &mut Space,
        chain: &mut Chain,
        first: &mut u32,
        contiguous: &mut bool,
        needed: u32,
        zero: bool,
    ) -> Result<()> {
        while chain.len() < needed {
            let want = needed - chain.len();
            chain.reserve()?;
            let last = chain.last();
            let after = match last {
                Some(last) => self.take_at(space, last + 1, want)?,
                None => 0,
            };
            let (start, n) = match (last, after) {
                (Some(last), n) if n > 0 => (last + 1, n),
                _ => self.take_near(space, want)?,
            };
            match last {
                Some(last) => {
                    if start != last + 1 && *contiguous {
                        // The run ends: the table gets the chain so far.
                        self.write_chain(space, chain)?;
                        *contiguous = false;
                    }
                    if !*contiguous {
                        self.link(space, last, 1, start)?;
                    }
                }
                None => *first = start,
            }
            if !*contiguous {
                self.link(space, start, n, END)?;
            }
            // Room was made for it.
            chain.push_run(start, n)?;
            if zero {
                let cs = u64::from(self.geo.cluster_size);
                self.cache.fill(self.geo.cluster_offset(start), (u64::from(n) * cs) as usize, 0)?;
            }
        }
        Ok(())
    }

    /// Writes the table entries of all of `chain`, which had none.
    fn write_chain(&self, space: &mut Space, chain: &Chain) -> Result<()> {
        let mut i = 0;
        while let Some((cluster, run)) = chain.extent(i) {
            let then = chain.get(i + run).unwrap_or(END);
            self.link(space, cluster, run, then)?;
            i += run;
        }
        Ok(())
    }

    /// Cuts `chain` ([`Inner::grow`]) to `keep` clusters, freeing the
    /// rest. `discard` drops them from the block cache: a directory's,
    /// whose entries must not be written over what is put there next.
    pub(crate) fn cut(
        &self,
        chain: &mut Chain,
        first: &mut u32,
        contiguous: &mut bool,
        keep: u32,
        discard: bool,
    ) -> Result<()> {
        if keep >= chain.len() {
            return Ok(());
        }
        let mut space = self.space.lock();
        self.cut_locked(&mut space, chain, first, contiguous, keep, discard)
    }

    fn cut_locked(
        &self,
        space: &mut Space,
        chain: &mut Chain,
        first: &mut u32,
        contiguous: &mut bool,
        keep: u32,
        discard: bool,
    ) -> Result<()> {
        if keep >= chain.len() {
            return Ok(());
        }
        if !*contiguous && let Some(last) = keep.checked_sub(1).and_then(|i| chain.get(i)) {
            self.link(space, last, 1, END)?;
        }
        let cs = u64::from(self.geo.cluster_size);
        let mut i = keep;
        while let Some((cluster, run)) = chain.extent(i) {
            self.set_bits(space, cluster, run, false)?;
            if discard {
                self.cache
                    .discard(self.geo.cluster_offset(cluster), (u64::from(run) * cs) as usize);
            }
            i += run;
        }
        chain.truncate(keep);
        if keep == 0 {
            *first = 0;
            *contiguous = false;
        }
        Ok(())
    }

    /// Sets the dirty bit on the disk if it is not set: before the first
    /// change since the volume was last clean.
    fn mark(&self, space: &mut Space) -> Result<()> {
        if !space.marked {
            self.write_flags(self.geo.flags | boot::flags::DIRTY)?;
            space.marked = true;
        }
        Ok(())
    }

    /// Notes a directory change about to be made: the volume is dirty
    /// from now on. The caller holds `ns`.
    pub(crate) fn changing(&self) -> Result<()> {
        self.mark(&mut self.space.lock())
    }

    /// Writes the volume flags `flags`, durably, before anything else is
    /// written.
    fn write_flags(&self, flags: u16) -> Result<()> {
        let block = self.cache.device().info().logical_block as usize;
        let mut page = CachePage::new()?;
        let bytes = &mut page.bytes_mut()[..block];
        self.cache.read(0, bytes)?;
        let at = FLAGS_AT as usize;
        bytes[at..at + 2].copy_from_slice(&flags.to_le_bytes());
        Ok(self.cache.write_through(0, bytes)?)
    }

    /// How many clusters are free, counting them the first time.
    fn free_clusters(&self) -> Result<u32> {
        let mut space = self.space.lock();
        if let Some(free) = space.free {
            return Ok(free);
        }
        let clusters = self.geo.clusters;
        let mut used = 0;
        self.scan_bits(0, clusters, |first, bits| {
            let valid = clusters.saturating_sub(first).min(8);
            let mask = if valid == 8 { 0xff } else { (1u8 << valid) - 1 };
            used += (bits & mask).count_ones();
            true
        })?;
        let free = clusters.saturating_sub(used);
        space.free = Some(free);
        Ok(free)
    }

    /// The node for the set `found` of directory `dir`, whose entries are
    /// at `offsets` on the device: the one in use, or a new one. The
    /// caller holds `ns`.
    pub(crate) fn node_for(
        this: &KArc<Inner>,
        dir: &KArc<ExfatNode>,
        found: &Found,
        runs: &Runs,
    ) -> Result<KArc<ExfatNode>> {
        let key = key(dir.dir_key(), found.index);
        let mut nodes = this.nodes.lock();
        let at = nodes.binary_search_by_key(&key, |(k, _)| *k);
        if let Ok(i) = at
            && let Some(node) = nodes[i].1.upgrade()
        {
            return Ok(node);
        }
        let node = ExfatNode::load(this.clone(), dir.clone(), found, runs)?;
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

    /// The node in use for `key`, taken out of the table: its entries are
    /// going. The caller holds `ns`.
    pub(crate) fn take_node(&self, key: u64) -> Option<KArc<ExfatNode>> {
        let weak = {
            let mut nodes = self.nodes.lock();
            let i = nodes.binary_search_by_key(&key, |(k, _)| *k).ok()?;
            nodes.remove(i).1
        };
        weak.upgrade()
    }

    /// Moves `node` from `old` to `new` in the table, after a rename. The
    /// caller holds `ns`.
    pub(crate) fn rekey(&self, old: u64, new: u64, node: &KArc<ExfatNode>) -> Result<()> {
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
    fn live_nodes(&self) -> Result<KVec<KArc<ExfatNode>>> {
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
    pub(crate) fn pin(&self, node: KArc<ExfatNode>) -> core::result::Result<(), KArc<ExfatNode>> {
        let mut dirty = self.dirty.lock();
        if dirty.try_reserve(1).is_err() {
            return Err(node);
        }
        let _ = dirty.push_within_capacity(node);
        Ok(())
    }

    /// Writes every change to the disk: the nodes' data and entries, then
    /// the table, the bitmap and directories, then flushes the device;
    /// then clears the dirty bit, unless something changed meanwhile.
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
        self.cache.sync()?;
        if let Some(err) = failed {
            return Err(err);
        }
        // Every change goes into the block cache under `ns` or `space`,
        // and a node is pinned before it takes clusters, so with both held
        // a clean cache and no pinned node mean the disk has it all.
        let _ns = self.ns.lock();
        let mut space = self.space.lock();
        if space.marked && !self.cache.is_dirty() && self.dirty.lock().is_empty() {
            self.write_flags(self.geo.flags & !boot::flags::DIRTY)?;
            space.marked = false;
        }
        Ok(())
    }
}

/// The namespace's buffers, on the heap: they are larger than a kernel
/// stack frame may be (ADR 0058).
#[inline(never)]
fn names() -> Result<KBox<Names>> {
    const EMPTY: Names = Names::new();
    Ok(mem::boxed(EMPTY)?)
}
