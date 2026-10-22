// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! FAT files and directories as VFS nodes.
//!
//! A node knows where its short directory entry is, its first cluster
//! and, once its contents are first used, its cluster chain. A file's
//! data goes through its page cache, whose backing moves each page
//! straight between the cache and the file's clusters on the disk; its
//! size lives in the cache while the node is in memory, and reaches the
//! directory entry, with the first cluster and the time, when the node
//! is flushed. A node with changes stays in memory until then (the
//! volume pins it), so nothing is lost when the last handle to it goes.
//!
//! A file or directory removed while in use keeps its clusters until its
//! last reference goes, as on Linux; its entry is gone at once.

use alloc::sync::Weak;
use core::any::Any;
use core::sync::atomic::{AtomicBool, Ordering};

use radix_block::mem::CachePage;
use radix_block::{PAGE_SIZE, Page, SECTOR, Segment, SegmentMut};
use radix_sync::{Mutex, Once};
use radix_vfs::cache::{Backing, FileCache};
use radix_vfs::mem::{self, Alloc, KArc};
use radix_vfs::path::check_name;
use radix_vfs::{Error, In, Kind, ListFn, Node, NodeRef, Out, Result, Stat, now};

use crate::chain::Chain;
use crate::dir::Names;
use crate::fs::{Inner, key};
use crate::name::{self, MAX_LONG, Short, ShortForm, attr, checksum, long_entries, short_form};
use crate::table::Width;
use crate::time::{from_fat, to_fat};

/// The largest FAT file: its size is 32 bits.
const MAX_SIZE: u64 = u32::MAX as u64;

/// What a node knows about itself.
pub(crate) struct Meta {
    /// Where its short entry is on the device; none for the root, and
    /// once it is removed.
    pub(crate) loc: Option<u64>,
    /// Its first cluster, 0 when it has none.
    pub(crate) first: u32,
    /// Its clusters, once loaded.
    pub(crate) chain: Option<Chain>,
    pub(crate) id: u64,
    pub(crate) created: u64,
    /// When a directory's entries last changed (a file's time is its
    /// cache's).
    pub(crate) modified: u64,
    /// Whether the entry on disk is behind.
    pub(crate) dirty: bool,
}

/// A FAT file or directory.
pub struct FatNode {
    pub(crate) fs: KArc<Inner>,
    pub(crate) kind: Kind,
    /// Whether it is the root directory, which has no entry.
    pub(crate) root: bool,
    pub(crate) meta: Mutex<Meta>,
    /// A file's data.
    cache: Option<FileCache>,
    me: Once<Weak<FatNode, Alloc>>,
    pinned: AtomicBool,
}

/// The node number of the entry at `index` of directory `dir`; 1 is the
/// root's.
pub(crate) fn id_of(dir: u32, index: u32) -> u64 {
    ((u64::from(dir) + 1) << 16) | u64::from(index)
}

/// Nanoseconds since 1970 of an entry's creation time.
fn created_of(short: &Short) -> u64 {
    let base = from_fat(short.created_date, short.created_time);
    if base == 0 { 0 } else { base + u64::from(short.created_tenths.min(199)) * 10_000_000 }
}

