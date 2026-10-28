// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! exFAT files and directories as VFS nodes.
//!
//! A node knows where its set's entries are, its first cluster, whether
//! its clusters need the table, and, once its contents are first used,
//! its cluster chain. A file's data goes through its page cache, whose
//! backing moves pages straight between the cache and the file's
//! clusters on the disk, a run of them in one request where the clusters
//! lie in a row; its size lives in the cache while the node is in memory,
//! and reaches the stream extension, with the first cluster and the
//! times, when the node is flushed. A node with changes stays in memory
//! until then (the volume pins it), so nothing is lost when the last
//! handle to it goes.
//!
//! exFAT keeps two sizes for a file: its size, and how much of it was
//! written (the valid data length); what lies past the second reads as
//! zeros, whatever the disk holds. So a file grown by `set_size` costs no
//! writes of zeros, as with Linux's driver: they are written only when
//! data is written past them, just before it.
//!
//! A file or directory removed while in use keeps its clusters until its
//! last reference goes, as on Linux; its entries are gone at once. Each
//! node holds its directory, so a directory's ancestors stay in memory
//! while it does, and a rename can tell a directory from one under it.

use alloc::sync::Weak;
use core::any::Any;
use core::sync::atomic::{AtomicBool, Ordering};

use radix_block::mem::CachePage;
use radix_block::{PAGE_SIZE, Page, SECTOR, Segment, SegmentMut};
use radix_fat::chain::Chain;
use radix_sync::{Mutex, Once};
use radix_vfs::cache::{Backing, FileCache, page_len};
use radix_vfs::mem::{self, Alloc, KArc, KVec};
use radix_vfs::path::check_name;
use radix_vfs::{Error, In, Kind, ListFn, Node, NodeRef, Out, Result, Stat, now};

use crate::dir::{Found, Names, Runs};
use crate::entry::{
    self, ENTRY, File, NAME_MAX, PER_NAME, Stamp, Stream, add_to_checksum, add_to_hash, attr,
    name_entry, stream,
};
use crate::fs::{Inner, key};

const PAGE: u64 = PAGE_SIZE as u64;

/// What a node knows about itself.
pub(crate) struct Meta {
    /// The directory its set is in; none for the root, and once it is
    /// removed.
    pub(crate) parent: Option<KArc<ExfatNode>>,
    /// Where its set's entries are on the device, in runs.
    pub(crate) set: KVec<(u64, u16)>,
    /// Its first cluster, 0 when it has none.
    pub(crate) first: u32,
    /// Whether its clusters lie in one run, without table entries.
    pub(crate) contiguous: bool,
    /// Its clusters, once loaded.
    pub(crate) chain: Option<Chain>,
    /// A directory's size: its clusters'. For a file, the length its
    /// entry gave when it was loaded, which its chain is first sized by;
    /// after, the file's size is its cache's.
    pub(crate) size: u64,
    /// The bytes of a file that were written; it reads as zeros after.
    pub(crate) valid: u64,
    pub(crate) attributes: u16,
    pub(crate) id: u64,
    pub(crate) created: u64,
    /// When a directory's entries last changed (a file's time is its
    /// cache's).
    pub(crate) modified: u64,
    /// When it was last read, to two seconds, as exFAT keeps it.
    pub(crate) accessed: u64,
    /// Whether the entries on disk are behind.
    pub(crate) dirty: bool,
    /// How many times they have fallen behind, so that a change made
    /// while they are stored is stored again ([`ExfatNode::store`]).
    pub(crate) changes: u64,
}

impl Meta {
    /// Notes that its entries on disk are behind.
    pub(crate) fn behind(&mut self) {
        self.dirty = true;
        self.changes = self.changes.wrapping_add(1);
    }
}

/// An exFAT file or directory.
pub struct ExfatNode {
    pub(crate) fs: KArc<Inner>,
    pub(crate) kind: Kind,
    /// Whether it is the root directory, which has no entries.
    pub(crate) root: bool,
    pub(crate) meta: Mutex<Meta>,
    /// A file's data.
    cache: Option<FileCache>,
    me: Once<Weak<ExfatNode, Alloc>>,
    pinned: AtomicBool,
}

/// The node number of the set at `index` of directory `dir`; 1 is the
/// root's.
pub(crate) fn id_of(dir: u32, index: u32) -> u64 {
    ((u64::from(dir) + 1) << 24) | u64::from(index)
}

impl ExfatNode {
    fn make(
        fs: KArc<Inner>,
        kind: Kind,
        root: bool,
        meta: Meta,
        size: u64,
    ) -> Result<KArc<ExfatNode>> {
        let cache = (kind == Kind::File).then(|| FileCache::new(size, meta.modified));
        let node = mem::arc(ExfatNode {
            fs,
            kind,
            root,
            meta: Mutex::new(meta),
            cache,
            me: Once::new(),
            pinned: AtomicBool::new(false),
        })?;
        let _ = node.me.set(KArc::downgrade(&node));
        Ok(node)
    }

    /// The root directory, whose clusters are `chain`.
    pub(crate) fn root(fs: KArc<Inner>, chain: Chain) -> Result<KArc<ExfatNode>> {
        let size = u64::from(chain.len()) * u64::from(fs.geo.cluster_size);
        let meta = Meta {
            parent: None,
            set: mem::vec(),
            first: fs.geo.root_cluster,
            contiguous: false,
            chain: Some(chain),
            size,
            valid: size,
            attributes: attr::DIRECTORY,
            id: 1,
            created: 0,
            modified: 0,
            accessed: 0,
            dirty: false,
            changes: 0,
        };
        ExfatNode::make(fs, Kind::Directory, true, meta, 0)
    }

    /// The node for the set `found` of directory `parent`, whose entries
    /// are at `runs`.
    pub(crate) fn load(
        fs: KArc<Inner>,
        parent: KArc<ExfatNode>,
        found: &Found,
        runs: &Runs,
    ) -> Result<KArc<ExfatNode>> {
        let (file, stream) = (found.file, found.stream);
        let kind = if file.is_dir() { Kind::Directory } else { Kind::File };
        let mut set = mem::vec_with_capacity(runs.len())?;
        for &run in runs {
            mem::push(&mut set, run)?;
        }
        let id = id_of(parent.dir_key(), found.index);
        let meta = Meta {
            parent: Some(parent),
            set,
            first: stream.first,
            contiguous: stream.contiguous(),
            chain: None,
            size: stream.size,
            valid: stream.valid.min(stream.size),
            attributes: file.attributes,
            id,
            created: file.created.ns(),
            modified: file.modified.ns(),
            accessed: file.accessed.ns(),
            dirty: false,
            changes: 0,
        };
        let size = if kind == Kind::File { stream.size } else { 0 };
        ExfatNode::make(fs, kind, false, meta, size)
    }

    /// The clusters a file of `size` bytes takes.
    fn clusters_for(&self, size: u64) -> Result<u32> {
        let cs = u64::from(self.fs.geo.cluster_size);
        u32::try_from(size.div_ceil(cs)).map_err(|_| Error::TooBig)
    }