impl FatNode {
    fn make(
        fs: KArc<Inner>,
        kind: Kind,
        root: bool,
        meta: Meta,
        size: u64,
    ) -> Result<KArc<FatNode>> {
        let cache = (kind == Kind::File).then(|| FileCache::new(size, meta.modified));
        let node = mem::arc(FatNode {
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

    /// The root directory.
    pub(crate) fn root(fs: KArc<Inner>) -> Result<KArc<FatNode>> {
        let first = if fs.geo.width == Width::Fat32 { fs.geo.root_cluster } else { 0 };
        let meta =
            Meta { loc: None, first, chain: None, id: 1, created: 0, modified: 0, dirty: false };
        FatNode::make(fs, Kind::Directory, true, meta, 0)
    }

    /// The node for the short entry `short` at `index` of directory
    /// `dir`, `offset` on the device.
    pub(crate) fn load(
        fs: KArc<Inner>,
        dir: u32,
        index: u32,
        offset: u64,
        short: &Short,
    ) -> Result<KArc<FatNode>> {
        let kind = if short.is_dir() { Kind::Directory } else { Kind::File };
        let meta = Meta {
            loc: Some(offset),
            first: short.cluster,
            chain: None,
            id: id_of(dir, index),
            created: created_of(short),
            modified: from_fat(short.modified_date, short.modified_time),
            dirty: false,
        };
        let size = if kind == Kind::File { u64::from(short.size) } else { 0 };
        FatNode::make(fs, kind, false, meta, size)
    }

    /// Its chain, loaded from the table the first time.
    pub(crate) fn chain<'m>(&self, meta: &'m mut Meta) -> Result<&'m mut Chain> {
        if meta.chain.is_none() {
            if self.kind == Kind::Directory && meta.first == 0 && !self.root {
                return Err(Error::Corrupt);
            }
            meta.chain = Some(self.fs.load_chain(meta.first)?);
        }
        meta.chain.as_mut().ok_or(Error::Corrupt)
    }

    /// The first cluster of this directory, which its entries' locations
    /// name; 0 for the fixed root.
    pub(crate) fn dir_key(&self) -> u32 {
        self.meta.lock().first
    }

    /// Whether it was removed.
    pub(crate) fn removed(&self) -> bool {
        !self.root && self.meta.lock().loc.is_none()
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

    /// Notes that a file's entry is behind, and keeps it in memory until
    /// the volume syncs. The caller holds no lock.
    fn changed(&self) {
        self.meta.lock().dirty = true;
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

    /// Writes its data, then its entry.
    pub(crate) fn flush(&self) -> Result<()> {
        if let Some(cache) = &self.cache
            && !self.removed()
        {
            cache.sync(&FileBacking(self))?;
        }
        let _ns = self.fs.ns.lock();
        self.store()
    }

    /// Writes its first cluster, size and time to its entry. The caller
    /// holds `ns`.
    pub(crate) fn store(&self) -> Result<()> {
        let (size, modified) = match &self.cache {
            Some(cache) => (cache.size(), Some(cache.modified())),
            None => (0, None),
        };
        let mut meta = self.meta.lock();
        if !meta.dirty {
            return Ok(());
        }
        let Some(at) = meta.loc else {
            meta.dirty = false;
            return Ok(());
        };
        let mut bytes = [0u8; name::ENTRY];
        self.fs.cache.read(at, &mut bytes)?;
        let mut short = Short::decode(&bytes);
        let modified = modified.unwrap_or(meta.modified);
        let (date, time) = to_fat(modified);
        short.cluster = meta.first;
        short.size = size as u32;
        short.modified_date = date;
        short.modified_time = time;
        short.accessed_date = date;
        if self.kind == Kind::File {
            short.attr |= attr::ARCHIVE;
        }
        self.fs.cache.write(at, &short.encode())?;
        meta.dirty = false;
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
            meta.dirty = true;
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
            let n = (len - done).min(room.min(PAGE_SIZE as u64) as usize);
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

    /// Its clusters' bytes.
    fn allocated(&self) -> u64 {
        let size = u64::from(self.fs.geo.cluster_size);
        let mut meta = self.meta.lock();
        match self.chain(&mut meta) {
            Ok(chain) => u64::from(chain.len()) * size,
            Err(_) => 0,
        }
    }
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

/// A file's bytes `len` rounded up to whole sectors.
fn sectors_of(len: usize) -> usize {
    len.next_multiple_of(SECTOR as usize)
}

/// A file's page cache backing: its clusters.
struct FileBacking<'a>(&'a FatNode);

impl Backing for FileBacking<'_> {
    fn persistent(&self) -> bool {
        true
    }

    fn read_page(&self, index: u64, page: &mut Page, len: usize) -> Result<()> {
        let whole = sectors_of(len);
        let pieces = self.0.pieces(index * PAGE_SIZE as u64, whole)?;
        let device = self.0.fs.cache.device();
        for (sector, offset, len) in pieces.iter() {
            device.read(sector, &mut [SegmentMut { page: &mut *page, offset, len }])?;
        }
        // The disk's bytes past the end of the file are not the file's.
        radix_hw::device_wrote(&mut page.bytes_mut()[..whole]);
        page.bytes_mut()[len..whole].fill(0);
        Ok(())
    }

    fn write_page(&self, index: u64, page: &Page, len: usize) -> Result<()> {
        let pieces = self.0.pieces(index * PAGE_SIZE as u64, sectors_of(len))?;
        let device = self.0.fs.cache.device();
        for (sector, offset, len) in pieces.iter() {
            device.write(sector, &[Segment { page, offset, len }])?;
        }
        Ok(())
    }

    fn reserve(&self, size: u64) -> Result<()> {
        let node = self.0;
        let cluster = u64::from(node.fs.geo.cluster_size);
        let needed = u32::try_from(size.div_ceil(cluster)).map_err(|_| Error::TooBig)?;
        let mut meta = node.meta.lock();
        let Meta { chain, first, .. } = &mut *meta;
        let chain = match chain {
            Some(chain) => chain,
            None => chain.insert(node.fs.load_chain(*first)?),
        };
        let had = chain.len();
        while chain.len() < needed {
            let grown = match chain.reserve() {
                Ok(()) => node.fs.extend(chain.last()),
                Err(err) => Err(err.into()),
            };
            match grown {
                Ok(cluster) => {
                    // Room was made for it.
                    let _ = chain.push(cluster);
                    if *first == 0 {
                        *first = cluster;
                    }
                }
                Err(err) => {
                    // Back to what it was: no room is left reserved.
                    let _ = node.fs.cut(chain, first, had, false);
                    return Err(err);
                }
            }
        }
        if chain.len() != had {
            meta.dirty = true;
        }
        Ok(())
    }

    fn zero(&self, from: u64, to: u64) -> Result<()> {
        let zeros = CachePage::new()?;
        let end = to.min(self.0.allocated());
        let device = self.0.fs.cache.device();
        let mut at = from;
        while at < end {
            let n = (end - at).min(PAGE_SIZE as u64) as usize;
            for (sector, offset, len) in self.0.pieces(at, n)?.iter() {
                device.write(sector, &[Segment { page: &zeros, offset, len }])?;
            }
            at += n as u64;
        }
        Ok(())
    }

    fn truncate(&self, size: u64) -> Result<()> {
        let node = self.0;
        let cluster = u64::from(node.fs.geo.cluster_size);
        let keep = u32::try_from(size.div_ceil(cluster)).map_err(|_| Error::TooBig)?;
        let mut meta = node.meta.lock();
        node.chain(&mut meta)?;
        let Meta { chain, first, dirty, .. } = &mut *meta;
        let Some(chain) = chain else { return Ok(()) };
        if keep < chain.len() {
            node.fs.cut(chain, first, keep, false)?;
            *dirty = true;
        }
        Ok(())
    }

    fn max_size(&self) -> u64 {
        MAX_SIZE
    }
}

impl Drop for FatNode {
    fn drop(&mut self) {
        // A removed node's clusters were kept for it; they go with it.
        let meta = self.meta.get_mut();
        if !self.root && meta.loc.is_none() && !self.fs.read_only {
            let _ = self.fs.free_chain(meta.first, self.kind == Kind::Directory);
        }
    }
}

/// The error for a name FAT cannot hold.
fn name_error(err: name::NameError) -> Error {
    match err {
        name::NameError::TooLong => Error::NameTooLong,
        name::NameError::Invalid => Error::BadName,
    }
}

/// Whether `moving` may replace `old`: a file a file, a directory an
/// empty directory.
fn check_replace(names: &mut Names, moving: Kind, old: &FatNode) -> Result<()> {
    match (moving, old.kind) {
        (_, Kind::Directory) if !old.is_empty_dir(names)? => Err(Error::NotEmpty),
        (Kind::Directory, Kind::File) => Err(Error::NotDirectory),
        (Kind::File, Kind::Directory) => Err(Error::IsDirectory),
        _ => Ok(()),
    }
}

/// A name ready to be written into a directory.
struct Prepared {
    /// Its UTF-16 units.
    units: [u16; name::LONG_MAX],
    len: usize,
    /// The short entry's name and case bits.
    short: [u8; 11],
    case: u8,
    /// Whether it needs long entries.
    long: bool,
}

impl FatNode {
    /// Checks `name` for this directory and picks its short name. `skip`
    /// are entries to ignore (a rename's own).
    fn prepare(&self, names: &mut Names, name: &str, skip: &[u32]) -> Result<Prepared> {
        let mut units = [0u16; name::LONG_MAX];
        let len = name::units(name, &mut units).map_err(name_error)?;
        let (short, case, long) = match short_form(name) {
            ShortForm::Exact(short, case) => {
                if self.survey(names, name, &[], skip)?.0 {
                    return Err(Error::Exists);
                }
                (short, case, false)
            }
            ShortForm::Cased(short) => {
                (self.unused_short(names, name, short, true, skip)?, 0, true)
            }
            ShortForm::Lossy(basis) => {
                (self.unused_short(names, name, basis, false, skip)?, 0, true)
            }
        };
        Ok(Prepared { units, len, short, case, long })
    }

    /// A short name no other entry has: `basis` itself if `as_is` and it
    /// is free, else `basis` with the first free tail `~1` to `~32`, else
    /// a basis made from a hash of the name with a tail (as Windows does).
    fn unused_short(
        &self,
        names: &mut Names,
        name: &str,
        basis: [u8; 11],
        as_is: bool,
        skip: &[u32],
    ) -> Result<[u8; 11]> {
        let mut candidates = [[0u8; 11]; 33];
        for round in 0..2 {
            let basis = if round == 0 { basis } else { hashed(&basis, name) };
            let mut count = 0;
            if as_is && round == 0 {
                candidates[0] = basis;
                count = 1;
            }
            for n in 1..=32 {
                candidates[count] = name::with_tail(&basis, n);
                count += 1;
            }
            let (exists, used) = self.survey(names, name, &candidates[..count], skip)?;
            if exists {
                return Err(Error::Exists);
            }
            if let Some(i) = (0..count).find(|&i| used & (1 << i) == 0) {
                return Ok(candidates[i]);
            }
        }
        Err(Error::NoSpace)
    }

    /// Makes a directory's first cluster, with `.` and `..`, for the new
    /// entry `entry` in this directory.
    fn new_dir_cluster(&self, entry: &Short) -> Result<u32> {
        let cluster = self.fs.extend(None)?;
        let parent = if self.root { 0 } else { self.dir_key() };
        let made = self.fs.zero_cluster(cluster).and_then(|()| {
            let offset = self.fs.geo.cluster_offset(cluster);
            let dot = Short { name: *b".          ", cluster, ..*entry };
            let dotdot = Short { name: *b"..         ", cluster: parent, ..*entry };
            self.fs.cache.write(offset, &dot.encode())?;
            Ok(self.fs.cache.write(offset + name::ENTRY as u64, &dotdot.encode())?)
        });
        if let Err(err) = made {
            let _ = self.fs.free_chain(cluster, true);
            return Err(err);
        }
        Ok(cluster)
    }

    /// Writes the entries for `name`, with short entry `short` (whose
    /// name and case it sets), into this directory; returns where the
    /// short entry went, and the entry.
    fn put_named(&self, name: &Prepared, mut short: Short) -> Result<(u32, u64, Short)> {
        short.name = name.short;
        short.case = name.case;
        let mut entries = [[0u8; name::ENTRY]; MAX_LONG + 1];
        let mut count = 0;
        if name.long {
            long_entries(&name.units[..name.len], checksum(&name.short), |entry| {
                entries[count] = entry;
                count += 1;
            });
        }
        entries[count] = short.encode();
        count += 1;
        let (index, offset) = self.put(&entries[..count])?;
        Ok((index, offset, short))
    }

    /// Whether `dir` is this directory or lies under it, following `..`
    /// up from it. This directory is never the root, which has no entry
    /// to move.
    fn holds(&self, dir: &FatNode) -> Result<bool> {
        if dir.root {
            return Ok(false);
        }
        let me = self.dir_key();
        let geo = &self.fs.geo;
        let mut cluster = dir.dir_key();
        for _ in 0..geo.clusters {
            if cluster == me {
                return Ok(true);
            }
            cluster = self.fs.parent_of(cluster)?;
            if cluster == 0 || (geo.width == Width::Fat32 && cluster == geo.root_cluster) {
                return Ok(false);
            }
        }
        Err(Error::Corrupt)
    }
}

/// `basis` cut to two characters and four hex digits of a hash of the
/// name, for when its tails `~1` to `~32` are all taken.
fn hashed(basis: &[u8; 11], name: &str) -> [u8; 11] {
    let hash =
        name.bytes().fold(0x811c_9dc5u32, |h, b| (h ^ u32::from(b)).wrapping_mul(0x0100_0193));
    let mut out = *basis;
    let keep = basis[..2].iter().take_while(|&&b| b != b' ').count();
    for (i, slot) in out[keep..6].iter_mut().enumerate() {
        let shift = 12u32.saturating_sub(4 * i as u32);
        *slot = b"0123456789ABCDEF"[((hash >> shift) & 0xf) as usize];
    }
    out[6..8].fill(b' ');
    out
}

impl Node for FatNode {
    fn stat(&self) -> Stat {
        let (id, created, dir_modified) = {
            let meta = self.meta.lock();
            (meta.id, meta.created, meta.modified)
        };
        let cluster = u64::from(self.fs.geo.cluster_size);
        let (size, allocated, modified) = match &self.cache {
            Some(cache) => {
                let (size, modified, _) = cache.status();
                (size, size.next_multiple_of(cluster), modified)
            }
            None => (0, self.allocated(), dir_modified),
        };
        Stat { kind: self.kind, size, allocated, id, created, modified }
    }

    fn kind(&self) -> Kind {
        self.kind
    }

    fn lookup(&self, name: &str) -> Result<NodeRef> {
        self.is_dir()?;
        let mut ns = self.fs.ns.lock();
        let found = self.find(&mut ns, name, &[])?.ok_or(Error::NotFound)?;
        Ok(Inner::node_for(&self.fs, self.dir_key(), found.index, found.offset, &found.short)?)
    }

    fn create(&self, name: &str, kind: Kind) -> Result<NodeRef> {
        self.writable()?;
        check_name(name)?;
        self.is_dir()?;
        if !matches!(kind, Kind::File | Kind::Directory) {
            return Err(Error::NotSupported);
        }
        let mut ns = self.fs.ns.lock();
        if self.removed() {
            return Err(Error::NotFound);
        }
        let time = now();
        let (date, clock) = to_fat(time);
        let mut short = Short {
            name: [b' '; 11],
            attr: if kind == Kind::Directory { attr::DIRECTORY } else { attr::ARCHIVE },
            case: 0,
            created_tenths: ((time / 1_000_000_000 % 2) * 100 + time % 1_000_000_000 / 10_000_000)
                as u8,
            created_time: clock,
            created_date: date,
            accessed_date: date,
            modified_time: clock,
            modified_date: date,
            cluster: 0,
            size: 0,
        };
        // A name that cannot be stored is refused before a directory's
        // cluster is taken for it.
        let prepared = self.prepare(&mut ns, name, &[])?;
        if kind == Kind::Directory {
            short.cluster = self.new_dir_cluster(&short)?;
        }
        let (index, offset, short) = match self.put_named(&prepared, short) {
            Ok(put) => put,
            Err(err) => {
                let _ = self.fs.free_chain(short.cluster, true);
                return Err(err);
            }
        };
        self.touch();
        Ok(Inner::node_for(&self.fs, self.dir_key(), index, offset, &short)?)
    }

    fn remove(&self, name: &str) -> Result<()> {
        self.writable()?;
        check_name(name)?;
        self.is_dir()?;
        let mut ns = self.fs.ns.lock();
        let found = self.find(&mut ns, name, &[])?.ok_or(Error::NotFound)?;
        let dir = self.dir_key();
        let node = Inner::node_for(&self.fs, dir, found.index, found.offset, &found.short)?;
        if node.kind == Kind::Directory && !node.is_empty_dir(&mut ns)? {
            return Err(Error::NotEmpty);
        }
        self.delete(&found)?;
        self.fs.take_node(key(dir, found.index));
        // Its clusters go when the last reference to it does: now, unless
        // a handle has it.
        node.meta.lock().loc = None;
        drop(node);
        self.touch();
        Ok(())
    }

    fn rename(&self, from: &str, to_dir: &dyn Node, to: &str) -> Result<()> {
        self.writable()?;
        check_name(from)?;
        check_name(to)?;
        self.is_dir()?;
        let target = radix_vfs::downcast::<FatNode>(to_dir).ok_or(Error::CrossMount)?;
        if !KArc::ptr_eq(&target.fs, &self.fs) {
            return Err(Error::CrossMount);
        }
        target.is_dir()?;
        let mut ns = self.fs.ns.lock();
        if self.removed() || target.removed() {
            return Err(Error::NotFound);
        }
        let same_dir = core::ptr::eq(self, target);
        let source = self.find(&mut ns, from, &[])?.ok_or(Error::NotFound)?;
        if same_dir && from == to {
            return Ok(());
        }
        let (dir, to_key) = (self.dir_key(), target.dir_key());
        let moving = Inner::node_for(&self.fs, dir, source.index, source.offset, &source.short)?;
        if moving.kind == Kind::Directory && !same_dir && moving.holds(target)? {
            return Err(Error::BadName);
        }
        // What it replaces, unless that is itself under another spelling.
        let replaced = match target.find(&mut ns, to, &[])? {
            Some(old) if same_dir && old.index == source.index => None,
            Some(old) => {
                let node = Inner::node_for(&self.fs, to_key, old.index, old.offset, &old.short)?;
                check_replace(&mut ns, moving.kind, &node)?;
                Some((old, node))
            }
            None => None,
        };
        // The entry is copied as it is now, with what is in memory.
        moving.store()?;
        let mut bytes = [0u8; name::ENTRY];
        self.fs.cache.read(source.offset, &mut bytes)?;
        let short = Short::decode(&bytes);
        let mut skip = [u32::MAX; 2];
        if same_dir {
            skip[0] = source.index;
        }
        if let Some((old, _)) = &replaced {
            skip[1] = old.index;
        }
        // The new entry is written before the old ones go, so that a
        // failure leaves the file where it was.
        let prepared = target.prepare(&mut ns, to, &skip)?;
        let (index, offset, _) = target.put_named(&prepared, short)?;
        if let Some((old, node)) = replaced {
            target.delete(&old)?;
            self.fs.take_node(key(to_key, old.index));
            node.meta.lock().loc = None;
            drop(node);
        }
        self.delete(&source)?;
        if moving.kind == Kind::Directory && !same_dir {
            // Its `..` names its new parent (0 for the root).
            let parent = if target.root { 0 } else { to_key };
            let first = moving.dir_key();
            let at = self.fs.geo.cluster_offset(first) + name::ENTRY as u64;
            self.fs.cache.read(at, &mut bytes)?;
            let mut dotdot = Short::decode(&bytes);
            dotdot.cluster = parent;
            self.fs.cache.write(at, &dotdot.encode())?;
        }
        {
            let mut meta = moving.meta.lock();
            meta.loc = Some(offset);
            meta.id = id_of(to_key, index);
        }
        self.fs.rekey(key(dir, source.index), key(to_key, index), &moving)?;
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
        self.scan(&mut ns, from, |visit| {
            let kind = if visit.short.is_dir() { Kind::Directory } else { Kind::File };
            f(u64::from(visit.index) + 1, visit.name, kind)
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

    fn sync(&self) -> Result<()> {
        // The whole volume: the tables a file's clusters are in are
        // shared, and the volume is then consistent on the disk.
        self.fs.sync_all()
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}