    /// Its chain, loaded the first time: one run from its first cluster,
    /// or from the table.
    pub(crate) fn chain<'m>(&self, meta: &'m mut Meta) -> Result<&'m mut Chain> {
        if meta.chain.is_none() {
            // Not the cache's size: this runs under its lock.
            let needed = self.clusters_for(meta.size)?;
            let chain = if meta.first == 0 {
                if self.kind == Kind::Directory || needed != 0 {
                    return Err(Error::Corrupt);
                }
                Chain::new()
            } else if meta.contiguous {
                let geo = &self.fs.geo;
                if needed == 0 || !geo.valid(meta.first) || !geo.valid(meta.first + (needed - 1)) {
                    return Err(Error::Corrupt);
                }
                let mut chain = Chain::new();
                chain.push_run(meta.first, needed)?;
                chain
            } else {
                let chain = self.fs.follow(meta.first)?;
                if chain.len() < needed {
                    return Err(Error::Corrupt);
                }
                chain
            };
            if self.kind == Kind::Directory {
                meta.size = u64::from(chain.len()) * u64::from(self.fs.geo.cluster_size);
                meta.valid = meta.size;
            }
            meta.chain = Some(chain);
        }
        meta.chain.as_mut().ok_or(Error::Corrupt)
    }

    /// The first cluster of this directory, which its sets' keys name.
    pub(crate) fn dir_key(&self) -> u32 {
        self.meta.lock().first
    }

    /// Itself, counted.
    fn me(&self) -> Result<KArc<ExfatNode>> {
        self.me.get().and_then(Weak::upgrade).ok_or(Error::NotFound)
    }

    /// Whether it was removed.
    pub(crate) fn removed(&self) -> bool {
        !self.root && self.meta.lock().parent.is_none()
    }

    pub(crate) fn writable(&self) -> Result<()> {
        if self.fs.read_only { Err(Error::ReadOnly) } else { Ok(()) }
    }

    fn file(&self) -> Result<&FileCache> {
        self.cache.as_ref().ok_or(Error::IsDirectory)
    }

    fn is_dir(&self) -> Result<()> {
        if self.kind == Kind::Directory { Ok(()) } else { Err(Error::NotDirectory) }
    }

    /// Notes that a file's entries are behind, and keeps it in memory
    /// until the volume syncs. The caller holds no lock.
    fn changed(&self) {
        self.meta.lock().behind();
        if !self.pin() {
            // No memory to remember it: it is written now instead.
            let _ = self.flush();
        }
    }

    /// Keeps it in memory until the volume syncs; false if there was no
    /// memory to.
    pub(crate) fn pin(&self) -> bool {
        if self.pinned.swap(true, Ordering::AcqRel) {
            return true;
        }
        let pinned =
            self.me.get().and_then(Weak::upgrade).is_some_and(|me| self.fs.pin(me).is_ok());
        if !pinned {
            self.pinned.store(false, Ordering::Release);
        }
        pinned
    }

    /// The volume no longer holds it for syncing.
    pub(crate) fn unpinned(&self) {
        self.pinned.store(false, Ordering::Release);
    }

    /// Writes its data, then its entries.
    pub(crate) fn flush(&self) -> Result<()> {
        if let Some(cache) = &self.cache
            && !self.removed()
        {
            cache.sync(&FileBacking(self))?;
        }
        let _ns = self.fs.ns.lock();
        self.store()
    }

    /// Writes its first cluster, sizes, attributes and times to its set,
    /// with the set's new checksum. The caller holds `ns`.
    pub(crate) fn store(&self) -> Result<()> {
        // What the cache says is read without the node's lock, which ranks
        // below the cache's: a change made meanwhile is stored next time.
        let seen = self.meta.lock().changes;
        let (cached, modified) = match &self.cache {
            Some(cache) => (Some(cache.size()), Some(cache.modified())),
            None => (None, None),
        };
        let mut meta = self.meta.lock();
        if !meta.dirty {
            return Ok(());
        }
        if meta.parent.is_none() {
            meta.dirty = false;
            return Ok(());
        }
        let size = cached.unwrap_or(meta.size);
        let valid = if self.kind == Kind::File { meta.valid.min(size) } else { size };
        let modified = Stamp::of(modified.unwrap_or(meta.modified), true);
        let accessed = Stamp::of(meta.accessed, false);
        let mut flags = stream::ALLOCATION_POSSIBLE;
        if meta.contiguous && meta.first != 0 {
            flags |= stream::NO_FAT_CHAIN;
        }
        let mut attributes = meta.attributes;
        if self.kind == Kind::File {
            attributes |= attr::ARCHIVE;
        }
        // The file entry and the stream extension change; the checksum
        // runs over every entry of the set.
        let mut head = [[0u8; ENTRY]; 2];
        let mut sum = 0;
        let mut n = 0;
        for &(at, count) in &meta.set {
            for k in 0..u64::from(count) {
                let mut bytes = [0u8; ENTRY];
                let offset = at + k * ENTRY as u64;
                self.fs.cache.read(offset, &mut bytes)?;
                match n {
                    0 => {
                        bytes[4..6].copy_from_slice(&attributes.to_le_bytes());
                        bytes[12..16].copy_from_slice(&modified.stamp.to_le_bytes());
                        bytes[16..20].copy_from_slice(&accessed.stamp.to_le_bytes());
                        bytes[21] = modified.hundredths;
                        bytes[23] = modified.utc;
                        bytes[24] = accessed.utc;
                        head[0] = bytes;
                    }
                    1 => {
                        bytes[1] = flags;
                        bytes[8..16].copy_from_slice(&valid.to_le_bytes());
                        bytes[20..24].copy_from_slice(&meta.first.to_le_bytes());
                        bytes[24..32].copy_from_slice(&size.to_le_bytes());
                        head[1] = bytes;
                    }
                    _ => {}
                }
                sum = add_to_checksum(sum, n, &bytes);
                n += 1;
            }
        }
        if n < 2 {
            return Err(Error::Corrupt);
        }
        head[0][2..4].copy_from_slice(&sum.to_le_bytes());
        self.fs.changing()?;
        for (n, bytes) in head.iter().enumerate() {
            self.fs.cache.write(offset_of(&meta.set, n).ok_or(Error::Corrupt)?, bytes)?;
        }
        meta.dirty = meta.changes != seen;
        Ok(())
    }

    /// Drops up to `max` clean pages of a file.
    pub(crate) fn shrink(&self, max: usize) -> usize {
        match &self.cache {
            Some(cache) => cache.shrink(max, &FileBacking(self)),
            None => 0,
        }
    }

    /// Marks a directory changed now. The caller holds `ns`; without the
    /// memory to keep the directory until the volume syncs, the time is
    /// not kept, which loses nothing else.
    fn touch(&self) {
        {
            let mut meta = self.meta.lock();
            meta.modified = now();
            meta.behind();
        }
        let _ = self.pin();
    }

    /// The device pieces of the file's bytes `start..start + len` (whole
    /// sectors, inside one page): (device sector, offset from `start`,
    /// length).
    fn pieces(&self, start: u64, len: usize) -> Result<Pieces> {
        let geo = &self.fs.geo;
        let size = u64::from(geo.cluster_size);
        let mut meta = self.meta.lock();
        let chain = self.chain(&mut meta)?;
        let mut pieces = Pieces { list: [(0, 0, 0); PIECES], count: 0 };
        let mut done = 0;
        while done < len {
            let at = start + done as u64;
            let index = u32::try_from(at / size).map_err(|_| Error::Corrupt)?;
            let within = at % size;
            let (cluster, run) = chain.extent(index).ok_or(Error::Corrupt)?;
            let room = u64::from(run) * size - within;
            let n = (len - done).min(room.min(PAGE) as usize);
            let byte = geo.cluster_offset(cluster) + within;
            let merged = match pieces.count.checked_sub(1).map(|i| &mut pieces.list[i]) {
                Some((sector, _, plen)) if *sector * SECTOR + *plen as u64 == byte => {
                    *plen += n;
                    true
                }
                _ => false,
            };
            if !merged {
                if pieces.count == PIECES {
                    return Err(Error::Corrupt);
                }
                pieces.list[pieces.count] = (byte / SECTOR, done, n);
                pieces.count += 1;
            }
            done += n;
        }
        Ok(pieces)
    }

    /// Where the file's whole pages from byte `pos` lie in one run on the
    /// disk, up to `max` of them and none past the written data: the
    /// device byte, and how many; or how many bytes were written, when
    /// `pos` is past them.
    fn run_at(&self, pos: u64, max: usize) -> Result<core::result::Result<(u64, usize), u64>> {
        let geo = &self.fs.geo;
        let cs = u64::from(geo.cluster_size);
        let mut meta = self.meta.lock();
        let valid = meta.valid;
        if pos >= valid {
            return Ok(Err(valid));
        }
        let chain = self.chain(&mut meta)?;
        let index = u32::try_from(pos / cs).map_err(|_| Error::Corrupt)?;
        let (cluster, run) = chain.extent(index).ok_or(Error::Corrupt)?;
        let within = pos % cs;
        let room = u64::from(run) * cs - within;
        let pages = (room / PAGE).min((valid - pos).div_ceil(PAGE)).min(max as u64) as usize;
        Ok(Ok((geo.cluster_offset(cluster) + within, pages)))
    }

    /// Its clusters' bytes.
    fn allocated(&self) -> u64 {
        let size = u64::from(self.fs.geo.cluster_size);
        let mut meta = self.meta.lock();
        match self.chain(&mut meta) {
            Ok(chain) => u64::from(chain.len()) * size,
            Err(_) => 0,
        }
    }

    /// Writes zeros over the file's bytes `from..to` on the disk: what
    /// lies between the data written and data about to be. `to` is a
    /// whole page.
    fn zero_disk(&self, from: u64, to: u64) -> Result<()> {
        let device = self.fs.cache.device();
        let ss = u64::from(self.fs.geo.sector_size);
        let mut page = CachePage::new()?;
        let mut at = from - from % ss;
        if at != from {
            // The sector the data ends in keeps the data.
            let within = (from - at) as usize;
            let pieces = self.pieces(at, ss as usize)?;
            for (sector, offset, len) in pieces.iter() {
                device.read(sector, &mut [SegmentMut { page: &mut page, offset, len }])?;
            }
            radix_hw::device_wrote(&mut page.bytes_mut()[..ss as usize]);
            page.bytes_mut()[within..].fill(0);
            for (sector, offset, len) in pieces.iter() {
                device.write(sector, &[Segment { page: &page, offset, len }])?;
            }
            page.bytes_mut().fill(0);
            at += ss;
        }
        while at < to {
            let n = (to - at).min(PAGE - at % PAGE) as usize;
            for (sector, offset, len) in self.pieces(at, n)?.iter() {
                device.write(sector, &[Segment { page: &page, offset, len }])?;
            }
            at += n as u64;
        }
        Ok(())
    }

    /// Whether `dir` is this directory or lies under it.
    fn holds(&self, dir: &KArc<ExfatNode>) -> bool {
        let mut current = dir.clone();
        loop {
            if core::ptr::eq(KArc::as_ptr(&current), self) {
                return true;
            }
            let parent = current.meta.lock().parent.clone();
            match parent {
                Some(parent) => current = parent,
                None => return false,
            }
        }
    }
}

/// The device offset of entry `n` of a set at `runs`.
fn offset_of(runs: &Runs, mut n: usize) -> Option<u64> {
    for &(at, count) in runs {
        if n < usize::from(count) {
            return Some(at + (n * ENTRY) as u64);
        }
        n -= usize::from(count);
    }
    None
}

/// The most pieces a page is in: one per cluster, at 512 bytes.
const PIECES: usize = PAGE_SIZE / 512;

struct Pieces {
    list: [(u64, usize, usize); PIECES],
    count: usize,
}

impl Pieces {
    fn iter(&self) -> impl Iterator<Item = (u64, usize, usize)> + '_ {
        self.list[..self.count].iter().copied()
    }
}

/// A file's bytes `len` rounded up to whole sectors of the volume's
/// (`sector` bytes, no smaller than the disk's logical blocks).
fn sectors_of(len: usize, sector: u32) -> usize {
    len.next_multiple_of(sector as usize)
}

/// The most pages one request to the disk moves.
const MAX_SEGMENTS: usize = 256;

/// A file's page cache backing: its clusters.
struct FileBacking<'a>(&'a ExfatNode);

impl FileBacking<'_> {
    /// Writes the first `len` bytes of page `index` alone, through its
    /// pieces.
    fn write_one(&self, index: u64, page: &Page, len: usize) -> Result<()> {
        let whole = sectors_of(len, self.0.fs.geo.sector_size);
        let pieces = self.0.pieces(index * PAGE, whole)?;
        let device = self.0.fs.cache.device();
        for (sector, offset, len) in pieces.iter() {
            device.write(sector, &[Segment { page, offset, len }])?;
        }
        Ok(())
    }
}

impl Backing for FileBacking<'_> {
    fn persistent(&self) -> bool {
        true
    }

    fn read_page(&self, index: u64, page: &mut Page, len: usize) -> Result<()> {
        let node = self.0;
        let pos = index * PAGE;
        let valid = node.meta.lock().valid;
        // Past the data written, the file reads as zeros.
        let upto = (valid.saturating_sub(pos).min(len as u64)) as usize;
        let whole = sectors_of(upto, node.fs.geo.sector_size);
        if whole > 0 {
            let pieces = node.pieces(pos, whole)?;
            let device = node.fs.cache.device();
            for (sector, offset, len) in pieces.iter() {
                device.read(sector, &mut [SegmentMut { page: &mut *page, offset, len }])?;
            }
            radix_hw::device_wrote(&mut page.bytes_mut()[..whole]);
        }
        let end = sectors_of(len, node.fs.geo.sector_size).max(whole);
        page.bytes_mut()[upto..end].fill(0);
        Ok(())
    }

    fn read_run(
        &self,
        index: u64,
        pages: &mut [CachePage],
        size: u64,
        _more: &mut dyn FnMut(u64, &[u8]) -> bool,
    ) -> Result<usize> {
        let node = self.0;
        let pos = index * PAGE;
        let max =
            pages.len().min(node.fs.cache.device().info().max_segments.clamp(1, MAX_SEGMENTS));
        let (byte, n) = match node.run_at(pos, max)? {
            Ok((byte, n)) if n > 1 => (byte, n),
            _ => {
                let page = pages.first_mut().ok_or(Error::Io)?;
                self.read_page(index, page, page_len(index, size))?;
                return Ok(1);
            }
        };
        let valid = node.meta.lock().valid;
        {
            let mut segments = mem::vec_with_capacity(n)?;
            for page in &mut pages[..n] {
                mem::push(&mut segments, SegmentMut { page, offset: 0, len: PAGE_SIZE })?;
            }
            node.fs.cache.device().read(byte / SECTOR, &mut segments)?;
        }
        for (at, page) in (index..).zip(&mut pages[..n]) {
            radix_hw::device_wrote(page.bytes_mut());
            // Past the file's end, and past the data written, it is zeros.
            let keep = valid.min(size).saturating_sub(at * PAGE).min(PAGE) as usize;
            page.bytes_mut()[keep..].fill(0);
        }
        Ok(n)
    }

    fn run_pages(&self) -> usize {
        radix_vfs::cache::READ_RUN
    }

    fn write_page(&self, index: u64, page: &Page, len: usize) -> Result<()> {
        self.write_pages(index, &[page], len)
    }

    fn write_pages(&self, index: u64, pages: &[&Page], len: usize) -> Result<()> {
        let node = self.0;
        let Some(last) = pages.len().checked_sub(1) else { return Ok(()) };
        let pos = index * PAGE;
        let end = pos + last as u64 * PAGE + len as u64;
        // What lies between the data written and these pages reads as
        // zeros: it must be zeros on the disk before the data written
        // reaches past it.
        let valid = node.meta.lock().valid;
        if pos > valid {
            node.zero_disk(valid, pos)?;
        }
        let max = node.fs.cache.device().info().max_segments.clamp(1, MAX_SEGMENTS);
        let sector = node.fs.geo.sector_size;
        let mut k = 0;
        while k < pages.len() {
            let at = pos + k as u64 * PAGE;
            // Whole pages in a row on the disk go in one request; the
            // written data does not limit them.
            let run = {
                let geo = &node.fs.geo;
                let cs = u64::from(geo.cluster_size);
                let mut meta = node.meta.lock();
                let chain = node.chain(&mut meta)?;
                let index = u32::try_from(at / cs).map_err(|_| Error::Corrupt)?;
                let (cluster, run) = chain.extent(index).ok_or(Error::Corrupt)?;
                let room = u64::from(run) * cs - at % cs;
                (geo.cluster_offset(cluster) + at % cs, (room / PAGE) as usize)
            };
            let n = run.1.min(pages.len() - k).min(max);
            if n == 0 {
                let n = if k == last { len } else { PAGE_SIZE };
                self.write_one(index + k as u64, pages[k], n)?;
                k += 1;
                continue;
            }
            let mut segments = mem::vec_with_capacity(n)?;
            for (i, &page) in pages.iter().enumerate().skip(k).take(n) {
                let len = if i == last { sectors_of(len, sector) } else { PAGE_SIZE };
                mem::push(&mut segments, Segment { page, offset: 0, len })?;
            }
            node.fs.cache.device().write(run.0 / SECTOR, &segments)?;
            k += n;
        }
        let mut meta = node.meta.lock();
        if end > meta.valid {
            meta.valid = end;
            meta.behind();
        }
        Ok(())
    }

    fn reserve(&self, size: u64) -> Result<()> {
        let node = self.0;
        let needed = node.clusters_for(size)?;
        // The file grows, which its entries must say. It is pinned before
        // it takes clusters, so that a sync meanwhile keeps the volume
        // marked dirty until they do.
        if !node.pin() {
            return Err(Error::NoMemory);
        }
        let mut meta = node.meta.lock();
        node.chain(&mut meta)?;
        let Meta { chain, first, contiguous, .. } = &mut *meta;
        let Some(chain) = chain else { return Err(Error::Corrupt) };
        if chain.len() < needed {
            if chain.is_empty() {
                *contiguous = true;
            }
            node.fs.grow(chain, first, contiguous, needed, false)?;
        }
        meta.behind();
        Ok(())
    }

    fn zero(&self, from: u64, to: u64) -> Result<()> {
        // What lies past the data written reads as zeros already.
        let valid = self.0.meta.lock().valid;
        if from < valid {
            self.0.zero_disk(from, to.min(valid.next_multiple_of(PAGE)))?;
        }
        Ok(())
    }

    fn truncate(&self, size: u64) -> Result<()> {
        let node = self.0;
        let keep = node.clusters_for(size)?;
        if !node.pin() {
            return Err(Error::NoMemory);
        }
        let mut meta = node.meta.lock();
        node.chain(&mut meta)?;
        let Meta { chain, first, contiguous, valid, .. } = &mut *meta;
        if let Some(chain) = chain
            && keep < chain.len()
        {
            node.fs.cut(chain, first, contiguous, keep, false)?;
        }
        *valid = (*valid).min(size);
        meta.behind();
        Ok(())
    }

    fn max_size(&self) -> u64 {
        let geo = &self.0.fs.geo;
        u64::from(geo.clusters) * u64::from(geo.cluster_size)
    }
}

impl Drop for ExfatNode {
    fn drop(&mut self) {
        // A removed node's clusters were kept for it; they go with it.
        if self.root || self.fs.read_only || self.meta.get_mut().parent.is_some() {
            return;
        }
        let mut meta = core::mem::replace(
            self.meta.get_mut(),
            Meta {
                parent: None,
                set: mem::vec(),
                first: 0,
                contiguous: false,
                chain: None,
                size: 0,
                valid: 0,
                attributes: 0,
                id: 0,
                created: 0,
                modified: 0,
                accessed: 0,
                dirty: false,
                changes: 0,
            },
        );
        if self.chain(&mut meta).is_err() {
            return;
        }
        let Meta { chain, first, contiguous, .. } = &mut meta;
        if let Some(chain) = chain {
            let _ = self.fs.cut(chain, first, contiguous, 0, self.kind == Kind::Directory);
        }
    }
}

/// The error for a name exFAT cannot hold.
fn name_error(err: entry::NameError) -> Error {
    match err {
        entry::NameError::TooLong => Error::NameTooLong,
        entry::NameError::Invalid => Error::BadName,
    }
}

/// Whether `moving` may replace `old`: a file a file, a directory an
/// empty directory.
fn check_replace(moving: Kind, old: &ExfatNode) -> Result<()> {
    match (moving, old.kind) {
        (_, Kind::Directory) if !old.is_empty_dir()? => Err(Error::NotEmpty),
        (Kind::Directory, Kind::File) => Err(Error::NotDirectory),
        (Kind::File, Kind::Directory) => Err(Error::IsDirectory),
        _ => Ok(()),
    }
}

/// Entry `k` of the set of a file entry `file`, stream extension `stream`
/// and name `units`.
fn set_entry(file: &File, stream: &Stream, units: &[u16], k: usize) -> [u8; ENTRY] {
    match k {
        0 => file.encode(),
        1 => stream.encode(),
        _ => {
            let at = (k - 2) * PER_NAME;
            name_entry(&units[at.min(units.len())..(at + PER_NAME).min(units.len())])
        }
    }
}

/// The set's file entry with its checksum, for `count` entries.
fn sealed(mut file: File, stream: &Stream, units: &[u16], count: usize) -> File {
    file.checksum = 0;
    let sum =
        (0..count).fold(0, |sum, k| add_to_checksum(sum, k, &set_entry(&file, stream, units, k)));
    file.checksum = sum;
    file
}

impl ExfatNode {
    /// Writes the set of `file` and `stream` under the name `name` into
    /// this directory; returns where it went, and the set.
    fn put_named(
        &self,
        names: &mut Names,
        name: &str,
        file: File,
        mut stream: Stream,
    ) -> Result<Found> {
        let mut units = [0u16; NAME_MAX];
        let len = entry::units(name, &mut units).map_err(name_error)?;
        let units = &units[..len];
        stream.name_len = len as u8;
        stream.hash = units.iter().fold(0, |hash, &u| add_to_hash(hash, self.fs.upcase.upper(u)));
        let count = 2 + len.div_ceil(PER_NAME);
        let file = sealed(File { secondaries: (count - 1) as u8, ..file }, &stream, units, count);
        let index = self.put(names, count as u32, |k| set_entry(&file, &stream, units, k))?;
        Ok(Found { index, count: count as u32, file, stream })
    }

    /// Reads the file entry and stream extension of the set at `runs`.
    fn read_head(&self, runs: &Runs) -> Result<(File, Stream)> {
        let mut bytes = [0u8; ENTRY];
        self.fs.cache.read(offset_of(runs, 0).ok_or(Error::Corrupt)?, &mut bytes)?;
        let file = File::decode(&bytes);
        self.fs.cache.read(offset_of(runs, 1).ok_or(Error::Corrupt)?, &mut bytes)?;
        Ok((file, Stream::decode(&bytes)))
    }

    /// Makes a directory's first cluster, zeroed, in one run.
    fn new_dir_cluster(&self) -> Result<(Chain, u32)> {
        let mut chain = Chain::new();
        let (mut first, mut contiguous) = (0, true);
        self.fs.grow(&mut chain, &mut first, &mut contiguous, 1, true)?;
        Ok((chain, first))
    }

    /// Frees what [`ExfatNode::new_dir_cluster`] made.
    fn free_dir_cluster(&self, mut chain: Chain, mut first: u32) {
        let mut contiguous = true;
        let _ = self.fs.cut(&mut chain, &mut first, &mut contiguous, 0, true);
    }
}

impl Node for ExfatNode {
    fn stat(&self) -> Stat {
        let (id, created, dir_modified, accessed) = {
            let meta = self.meta.lock();
            (meta.id, meta.created, meta.modified, meta.accessed)
        };
        let cluster = u64::from(self.fs.geo.cluster_size);
        let (size, allocated, modified) = match &self.cache {
            Some(cache) => {
                let (size, modified, _) = cache.status();
                (size, size.next_multiple_of(cluster), modified)
            }
            None => {
                let allocated = self.allocated();
                (allocated, allocated, dir_modified)
            }
        };
        Stat { kind: self.kind, size, allocated, id, created, modified, accessed }
    }

    fn kind(&self) -> Kind {
        self.kind
    }

    fn lookup(&self, name: &str) -> Result<NodeRef> {
        self.is_dir()?;
        let me = self.me()?;
        let mut ns = self.fs.ns.lock();
        let found = self.find(&mut ns, name, &[])?.ok_or(Error::NotFound)?;
        Ok(Inner::node_for(&self.fs, &me, &found, ns.runs())?)
    }

    fn create(&self, name: &str, kind: Kind) -> Result<NodeRef> {
        self.writable()?;
        check_name(name)?;
        self.is_dir()?;
        if !matches!(kind, Kind::File | Kind::Directory) {
            return Err(Error::NotSupported);
        }
        let me = self.me()?;
        let mut ns = self.fs.ns.lock();
        if self.removed() {
            return Err(Error::NotFound);
        }
        // A name that cannot be stored is refused before a directory's
        // cluster is taken for it.
        entry::units(name, &mut [0; NAME_MAX]).map_err(name_error)?;
        if self.find(&mut ns, name, &[])?.is_some() {
            return Err(Error::Exists);
        }
        let time = now();
        let file = File {
            attributes: if kind == Kind::Directory { attr::DIRECTORY } else { attr::ARCHIVE },
            created: Stamp::of(time, true),
            modified: Stamp::of(time, true),
            accessed: Stamp::of(time, false),
            ..File::default()
        };
        let mut stream = Stream { flags: stream::ALLOCATION_POSSIBLE, ..Stream::default() };
        let made = if kind == Kind::Directory {
            let (chain, first) = self.new_dir_cluster()?;
            let size = u64::from(self.fs.geo.cluster_size);
            stream = Stream {
                flags: stream::ALLOCATION_POSSIBLE | stream::NO_FAT_CHAIN,
                first,
                size,
                valid: size,
                ..stream
            };
            Some((chain, first))
        } else {
            None
        };
        let found = match self.put_named(&mut ns, name, file, stream) {
            Ok(found) => found,
            Err(err) => {
                if let Some((chain, first)) = made {
                    self.free_dir_cluster(chain, first);
                }
                return Err(err);
            }
        };
        self.touch();
        let node = Inner::node_for(&self.fs, &me, &found, ns.runs())?;
        if let Some((chain, _)) = made {
            node.meta.lock().chain = Some(chain);
        }
        Ok(node)
    }

    fn remove(&self, name: &str) -> Result<()> {
        self.writable()?;
        check_name(name)?;
        self.is_dir()?;
        let me = self.me()?;
        let mut ns = self.fs.ns.lock();
        let found = self.find(&mut ns, name, &[])?.ok_or(Error::NotFound)?;
        let node = Inner::node_for(&self.fs, &me, &found, ns.runs())?;
        if node.kind == Kind::Directory && !node.is_empty_dir()? {
            return Err(Error::NotEmpty);
        }
        self.delete(&found)?;
        self.fs.take_node(key(self.dir_key(), found.index));
        // Its clusters go when the last reference to it does: now, unless
        // a handle has it.
        let parent = node.meta.lock().parent.take();
        drop(parent);
        drop(node);
        self.touch();
        Ok(())
    }

    fn rename(&self, from: &str, to_dir: &dyn Node, to: &str) -> Result<()> {
        self.writable()?;
        check_name(from)?;
        check_name(to)?;
        self.is_dir()?;
        let target = radix_vfs::downcast::<ExfatNode>(to_dir).ok_or(Error::CrossMount)?;
        if !KArc::ptr_eq(&target.fs, &self.fs) {
            return Err(Error::CrossMount);
        }
        target.is_dir()?;
        let (me, target_me) = (self.me()?, target.me()?);
        let mut ns = self.fs.ns.lock();
        if self.removed() || target.removed() {
            return Err(Error::NotFound);
        }
        let same_dir = core::ptr::eq(self, target);
        let source = self.find(&mut ns, from, &[])?.ok_or(Error::NotFound)?;
        if same_dir && from == to {
            return Ok(());
        }
        entry::units(to, &mut [0; NAME_MAX]).map_err(name_error)?;
        let (dir, to_key) = (self.dir_key(), target.dir_key());
        let moving = Inner::node_for(&self.fs, &me, &source, ns.runs())?;
        if moving.kind == Kind::Directory && !same_dir && moving.holds(&target_me) {
            return Err(Error::BadName);
        }
        // What it replaces, unless that is itself under another spelling.
        let replaced = match target.find(&mut ns, to, &[])? {
            Some(old) if same_dir && old.index == source.index => None,
            Some(old) => {
                let node = Inner::node_for(&self.fs, &target_me, &old, ns.runs())?;
                check_replace(moving.kind, &node)?;
                Some((old, node))
            }
            None => None,
        };
        // The set is copied as it is now, with what is in memory.
        moving.store()?;
        let (file, stream) = {
            let meta = moving.meta.lock();
            self.read_head(&meta.set)?
        };
        // The new set is written before the old one goes, so that a
        // failure leaves the file where it was.
        let found = target.put_named(&mut ns, to, file, stream)?;
        let mut set = mem::vec_with_capacity(ns.runs().len())?;
        for &run in ns.runs() {
            mem::push(&mut set, run)?;
        }
        if let Some((old, node)) = replaced {
            target.delete(&old)?;
            self.fs.take_node(key(to_key, old.index));
            let parent = node.meta.lock().parent.take();
            drop(parent);
            drop(node);
        }
        self.delete(&source)?;
        let old_parent = {
            let mut meta = moving.meta.lock();
            meta.set = set;
            meta.id = crate::node::id_of(to_key, found.index);
            meta.parent.replace(target_me)
        };
        drop(old_parent);
        self.fs.rekey(key(dir, source.index), key(to_key, found.index), &moving)?;
        self.touch();
        if !same_dir {
            target.touch();
        }
        Ok(())
    }

    fn list(&self, cursor: u64, f: ListFn<'_>) -> Result<()> {
        self.is_dir()?;
        let mut ns = self.fs.ns.lock();
        let from = u32::try_from(cursor).unwrap_or(u32::MAX);
        self.scan(&mut ns, from, |visit, text| {
            let kind = if visit.found.file.is_dir() { Kind::Directory } else { Kind::File };
            text.set_units(visit.units);
            f(u64::from(visit.found.index + visit.found.count), text.as_str(), kind)
        })
    }

    fn read(&self, offset: u64, out: &mut dyn Out) -> Result<usize> {
        self.file()?.read(offset, out, &FileBacking(self))
    }

    fn write(&self, offset: u64, data: &dyn In) -> Result<usize> {
        self.writable()?;
        let result = self.file()?.write(offset, data, &FileBacking(self));
        self.changed();
        result
    }

    fn set_size(&self, size: u64) -> Result<()> {
        self.writable()?;
        let result = self.file()?.set_size(size, &FileBacking(self));
        self.changed();
        result
    }

    fn set_times(&self, accessed: Option<u64>, modified: Option<u64>) -> Result<()> {
        self.writable()?;
        // As exFAT keeps them, so that what `stat` says now is what the
        // disk will say: a change time in hundredths of a second, a read's
        // in two-second steps.
        let modified = modified.map(|t| Stamp::of(t, true).ns());
        let accessed = accessed.map(|t| Stamp::of(t, false).ns());
        // A file's change time is its cache's; the cache's lock comes
        // before the node's (write-back takes it), so they are taken one
        // at a time.
        if let (Some(modified), Some(cache)) = (modified, &self.cache) {
            cache.set_modified(modified);
        }
        {
            let mut meta = self.meta.lock();
            if let Some(accessed) = accessed {
                meta.accessed = accessed;
            }
            if let (Some(modified), None) = (modified, &self.cache) {
                meta.modified = modified;
            }
        }
        self.changed();
        Ok(())
    }

    fn sync(&self) -> Result<()> {
        // The whole volume: the bitmap and table a file's clusters are in
        // are shared, and the volume is then consistent on the disk.
        self.fs.sync_all()
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}
