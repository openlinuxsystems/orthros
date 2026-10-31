// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! NTFS files, directories and links as VFS nodes.
//!
//! A node is made from its file's records when it is first looked up,
//! and kept while in use, one for each file however many names it has. A
//! file's data goes through its page cache, whose backing moves pages
//! straight between the cache and the clusters on the disk, a run of them
//! in one request where they lie in a row; holes, and what lies past what
//! was written of it, read as zeros. A compressed file's unit is read
//! whole and uncompressed ([`crate::lznt1`]), and its other pages go to
//! the cache with the one asked for.
//!
//! Some files' data Radix does not have: an encrypted (EFS) file's reads
//! fail with [`Error::Denied`], as on Linux, and those of a file whose
//! data is elsewhere (in the cloud, as OneDrive leaves a file not yet
//! downloaded, or in deduplication's chunk store) with
//! [`Error::NotSupported`], rather than read as zeros.
//!
//! # Writing
//!
//! A file's size, times and where its clusters are live in memory while
//! the node is, and reach its record, and the copies of them in its
//! directories' entries, when the node is flushed; a node with changes is
//! kept in memory until then (the volume pins it). NTFS keeps two sizes
//! for a file, as exFAT does: what lies past what was written reads as
//! zeros, so growing a file writes no zeros until data goes past them.
//! A small file's data lives in its record (resident): Radix writes it
//! there while it keeps its size, and moves it to clusters of its own when
//! it grows, or when the record needs the room for a name.
//!
//! A directory's changes go to its index ([`crate::tree`]) and record at
//! once, in the cache. A file removed while in use keeps its record and
//! clusters until its last reference goes, as on Linux; its names are
//! gone at once.
//!
//! Radix does not write compressed, sparse or encrypted data yet, nor a
//! file whose attributes are in more than one record, nor make symbolic
//! links: those fail with [`Error::NotSupported`] (ADR 0237).

use alloc::sync::Weak;
use core::any::Any;
use core::cmp::Ordering as Order;
use core::sync::atomic::{AtomicBool, Ordering};

use radix_block::mem::CachePage;
use radix_block::{PAGE_SIZE, Page, SECTOR, Segment, SegmentMut};
use radix_sync::{Mutex, Once};
use radix_vfs::cache::{Backing, FileCache, READ_RUN, page_len};
use radix_vfs::mem::{self, Alloc, KArc, KVec};
use radix_vfs::path::check_name;
use radix_vfs::{
    Access, AccessChange, Error, In, Kind, ListFn, Node, NodeRef, Out, Result, Stat, now,
};

use crate::edit::{self, I30, put_u16, put_u32};
use crate::fs::{
    Info, Inner, Loaded, Scratch, Stream, Value, records, replace_named, replace_stream, zeroed,
};
use crate::index::{self, FileName, NAME_MAX, ROOT_HEADER, attributes, namespace};
use crate::lznt1;
use crate::names::{self, Dup};
use crate::record::{self, Record, attr_flags, fix_up, kind, record_of, u16_at, u32_at, u64_at};
use crate::reparse::{self, Target};
use crate::runs::{HOLE, Runs};
use crate::space::write_runs;
use crate::tree::{self, Store};

const PAGE: u64 = PAGE_SIZE as u64;

/// The largest reparse point: Windows' own limit, 16 KiB.
const MAX_REPARSE: u64 = 16 << 10;

/// The largest Interix link Radix reads.
const MAX_INTERIX: u64 = 4096;

/// The largest compression unit Radix reads: 16 clusters of 4 KiB, all
/// Windows makes.
const MAX_UNIT: u64 = 64 << 10;

/// The largest cluster of a compressed file Radix writes: Windows
/// compresses only on volumes of clusters of 4 KiB or less.
const MAX_COMPRESS_CLUSTER: u64 = 4096;

/// The compression unit of the files Radix compresses, as Windows'
/// are: 16 clusters.
const UNIT: u8 = 4;

/// The deepest a link's directory may be below the root.
const MAX_DEPTH: usize = 256;

/// The most bitmap of a directory's index Radix reads: a bit for each
/// block, so room for 2^27 blocks.
const MAX_WHOLE_BITMAP: u64 = 16 << 20;

/// The most names a file may have, as Windows allows.
const MAX_LINKS: u32 = 1024;

/// The most listings of one directory that go on where they stopped at
/// once ([`Mark`]).
const MARKS: usize = 16;

/// A file's data.
enum Data {
    /// In its record.
    Resident(KVec<u8>),
    /// In clusters.
    Clusters(Stream),
    /// Not here, or not readable by Radix: why its reads fail.
    Unreadable(Error),
}

/// Where a listing of a directory stopped: the name of the last entry
/// it gave. The index is in name order, so a listing goes on after that
/// name, wherever the entries around it moved meanwhile, and gives each
/// name there all along once, as POSIX asks, though names before it went
/// (as `rm -r` takes them). A listing's cursor is the mark's number and
/// how many entries the listing gave since the name it started after.
struct Mark {
    id: u32,
    /// The name the listing started after; none for the start.
    start: Option<KVec<u16>>,
    /// How many entries were given before it started.
    base: u32,
    /// The last entry it gave, and how many it had given then.
    last: Option<KVec<u16>>,
    count: u32,
}

/// A directory's index.
struct Dir {
    /// `$I30`'s root.
    root: KVec<u8>,
    /// Its blocks, if it has any.
    blocks: Option<Stream>,
    /// Which blocks are in use.
    bitmap: KVec<u8>,
    /// Whether the bitmap is in clusters, which Radix does not change.
    fixed: bool,
    /// Where listings stopped.
    marks: KVec<Mark>,
    next_mark: u32,
}

/// What a node knows of itself that changes.
struct State {
    sequence: u16,
    info: Info,
    links: u32,
    /// The directory of its first name.
    parent: u64,
    /// The bytes its clusters take, as its record said.
    allocated: u64,
    data: Option<Data>,
    dir: Option<Dir>,
    /// Why its data cannot be written, if it cannot.
    fixed: Option<Error>,
    /// Whether its attributes are in more than one record.
    listed: bool,
    /// Whether its record on the disk is behind.
    dirty: bool,
    /// How many times it has fallen behind, so that a change made while
    /// it is stored is stored again.
    changes: u64,
    /// Whether its last name is gone: its record and clusters go with it.
    removed: bool,
}

impl State {
    fn behind(&mut self) {
        self.dirty = true;
        self.changes = self.changes.wrapping_add(1);
    }
}

/// An NTFS file, directory or link.
pub struct NtfsNode {
    pub(crate) fs: KArc<Inner>,
    /// Its base record's number.
    record: u64,
    pub(crate) kind: Kind,
    state: Mutex<State>,
    cache: Option<FileCache>,
    /// A link's target, as the disk has it, and as a path once worked out.
    link: Option<Target>,
    path: Once<KVec<u8>>,
    me: Once<Weak<NtfsNode, Alloc>>,
    pinned: AtomicBool,
}

/// The time now, as finely as NTFS keeps it.
fn now_coarse() -> u64 {
    names::coarse(now())
}

impl NtfsNode {
    /// The node of the file whose base record is `record`, from what was
    /// read of it.
    #[inline(never)]
    pub(crate) fn new(fs: KArc<Inner>, record: u64, loaded: Loaded) -> Result<KArc<NtfsNode>> {
        let Loaded {
            sequence,
            flags,
            info,
            links,
            parent,
            data,
            root,
            blocks,
            bitmap,
            reparse,
            listed,
            ..
        } = loaded;
        let vol = &fs.vol;
        let cs = u64::from(vol.geo.cluster_size);
        let reparse = match &reparse {
            Some(value) => Some(vol.read_whole(value, MAX_REPARSE)?),
            None => None,
        };
        let tag = reparse.as_deref().filter(|r| r.len() >= 4).map_or(0, |r| u32_at(r, 0));
        let mut link = None;
        let mut kind = if let Some(value) = reparse.as_deref()
            && reparse::is_link(tag)
        {
            link = Some(reparse::target(value)?);
            Kind::Symlink
        } else if flags & record::flags::DIRECTORY != 0 {
            Kind::Directory
        } else {
            Kind::File
        };
        if kind == Kind::File
            && info.attributes & attributes::SYSTEM != 0
            && let Some(value) = &data
            && (reparse::INTERIX_LINK.len() as u64 + 2..=MAX_INTERIX).contains(&value.size())
            && value.flags() & (attr_flags::COMPRESSED | attr_flags::ENCRYPTED) == 0
        {
            let mut start = [0u8; 8];
            vol.read_value(value, 0, &mut start)?;
            if &start == reparse::INTERIX_LINK {
                link = Some(reparse::interix(&vol.read_whole(value, MAX_INTERIX)?)?);
                kind = Kind::Symlink;
            }
        }
        let mut size = 0;
        let mut allocated = 0;
        let mut file_data = None;
        let mut dir = None;
        let mut fixed = listed.then_some(Error::NotSupported);
        match kind {
            Kind::File => {
                size = data.as_ref().map_or(0, Value::size);
                let unreadable = if info.attributes & attributes::ENCRYPTED != 0 {
                    Some(Error::Denied)
                } else if info.attributes
                    & (attributes::OFFLINE
                        | attributes::RECALL_ON_OPEN
                        | attributes::RECALL_ON_DATA_ACCESS)
                    != 0
                    || tag == reparse::tag::DEDUP
                {
                    Some(Error::NotSupported)
                } else {
                    None
                };
                let data = match data {
                    None => Data::Resident(mem::vec()),
                    Some(Value::Resident(bytes)) => Data::Resident(bytes),
                    Some(Value::Clusters(stream)) => {
                        if stream.runs.clusters() * cs < stream.size {
                            return Err(Error::Corrupt);
                        }
                        let compressed = stream.flags & attr_flags::COMPRESSED != 0;
                        if compressed && stream.unit == 0 {
                            return Err(Error::Corrupt);
                        }
                        allocated = stream.runs.allocated_in(0, stream.runs.clusters()) * cs;
                        // Compressed and sparse data are written a unit
                        // at a time; encrypted data is not.
                        let known = attr_flags::COMPRESSED | attr_flags::SPARSE;
                        if stream.flags & !known != 0 || (compressed && cs > MAX_COMPRESS_CLUSTER) {
                            fixed = Some(Error::NotSupported);
                        }
                        if stream.flags & attr_flags::ENCRYPTED != 0 {
                            Data::Unreadable(Error::Denied)
                        } else if compressed
                            && cs.checked_shl(stream.unit.into()).is_none_or(|u| u > MAX_UNIT)
                        {
                            Data::Unreadable(Error::NotSupported)
                        } else {
                            Data::Clusters(stream)
                        }
                    }
                };
                let data = match unreadable {
                    Some(err) => Data::Unreadable(err),
                    None => data,
                };
                if let Data::Unreadable(err) = data {
                    fixed = Some(err);
                }
                file_data = Some(data);
            }
            Kind::Directory => {
                let root = root.ok_or(Error::Corrupt)?;
                // A directory's index is of names, collated as names are.
                if root.len() < ROOT_HEADER + 16 || u32_at(&root, 0) != record::kind::FILE_NAME {
                    return Err(Error::Corrupt);
                }
                let blocks = match blocks {
                    Some(Value::Clusters(stream)) => Some(stream),
                    Some(Value::Resident(_)) => return Err(Error::Corrupt),
                    None => None,
                };
                let (bitmap, in_clusters) = match &bitmap {
                    Some(value @ Value::Clusters(_)) => {
                        (vol.read_whole(value, MAX_WHOLE_BITMAP)?, true)
                    }
                    Some(Value::Resident(bytes)) => (mem::copy(bytes)?, false),
                    None => (mem::vec(), false),
                };
                if blocks.is_some() && bitmap.is_empty() {
                    return Err(Error::Corrupt);
                }
                size = blocks.as_ref().map_or(root.len() as u64, |b| b.size);
                allocated = blocks.as_ref().map_or(0, |b| b.size);
                dir = Some(Dir {
                    root,
                    blocks,
                    bitmap,
                    fixed: in_clusters,
                    marks: mem::vec(),
                    next_mark: 1,
                });
            }
            _ => {}
        }
        if kind == Kind::Symlink {
            fixed = Some(Error::NotSupported);
        }
        let cache = file_data.as_ref().map(|_| FileCache::new(size, info.modified));
        let state = State {
            sequence,
            info,
            links,
            parent,
            allocated,
            data: file_data,
            dir,
            fixed,
            listed,
            dirty: false,
            changes: 0,
            removed: false,
        };
        let node = mem::arc(NtfsNode {
            fs,
            record,
            kind,
            state: Mutex::new(state),
            cache,
            link,
            path: Once::new(),
            me: Once::new(),
            pinned: AtomicBool::new(false),
        })?;
        let _ = node.me.set(KArc::downgrade(&node));
        Ok(node)
    }

    /// Its file reference: its record and the record's sequence number.
    fn reference(&self) -> u64 {
        self.record | (u64::from(self.state.lock().sequence) << 48)
    }

    fn writable(&self) -> Result<()> {
        if self.fs.read_only { Err(Error::ReadOnly) } else { Ok(()) }
    }

    fn is_dir(&self) -> Result<()> {
        if self.kind == Kind::Directory { Ok(()) } else { Err(Error::NotDirectory) }
    }

    fn file(&self) -> Result<&FileCache> {
        self.cache.as_ref().ok_or(Error::IsDirectory)
    }

    /// Whether its last name is gone.
    fn removed(&self) -> bool {
        self.state.lock().removed
    }

    /// Fails as its data's writes do, if they do.
    fn data_writable(&self) -> Result<()> {
        match self.state.lock().fixed {
            Some(err) => Err(err),
            None => Ok(()),
        }
    }

    /// Fails with [`Error::NotSupported`] if its record cannot change:
    /// its attributes are in more than one.
    fn editable(&self) -> Result<()> {
        let st = self.state.lock();
        if st.listed || st.dir.as_ref().is_some_and(|d| d.fixed) {
            return Err(Error::NotSupported);
        }
        Ok(())
    }

    /// Drops up to `max` pages of its data, which can be read again.
    pub(crate) fn shrink(&self, max: usize) -> usize {
        self.cache.as_ref().map_or(0, |cache| cache.shrink(max, &FileBacking(self)))
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

    /// Notes that its record is behind, and keeps it in memory until the
    /// volume syncs. The caller holds no lock.
    fn changed(&self) {
        self.state.lock().behind();
        if !self.pin() {
            // No memory to remember it: it is written now instead.
            let _ = self.flush();
        }
    }

    /// Marks this directory compressed, as Windows' Properties do, so
    /// that what is made in it is compressed.
    #[cfg(test)]
    pub(crate) fn mark_compressed(&self) -> Result<()> {
        self.writable()?;
        self.fs.changing()?;
        self.state.lock().info.attributes |= attributes::COMPRESSED;
        self.changed();
        Ok(())
    }

    /// Notes that its data changed: Windows' archive bit goes on.
    fn data_changed(&self) {
        {
            let mut st = self.state.lock();
            st.info.attributes |= attributes::ARCHIVE;
            st.info.changed = now_coarse();
        }
        self.changed();
    }

    /// Marks a directory changed now. The caller holds `ns`; without the
    /// memory to keep it until the volume syncs, the time is not kept,
    /// which loses nothing else.
    fn touch(&self) {
        {
            let mut st = self.state.lock();
            let t = now_coarse();
            st.info.modified = t;
            st.info.changed = t;
            st.behind();
        }
        let _ = self.pin();
    }

    /// Writes its data, then its record.
    pub(crate) fn flush(&self) -> Result<()> {
        if let Some(cache) = &self.cache
            && !self.removed()
        {
            cache.sync(&FileBacking(self))?;
        }
        let _ns = self.fs.ns.lock();
        self.store()
    }

    /// Its record, read.
    fn read_rec(&self) -> Result<KVec<u8>> {
        let mut buf = zeroed(self.fs.vol.geo.record_size as usize)?;
        self.fs.vol.read_record(self.record, &mut buf)?;
        Ok(buf)
    }

    /// Writes `buf` as its record, in the cache.
    fn write_rec(&self, buf: &[u8]) -> Result<()> {
        self.fs.vol.write_record(self.record, buf, false)
    }

    /// What its names say of it, its data `size` bytes.
    fn dup(&self, st: &State, size: u64) -> Dup {
        let cs = u64::from(self.fs.vol.geo.cluster_size);
        let (allocated, size) = match (&st.data, &st.fixed) {
            // Compressed and sparse data: what its clusters on the disk
            // hold, as ntfs-3g puts it.
            (Some(Data::Clusters(s)), None) if s.packed() => {
                (s.runs.allocated_in(0, s.runs.clusters()) * cs, size)
            }
            (Some(Data::Clusters(s)), None) => (s.runs.clusters() * cs, size),
            (Some(Data::Resident(b)), None) => ((b.len() as u64).next_multiple_of(8), size),
            (Some(_), _) => (st.allocated, size),
            (None, _) => (0, 0),
        };
        let dir = if self.kind == Kind::Directory { attributes::DIRECTORY } else { 0 };
        Dup {
            created: names::to_nt(st.info.created),
            modified: names::to_nt(st.info.modified),
            changed: names::to_nt(st.info.changed),
            accessed: names::to_nt(st.info.accessed),
            allocated,
            size,
            flags: st.info.attributes | dir,
        }
    }

    /// Puts its data's attribute, as memory has it, in its record `buf`,
    /// for data of `size` bytes.
    fn put_data(&self, st: &State, buf: &mut [u8], size: u64) -> Result<()> {
        let cs = u64::from(self.fs.vol.geo.cluster_size);
        match &st.data {
            Some(Data::Resident(bytes)) => {
                let at = edit::find(buf, kind::DATA, &[]).ok_or(Error::Corrupt)?;
                if buf[at + 8] != 0 {
                    return Err(Error::Corrupt);
                }
                let n = bytes.len().min(size as usize);
                edit::set_value(buf, at, &bytes[..n])
            }
            Some(Data::Clusters(stream)) => {
                let clusters = stream.runs.clusters();
                let sizes = edit::Sizes {
                    allocated: clusters * cs,
                    size,
                    initialized: stream.initialized.min(size),
                    compressed: stream.runs.allocated_in(0, clusters) * cs,
                    unit: stream.unit,
                    packed: stream.flags,
                };
                replace_stream(buf, kind::DATA, &stream.runs, clusters, sizes)
            }
            _ => Ok(()),
        }
    }

    /// Writes its size, times, attributes and where its clusters are to
    /// its record, and what its names say of it to their entries in its
    /// directories. The caller holds `ns`.
    pub(crate) fn store(&self) -> Result<()> {
        // What the cache says is read without the node's lock, which ranks
        // below the cache's: a change made meanwhile is stored next time.
        let seen = self.state.lock().changes;
        let (size, modified) = match &self.cache {
            Some(cache) => (cache.size(), Some(cache.modified())),
            None => (0, None),
        };
        let mut st = self.state.lock();
        if !st.dirty {
            return Ok(());
        }
        if st.removed || self.fs.read_only || st.listed {
            st.dirty = false;
            return Ok(());
        }
        if let Some(modified) = modified {
            st.info.modified = names::coarse(modified);
        }
        if st.info.changed < st.info.modified {
            st.info.changed = st.info.modified;
        }
        let dup = self.dup(&st, size);
        let mut buf = self.read_rec()?;
        let si = edit::find(&buf, kind::STANDARD_INFORMATION, &[]).ok_or(Error::Corrupt)?;
        names::put_standard(edit::value_mut(&mut buf, si), &dup, st.info.attributes);
        if self.kind == Kind::File && st.fixed.is_none() {
            self.put_data(&st, &mut buf, size)?;
        }
        let mut names = mem::vec();
        let at: KVec<usize> = collect(edit::find_all(&buf, kind::FILE_NAME))?;
        for &at in &at {
            let value = edit::value_mut(&mut buf, at);
            if value.len() < 0x42 {
                return Err(Error::Corrupt);
            }
            dup.put(value);
            let name = FileName::new(&*value)?;
            mem::push(&mut names, (name.parent(), collect(name.units())?))?;
        }
        self.fs.changing()?;
        self.write_rec(&buf)?;
        st.dirty = st.changes != seen;
        let reference = self.record | (u64::from(st.sequence) << 48);
        drop(st);
        // What a listing in Windows shows of the file comes from its
        // entries: they follow, though one that cannot changes nothing
        // else.
        for (parent, units) in &names {
            if let Ok(dir) = Inner::node_for(&self.fs, *parent) {
                let _ = dir.update_entry(units, reference, &dup);
            }
        }
        Ok(())
    }

    /// Moves a file's resident data to `size` bytes' worth of clusters of
    /// its own: its bytes are written there now, and its record says so
    /// when it is next stored.
    /// Whether its record has room for its data in it, `size` bytes.
    fn resident_fits(&self, size: usize) -> Result<bool> {
        let buf = self.read_rec()?;
        let at = edit::find(&buf, kind::DATA, &[]).ok_or(Error::Corrupt)?;
        let old = u32_at(&buf, at + 4) as usize;
        let len = (0x18 + size).next_multiple_of(8);
        Ok(edit::used(&buf) - old + len <= buf.len())
    }

    fn to_clusters(&self, st: &mut State, size: u64) -> Result<()> {
        let geo = &self.fs.vol.geo;
        let cs = u64::from(geo.cluster_size);
        let Some(Data::Resident(bytes)) = &st.data else { return Err(Error::Corrupt) };
        if st.info.attributes & attributes::COMPRESSED != 0 && cs <= MAX_COMPRESS_CLUSTER {
            return self.to_units(st, size);
        }
        let need = size.max(bytes.len() as u64).div_ceil(cs);
        let got = self.fs.take_clusters(need, None)?;
        let mut runs = Runs::new();
        let give_back = |got: &[(u64, u64)]| {
            for &(lcn, n) in got {
                let _ = self.fs.give_clusters(lcn, n, false);
            }
        };
        let made = (|| {
            for &(lcn, n) in &got {
                runs.append(lcn, n)?;
            }
            let stream = Stream { runs, size: bytes.len() as u64, ..Stream::new() };
            write_bytes(&self.fs, &stream, bytes)?;
            Ok(stream)
        })();
        match made {
            Ok(mut stream) => {
                stream.initialized = stream.size;
                st.data = Some(Data::Clusters(stream));
                st.behind();
                Ok(())
            }
            Err(err) => {
                give_back(&got);
                Err(err)
            }
        }
    }

    /// Fails with [`Error::NoSpace`] if its record has no room for where
    /// the clusters of its data `stream` are.
    fn runs_fit(&self, stream: &Stream) -> Result<()> {
        let buf = self.read_rec()?;
        let at = edit::find(&buf, kind::DATA, &[]).ok_or(Error::Corrupt)?;
        let packed = stream.runs.pack(0, stream.runs.clusters())?;
        let header = if stream.packed() { 0x48 } else { 0x40 };
        let len = (header + packed.len()).next_multiple_of(8);
        let old = u32_at(&buf, at + 4) as usize;
        if edit::used(&buf) - old + len > buf.len() { Err(Error::NoSpace) } else { Ok(()) }
    }

    /// Gives the holes of the sparse `stream` that bytes `from..to` fall
    /// in clusters, of zeros, a compression unit's at a time as Windows
    /// fills them.
    fn fill_holes(&self, stream: &mut Stream, from: u64, to: u64) -> Result<()> {
        let cs = u64::from(self.fs.vol.geo.cluster_size);
        let grain = stream.grain();
        let end = to.div_ceil(cs).next_multiple_of(grain).min(stream.runs.clusters());
        let mut vcn = from / cs / grain * grain;
        while vcn < end {
            let (lcn, n) = stream.runs.map(vcn).ok_or(Error::Corrupt)?;
            let n = n.min(end - vcn);
            if lcn != HOLE {
                vcn += n;
                continue;
            }
            let near = vcn.checked_sub(1).and_then(|v| stream.runs.map(v));
            let near = near.and_then(|(l, _)| (l != HOLE).then(|| l + 1));
            let got = self.fs.take_clusters(n, near)?;
            let give_back = |got: &[(u64, u64)]| {
                for &(lcn, n) in got {
                    let _ = self.fs.give_clusters(lcn, n, false);
                }
            };
            let filled = (|| {
                write_clusters(&self.fs, &got, &zeroed((n * cs) as usize)?)?;
                stream.runs.replace(vcn, n, &got)?;
                Ok(())
            })();
            if let Err(err) = filled {
                give_back(&got);
                return Err(err);
            }
            if let Err(err) = self.runs_fit(stream) {
                let _ = stream.runs.replace(vcn, n, &[(HOLE, n)]);
                give_back(&got);
                return Err(err);
            }
            vcn += n;
        }
        Ok(())
    }

    /// [`Self::to_clusters`] for a file marked compressed: its data becomes
    /// compressed, a unit of holes at a time, and its bytes are written to
    /// the first.
    fn to_units(&self, st: &mut State, size: u64) -> Result<()> {
        let cs = u64::from(self.fs.vol.geo.cluster_size);
        let Some(Data::Resident(bytes)) = &st.data else { return Err(Error::Corrupt) };
        let per_unit = 1u64 << UNIT;
        let clusters = size.max(bytes.len() as u64).div_ceil(cs).next_multiple_of(per_unit);
        let mut stream = Stream {
            runs: Runs::new(),
            size: bytes.len() as u64,
            initialized: 0,
            flags: attr_flags::COMPRESSED,
            unit: UNIT,
        };
        stream.runs.append(HOLE, clusters)?;
        if !bytes.is_empty() {
            let mut plain = zeroed((per_unit * cs) as usize)?;
            plain[..bytes.len()].copy_from_slice(bytes);
            let mut packed = zeroed((per_unit * cs) as usize)?;
            let mut matcher = lznt1::Matcher::new()?;
            FileBacking(self).store_unit(&mut stream, 0, &plain, &mut packed, &mut matcher)?;
        }
        stream.initialized = stream.size;
        st.data = Some(Data::Clusters(stream));
        st.behind();
        Ok(())
    }

    /// Makes room for `need` more bytes in its record `buf`, for a name:
    /// a file's resident data moves to clusters. `size` is the file's.
    fn make_room(&self, buf: &mut [u8], need: usize, size: u64) -> Result<()> {
        let mut st = self.state.lock();
        if st.fixed.is_some()
            || !matches!(&st.data, Some(Data::Resident(bytes)) if !bytes.is_empty())
        {
            return Err(Error::NoSpace);
        }
        self.fs.changing()?;
        self.to_clusters(&mut st, size)?;
        self.put_data(&st, buf, size)?;
        if edit::room(buf) < need {
            return Err(Error::NoSpace);
        }
        Ok(())
    }

    /// Frees record `number`, which a new file was to have: no file is in
    /// it, and its sequence number goes up, so that references to it
    /// fail.
    fn free_record(fs: &Inner, number: u64) -> Result<()> {
        let mut buf = zeroed(fs.vol.geo.record_size as usize)?;
        fs.vol.read_record(number, &mut buf)?;
        let flags = u16_at(&buf, 0x16) & !record::flags::IN_USE;
        put_u16(&mut buf, 0x16, flags);
        let sequence = match u16_at(&buf, 0x10).wrapping_add(1) {
            0 => 1,
            n => n,
        };
        put_u16(&mut buf, 0x10, sequence);
        fs.vol.write_record(number, &buf, false)?;
        fs.give_record(number)
    }
}

/// The items of `iter`, in a vector.
fn collect<T>(iter: impl Iterator<Item = T>) -> Result<KVec<T>> {
    let mut v = mem::vec();
    for item in iter {
        mem::push(&mut v, item)?;
    }
    Ok(v)
}

/// Writes `bytes` from the start of `stream`'s clusters, straight to the
/// disk, the last sector padded with zeros.
fn write_bytes(fs: &Inner, stream: &Stream, bytes: &[u8]) -> Result<()> {
    let geo = &fs.vol.geo;
    let device = fs.vol.cache.device();
    let mut page = CachePage::new()?;
    let mut at = 0;
    while at < bytes.len() {
        let n = (bytes.len() - at).min(PAGE_SIZE);
        page.bytes_mut().fill(0);
        page.bytes_mut()[..n].copy_from_slice(&bytes[at..at + n]);
        let whole = n.next_multiple_of(geo.sector_size as usize);
        for (sector, offset, len) in pieces(geo, stream, at as u64, whole)?.iter() {
            device.write(sector, &[Segment { page: &page, offset, len }])?;
        }
        at += n;
    }
    Ok(())
}

/// Writes `bytes` to the clusters `got` (runs of `(lcn, clusters)`), in
/// order, straight to the disk.
fn write_clusters(fs: &Inner, got: &[(u64, u64)], bytes: &[u8]) -> Result<()> {
    let mut runs = Runs::new();
    for &(lcn, n) in got {
        runs.append(lcn, n)?;
    }
    write_bytes(fs, &Stream { runs, ..Stream::new() }, bytes)
}

/// Reads `buf.len()` bytes of a file's data from cluster `lcn` on, through
/// the block cache, which keeps none of it: data is written straight to
/// the disk, past the cache.
fn read_data(vol: &crate::fs::Volume, lcn: u64, buf: &mut [u8]) -> Result<()> {
    let at = vol.geo.cluster_offset(lcn);
    let read = vol.cache.read(at, buf);
    vol.cache.discard(at, buf.len());
    Ok(read?)
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

/// The device pieces of bytes `start..start + len` of `stream` (whole
/// sectors, inside one page): (device sector, offset from `start`,
/// length).
fn pieces(geo: &crate::boot::Geometry, stream: &Stream, start: u64, len: usize) -> Result<Pieces> {
    let cs = u64::from(geo.cluster_size);
    let mut out = Pieces { list: [(0, 0, 0); PIECES], count: 0 };
    let mut done = 0;
    while done < len {
        let at = start + done as u64;
        let (lcn, n) = stream.runs.map(at / cs).ok_or(Error::Corrupt)?;
        if lcn == HOLE {
            return Err(Error::Corrupt);
        }
        let within = at % cs;
        let span = ((n * cs - within) as usize).min(len - done);
        let sector = (geo.cluster_offset(lcn) + within) / SECTOR;
        match out.count.checked_sub(1).map(|i| &mut out.list[i]) {
            Some(last) if last.0 + (last.2 as u64) / SECTOR == sector => last.2 += span,
            _ => {
                *out.list.get_mut(out.count).ok_or(Error::Corrupt)? = (sector, done, span);
                out.count += 1;
            }
        }
        done += span;
    }
    Ok(out)
}

/// A file's bytes `len` rounded up to whole sectors of `sector` bytes.
fn sectors_of(len: usize, sector: u32) -> usize {
    len.next_multiple_of(sector as usize)
}

/// The most pages one request to the disk moves.
const MAX_SEGMENTS: usize = 256;

/// The bytes of a resident `$INDEX_ROOT` named `$I30` with a value of
/// `len` bytes.
fn root_attr_len(len: usize) -> usize {
    (0x20 + len).next_multiple_of(8)
}

/// Reads the index block at VCN `vcn` of `blocks` into `buf`.
fn read_block(fs: &Inner, blocks: Option<&Stream>, vcn: u64, buf: &mut [u8]) -> Result<()> {
    let geo = &fs.vol.geo;
    let blocks = blocks.ok_or(Error::Corrupt)?;
    let at = vcn.checked_mul(u64::from(geo.index_vcn_size())).ok_or(Error::Corrupt)?;
    if at.checked_add(buf.len() as u64).is_none_or(|end| end > blocks.size) {
        return Err(Error::Corrupt);
    }
    fs.vol.read_stream(blocks, at, buf)?;
    fix_up(buf, b"INDX")?;
    if u64_at(buf, 0x10) != vcn {
        return Err(Error::Corrupt);
    }
    Ok(())
}

/// A directory's index blocks, for [`tree`]: in its `$INDEX_ALLOCATION`,
/// through the block cache, with its bitmap.
struct DirStore<'a> {
    fs: &'a Inner,
    blocks: &'a mut Option<Stream>,
    bitmap: &'a mut KVec<u8>,
    /// The bytes of its record but its index root's attribute.
    others: usize,
}

impl DirStore<'_> {
    /// How many blocks it has.
    fn count(&self) -> u64 {
        let size = u64::from(self.fs.vol.geo.index_size);
        self.blocks.as_ref().map_or(0, |b| b.size / size)
    }

    fn in_use(&self, block: u64) -> bool {
        self.bitmap.get((block / 8) as usize).is_some_and(|b| b & (1 << (block % 8)) != 0)
    }

    /// How many of its blocks are free.
    fn spare(&self) -> u64 {
        (0..self.count()).filter(|&b| !self.in_use(b)).count() as u64
    }

    /// Block `block`'s VCN.
    fn vcn_of(&self, block: u64) -> u64 {
        let geo = &self.fs.vol.geo;
        block * u64::from(geo.index_size) / u64::from(geo.index_vcn_size())
    }
}

impl Store for DirStore<'_> {
    fn read(&mut self, vcn: u64, buf: &mut [u8]) -> Result<()> {
        read_block(self.fs, self.blocks.as_ref(), vcn, buf)
    }

    fn write(&mut self, vcn: u64, buf: &[u8]) -> Result<()> {
        let geo = &self.fs.vol.geo;
        let blocks = self.blocks.as_ref().ok_or(Error::Corrupt)?;
        let mut out = mem::copy(buf)?;
        edit::fix_down(&mut out)?;
        let at = vcn * u64::from(geo.index_vcn_size());
        write_runs(&self.fs.vol.cache, geo, &blocks.runs, at, &out, false)
    }

    fn alloc(&mut self) -> Result<u64> {
        // Blocks were made ready before the change began
        // ([`NtfsNode::change_index`]).
        let block = (0..self.count()).find(|&b| !self.in_use(b)).ok_or(Error::NoSpace)?;
        self.bitmap[(block / 8) as usize] |= 1 << (block % 8);
        Ok(self.vcn_of(block))
    }

    fn free(&mut self, vcn: u64) -> Result<()> {
        let geo = &self.fs.vol.geo;
        let block = vcn * u64::from(geo.index_vcn_size()) / u64::from(geo.index_size);
        let byte = self.bitmap.get_mut((block / 8) as usize).ok_or(Error::Corrupt)?;
        *byte &= !(1 << (block % 8));
        Ok(())
    }

    fn root_fits(&self, len: usize) -> bool {
        self.others + root_attr_len(len) <= self.fs.vol.geo.record_size as usize
    }

    fn block_size(&self) -> usize {
        self.fs.vol.geo.index_size as usize
    }
}

/// A directory's fresh index root: of names, blocks as `fields` (bytes 8
/// to 16 of its parent's root: their size and clusters) say, no entries
/// but the last.
fn fresh_root(fields: &[u8]) -> Result<KVec<u8>> {
    let mut r = zeroed(0x30)?;
    put_u32(&mut r, 0, kind::FILE_NAME);
    // Collated as file names.
    put_u32(&mut r, 4, 1);
    r[8..16].copy_from_slice(fields);
    let h = ROOT_HEADER;
    put_u32(&mut r, h, 0x10);
    put_u32(&mut r, h + 4, 0x20);
    put_u32(&mut r, h + 8, 0x20);
    put_u16(&mut r, 0x28, 0x10);
    put_u16(&mut r, 0x2c, 2);
    Ok(r)
}

/// Puts `dir`'s root, and its bitmap if it has blocks, in its record
/// `buf`.
fn put_root(buf: &mut [u8], dir: &Dir) -> Result<()> {
    let at = edit::find(buf, kind::INDEX_ROOT, &I30).ok_or(Error::Corrupt)?;
    edit::set_value(buf, at, &dir.root)?;
    if dir.blocks.is_some() {
        let at = edit::find(buf, kind::BITMAP, &I30).ok_or(Error::Corrupt)?;
        edit::set_value(buf, at, &dir.bitmap)?;
    }
    Ok(())
}

/// A `$FILE_NAME` in a record: where it is, its namespace and its name.
struct NameAt {
    at: usize,
    ns: u8,
    units: KVec<u16>,
}

/// The names in the record `buf` that are in the directory whose record
/// is `dir`.
fn names_in(buf: &[u8], dir: u64) -> Result<KVec<NameAt>> {
    let mut out = mem::vec();
    for at in edit::find_all(buf, kind::FILE_NAME) {
        let off = at + usize::from(u16_at(buf, at + 0x14));
        let len = u32_at(buf, at + 0x10) as usize;
        let name = FileName::new(buf.get(off..off + len).ok_or(Error::Corrupt)?)?;
        if record_of(name.parent()) == dir {
            mem::push(
                &mut out,
                NameAt { at, ns: name.namespace(), units: collect(name.units())? },
            )?;
        }
    }
    Ok(out)
}

impl NtfsNode {
    /// Looks `units` up in this directory: the name of exactly that case,
    /// or one the same but for case. The caller holds `ns`.
    fn find(&self, scratch: &mut Scratch, units: &[u16]) -> Result<Option<index::Found>> {
        let st = self.state.lock();
        let dir = st.dir.as_ref().ok_or(Error::NotDirectory)?;
        let mut read = |vcn, buf: &mut [u8]| read_block(&self.fs, dir.blocks.as_ref(), vcn, buf);
        index::find(&dir.root, &self.fs.upcase, units, &mut read, &mut scratch.block)
    }

    /// Changes this directory's index by `op`, on its root's value and its
    /// blocks, and writes its record. A change that may add an entry of
    /// `adds` bytes (`Some(0)` for one that takes one out, which may move
    /// another up) first has free blocks for as many as it may need: two
    /// for a root with no room left, and one more than the tree is deep
    /// for one with blocks. The caller holds `ns`.
    fn change_index(
        &self,
        adds: Option<usize>,
        op: &mut dyn FnMut(&mut KVec<u8>, &mut DirStore<'_>) -> Result<()>,
    ) -> Result<()> {
        let rs = self.fs.vol.geo.record_size as usize;
        let mut st = self.state.lock();
        if st.listed {
            return Err(Error::NotSupported);
        }
        let dir = st.dir.as_mut().ok_or(Error::NotDirectory)?;
        if dir.fixed {
            return Err(Error::NotSupported);
        }
        let mut buf = self.read_rec()?;
        let had_blocks = dir.blocks.is_some();
        let grew = match adds {
            Some(adds) => self.prepare(dir, &mut buf, adds)?,
            None => false,
        };
        let root_at = edit::find(&buf, kind::INDEX_ROOT, &I30).ok_or(Error::Corrupt)?;
        let others = edit::used(&buf) - u32_at(&buf, root_at + 4) as usize;
        if others > rs {
            return Err(Error::Corrupt);
        }
        let mut root = mem::copy(&dir.root)?;
        let done = {
            let mut store =
                DirStore { fs: &self.fs, blocks: &mut dir.blocks, bitmap: &mut dir.bitmap, others };
            op(&mut root, &mut store)
        };
        match done {
            Ok(()) => {
                dir.root = root;
                put_root(&mut buf, dir)?;
                self.write_rec(&buf)
            }
            // Blocks first given to a root that then failed to go down
            // to them go back: its record never says it had them.
            Err(err) if grew && !had_blocks => {
                if let Some(blocks) = dir.blocks.take() {
                    for run in blocks.runs.as_slice() {
                        let _ = self.fs.give_clusters(run.lcn, run.len, true);
                    }
                }
                dir.bitmap.clear();
                Err(err)
            }
            Err(err) => {
                if grew {
                    put_root(&mut buf, dir)?;
                    self.write_rec(&buf)?;
                }
                Err(err)
            }
        }
    }

    /// Makes ready the free blocks a change adding an entry of `adds`
    /// bytes may need ([`NtfsNode::change_index`]), with the record `buf`
    /// saying where they are; whether it took any.
    fn prepare(&self, dir: &mut Dir, buf: &mut [u8], adds: usize) -> Result<bool> {
        let geo = &self.fs.vol.geo;
        let rs = geo.record_size as usize;
        let cs = u64::from(geo.cluster_size);
        let index_size = u64::from(geo.index_size);
        let needed = {
            let mut store = DirStore {
                fs: &self.fs,
                blocks: &mut dir.blocks,
                bitmap: &mut dir.bitmap,
                others: 0,
            };
            if store.blocks.is_none() {
                let root_at = edit::find(buf, kind::INDEX_ROOT, &I30).ok_or(Error::Corrupt)?;
                let others = edit::used(buf) - u32_at(buf, root_at + 4) as usize;
                // The entry, and a block's number in each entry if the
                // root goes down.
                if others + root_attr_len(dir.root.len() + adds + 8) <= rs { 0 } else { 2 }
            } else {
                let depth = tree::depth(&dir.root, &mut store)? as u64;
                let spare = store.spare();
                (depth + 2).saturating_sub(spare)
            }
        };
        if needed == 0 {
            return Ok(false);
        }
        let had = dir.blocks.is_some();
        if !had {
            // The root goes down to its first blocks in the change, so its
            // record makes room for where they are from its room: the
            // root it keeps meanwhile is an empty one.
            let mut fields = [0u8; 8];
            fields.copy_from_slice(dir.root.get(8..16).ok_or(Error::Corrupt)?);
            let at = edit::find(buf, kind::INDEX_ROOT, &I30).ok_or(Error::Corrupt)?;
            edit::set_value(buf, at, &fresh_root(&fields)?)?;
        }
        let blocks = dir.blocks.get_or_insert_with(Stream::new);
        let count = blocks.size / index_size;
        let size = (count + needed) * index_size;
        let more = size.saturating_sub(blocks.runs.clusters() * cs).div_ceil(cs);
        let near = blocks.runs.as_slice().last().map(|r| r.lcn + r.len);
        let old_clusters = blocks.runs.clusters();
        let old_size = blocks.size;
        let old_bitmap = dir.bitmap.len();
        let got = self.fs.take_clusters(more, near)?;
        let made = (|| {
            for &(lcn, n) in &got {
                blocks.runs.append(lcn, n)?;
            }
            blocks.size = size;
            blocks.initialized = size;
            let bytes = ((count + needed).div_ceil(64) * 8) as usize;
            while dir.bitmap.len() < bytes {
                mem::push(&mut dir.bitmap, 0)?;
            }
            let clusters = blocks.runs.clusters();
            let sizes = edit::Sizes { allocated: clusters * cs, size, initialized: size, ..edit::Sizes::default() };
            replace_named(buf, kind::INDEX_ALLOCATION, &I30, &blocks.runs, clusters, sizes)?;
            match edit::find(buf, kind::BITMAP, &I30) {
                Some(at) => edit::set_value(buf, at, &dir.bitmap)?,
                None => {
                    let i = u16_at(buf, 0x28);
                    let attr = edit::resident(kind::BITMAP, &I30, i, &dir.bitmap, false)?;
                    edit::insert(buf, &attr)?;
                    edit::next_instance(buf);
                }
            }
            Ok(())
        })();
        if let Err(err) = made {
            for &(lcn, n) in &got {
                let _ = self.fs.give_clusters(lcn, n, true);
            }
            let _ = blocks.runs.truncate(old_clusters);
            blocks.size = old_size;
            blocks.initialized = old_size;
            dir.bitmap.truncate(old_bitmap);
            if !had {
                dir.blocks = None;
            }
            return Err(err);
        }
        Ok(true)
    }

    /// Puts `name`'s entry for the file `reference`, with `dup` for what
    /// it says of the file, in this directory's index. The caller holds
    /// `ns`.
    fn update_entry(&self, name: &[u16], reference: u64, dup: &Dup) -> Result<()> {
        let upcase = &self.fs.upcase;
        self.change_index(None, &mut |root, store| {
            tree::update(root, store, upcase, name, reference, &mut |key| dup.put(key))
        })
    }

    /// The name of `node` in this directory that `units` asks for, as it
    /// is kept: of exactly that case, or the same but for case.
    fn stored_name(&self, node: &NtfsNode, units: &[u16]) -> Result<KVec<u16>> {
        let buf = node.read_rec()?;
        let names = names_in(&buf, self.record)?;
        let upcase = &self.fs.upcase;
        let pick = names
            .iter()
            .find(|n| n.units[..] == *units)
            .or_else(|| {
                names.iter().find(|n| {
                    upcase.compare(n.units.iter().copied(), units.iter().copied()) == Order::Equal
                })
            })
            .ok_or(Error::NotFound)?;
        collect(pick.units.iter().copied())
    }

    /// Takes the name `units` of `node` (of that case, or the same but for
    /// case) out of this directory, with its short name or the long one it
    /// is short for. The caller holds `ns`.
    fn remove_name(&self, node: &NtfsNode, units: &[u16]) -> Result<()> {
        node.editable()?;
        let reference = node.reference();
        let mut buf = node.read_rec()?;
        let names = names_in(&buf, self.record)?;
        let upcase = &self.fs.upcase;
        let pick = names
            .iter()
            .position(|n| n.units[..] == *units)
            .or_else(|| {
                names.iter().position(|n| {
                    upcase.compare(n.units.iter().copied(), units.iter().copied()) == Order::Equal
                })
            })
            .ok_or(Error::NotFound)?;
        let twin = match names[pick].ns {
            namespace::WIN32 => names.iter().position(|n| n.ns == namespace::DOS),
            namespace::DOS => names.iter().position(|n| n.ns == namespace::WIN32),
            _ => None,
        };
        let mut gone: KVec<usize> = mem::vec();
        mem::push(&mut gone, pick)?;
        if let Some(twin) = twin {
            mem::push(&mut gone, twin)?;
        }
        for &i in &gone {
            let name = &names[i].units;
            self.change_index(Some(0), &mut |root, store| {
                tree::remove(root, store, upcase, name, reference)
            })?;
        }
        // The later attribute first, so that the other stays where it is.
        gone.sort_unstable_by_key(|&i| core::cmp::Reverse(names[i].at));
        for &i in &gone {
            edit::remove(&mut buf, names[i].at);
        }
        node.write_rec(&buf)?;
        let lost = gone.iter().filter(|&&i| names[i].ns != namespace::DOS).count() as u32;
        let left = first_parent(&buf)?;
        let mut st = node.state.lock();
        st.links = st.links.saturating_sub(lost);
        if st.links == 0 {
            st.removed = true;
        } else {
            if record_of(st.parent) == self.record
                && let Some(parent) = left
            {
                st.parent = parent;
            }
            st.info.changed = now_coarse();
            st.behind();
        }
        drop(st);
        let _ = node.pin();
        Ok(())
    }

    /// Gives `node` the name `units`, in namespace `ns`, in this
    /// directory, and counts it among its links. The caller holds `ns`.
    fn add_name(&self, node: &NtfsNode, units: &[u16], ns: u8) -> Result<()> {
        node.editable()?;
        let reference = node.reference();
        let size = node.cache.as_ref().map_or(0, FileCache::size);
        let dup = node.dup(&node.state.lock(), size);
        let key = names::file_name(self.reference(), &dup, units, ns)?;
        let mut buf = node.read_rec()?;
        let need = (0x18 + key.len()).next_multiple_of(8);
        if edit::room(&buf) < need {
            node.make_room(&mut buf, need, size)?;
        }
        let i = u16_at(&buf, 0x28);
        let attr = edit::resident(kind::FILE_NAME, &[], i, &key, true)?;
        edit::insert(&mut buf, &attr)?;
        edit::next_instance(&mut buf);
        let entry = tree::make_entry(reference, &key, None)?;
        let upcase = &self.fs.upcase;
        self.change_index(Some(entry.len()), &mut |root, store| {
            tree::insert(root, store, upcase, &entry)
        })?;
        node.write_rec(&buf)?;
        if ns != namespace::DOS {
            node.state.lock().links += 1;
        }
        Ok(())
    }

    /// Makes a record for a new file or directory (`kind`) named `units`
    /// (in namespace `ns`) in this directory; returns its reference and
    /// its `$FILE_NAME`. The caller holds `ns`.
    #[inline(never)]
    fn new_record(&self, kind: Kind, units: &[u16], ns: u8) -> Result<(u64, KVec<u8>)> {
        let (parent, security, mut fields, compress) = {
            let st = self.state.lock();
            let dir = st.dir.as_ref().ok_or(Error::NotDirectory)?;
            let mut fields = [0u8; 8];
            fields.copy_from_slice(dir.root.get(8..16).ok_or(Error::Corrupt)?);
            // What is made in a directory marked compressed is, as in
            // Windows, on volumes whose clusters Windows compresses.
            let compress = st.info.attributes & attributes::COMPRESSED != 0
                && u64::from(self.fs.vol.geo.cluster_size) <= MAX_COMPRESS_CLUSTER;
            (self.record | (u64::from(st.sequence) << 48), st.info.security, fields, compress)
        };
        let fs = &self.fs;
        let number = fs.take_record()?;
        let made = (|| {
            let mut buf = zeroed(fs.vol.geo.record_size as usize)?;
            let sequence = match fs.vol.read_record(number, &mut buf) {
                Ok(()) => {
                    if Record::new(&buf).is_ok_and(|r| r.in_use()) {
                        return Err(Error::Corrupt);
                    }
                    u16_at(&buf, 0x10).max(1)
                }
                Err(_) => 1,
            };
            let is_dir = kind == Kind::Directory;
            let t = names::to_nt(now());
            let file_attributes = if is_dir { 0 } else { attributes::ARCHIVE }
                | if compress { attributes::COMPRESSED } else { 0 };
            let dup = Dup {
                created: t,
                modified: t,
                changed: t,
                accessed: t,
                allocated: 0,
                size: 0,
                flags: file_attributes | if is_dir { attributes::DIRECTORY } else { 0 },
            };
            let flags = record::flags::IN_USE | if is_dir { record::flags::DIRECTORY } else { 0 };
            edit::format_record(&mut buf, number, sequence, flags);
            let si = names::standard_information(&dup, file_attributes, security)?;
            let key = names::file_name(parent, &dup, units, ns)?;
            let put = |buf: &mut [u8], kind: u32, name: &[u8], value: &[u8], indexed: bool| {
                let i = edit::next_instance(buf);
                edit::insert(buf, &edit::resident(kind, name, i, value, indexed)?)
            };
            put(&mut buf, kind::STANDARD_INFORMATION, &[], &si, false)?;
            put(&mut buf, kind::FILE_NAME, &[], &key, true)?;
            if is_dir {
                if fields[0..4] == [0; 4] {
                    fields[0..4].copy_from_slice(&fs.vol.geo.index_size.to_le_bytes());
                }
                put(&mut buf, kind::INDEX_ROOT, &I30, &fresh_root(&fields)?, false)?;
            } else {
                put(&mut buf, kind::DATA, &[], &[], false)?;
            }
            fs.vol.write_record(number, &buf, false)?;
            Ok((number | (u64::from(sequence) << 48), key))
        })();
        if made.is_err() {
            let _ = fs.give_record(number);
        }
        made
    }

    /// Whether this directory has no entries.
    fn is_empty_dir(&self) -> Result<bool> {
        let mut st = self.state.lock();
        let dir = st.dir.as_mut().ok_or(Error::NotDirectory)?;
        let mut store =
            DirStore { fs: &self.fs, blocks: &mut dir.blocks, bitmap: &mut dir.bitmap, others: 0 };
        let mut empty = true;
        tree::walk(&dir.root, &mut store, &self.fs.upcase, None, &mut |_| {
            empty = false;
            Ok(false)
        })?;
        Ok(empty)
    }

    /// Whether `dir` is this directory or lies under it, as their records
    /// say.
    fn holds(&self, dir: &NtfsNode) -> Result<bool> {
        let mut number = dir.record;
        for _ in 0..MAX_DEPTH {
            if number == self.record {
                return Ok(true);
            }
            if number == records::ROOT {
                return Ok(false);
            }
            number = record_of(self.fs.vol.load(number)?.parent);
        }
        Err(Error::Loop)
    }

    /// The kind of the file an entry with the name `name` names: as the
    /// name says, but for a system file, which may be an Interix link.
    fn kind_of(&self, reference: u64, name: &FileName<'_>) -> Kind {
        let attrs = name.attributes();
        if attrs & attributes::REPARSE_POINT != 0 && reparse::is_link(name.reparse_tag()) {
            Kind::Symlink
        } else if attrs & attributes::DIRECTORY != 0 {
            Kind::Directory
        } else if attrs & attributes::SYSTEM != 0 {
            Inner::node_for(&self.fs, reference).map_or(Kind::File, |node| node.kind)
        } else {
            Kind::File
        }
    }

    /// How many directories its first name's is below the root.
    fn depth(&self) -> Result<usize> {
        let mut number = record_of(self.state.lock().parent);
        let mut depth = 0;
        while number != records::ROOT {
            depth += 1;
            if depth > MAX_DEPTH {
                return Err(Error::Loop);
            }
            number = record_of(self.fs.vol.load(number)?.parent);
        }
        Ok(depth)
    }

    /// A link's target as a path.
    fn target(&self) -> Result<&[u8]> {
        if let Some(path) = self.path.get() {
            return Ok(path);
        }
        let path = match self.link.as_ref().ok_or(Error::NotLink)? {
            Target::Path(path) => mem::copy(path)?,
            Target::Absolute(path) => {
                let depth = self.depth()?;
                let mut out = mem::vec_with_capacity(3 * depth + path.len().max(1))?;
                for _ in 0..depth {
                    mem::extend(&mut out, b"../")?;
                }
                mem::extend(&mut out, path)?;
                if out.is_empty() {
                    mem::extend(&mut out, b".")?;
                }
                out
            }
        };
        let _ = self.path.set(path);
        self.path.get().map(|p| &p[..]).ok_or(Error::NoMemory)
    }
}

/// The directory of the first name in the record `buf` that is not a
/// short one, if it has one.
fn first_parent(buf: &[u8]) -> Result<Option<u64>> {
    for at in edit::find_all(buf, kind::FILE_NAME) {
        let off = at + usize::from(u16_at(buf, at + 0x14));
        let len = u32_at(buf, at + 0x10) as usize;
        let name = FileName::new(buf.get(off..off + len).ok_or(Error::Corrupt)?)?;
        if name.namespace() != namespace::DOS {
            return Ok(Some(name.parent()));
        }
    }
    Ok(None)
}

/// Puts the units of `name` in `out` as UTF-8; returns how many bytes.
/// Units that are not valid UTF-16 become U+FFFD.
fn utf8(units: impl Iterator<Item = u16>, out: &mut [u8; NAME_MAX * 3]) -> usize {
    let mut len = 0;
    for c in char::decode_utf16(units) {
        let mut buf = [0u8; 4];
        let s = c.unwrap_or(char::REPLACEMENT_CHARACTER).encode_utf8(&mut buf);
        if len + s.len() > out.len() {
            break;
        }
        out[len..len + s.len()].copy_from_slice(s.as_bytes());
        len += s.len();
    }
    len
}

/// The UTF-16 units of `name`, in a vector.
fn units_vec(name: &str) -> Result<KVec<u16>> {
    let mut units = [0u16; NAME_MAX];
    let n = names::units(name, &mut units)?;
    collect(units[..n].iter().copied())
}

/// Whether `moving` may replace `old`: a file a file, a directory an
/// empty directory.
fn check_replace(moving: Kind, old: &NtfsNode) -> Result<()> {
    match (moving, old.kind) {
        (_, Kind::Directory) if !old.is_empty_dir()? => Err(Error::NotEmpty),
        (Kind::Directory, Kind::Directory) => Ok(()),
        (Kind::Directory, _) => Err(Error::NotDirectory),
        (_, Kind::Directory) => Err(Error::IsDirectory),
        _ => Ok(()),
    }
}

impl Node for NtfsNode {
    fn stat(&self) -> Stat {
        let cs = u64::from(self.fs.vol.geo.cluster_size);
        let (info, links, dir_size, allocated, data_allocated) = {
            let st = self.state.lock();
            let dir_size = st
                .dir
                .as_ref()
                .map_or(0, |d| d.blocks.as_ref().map_or(d.root.len() as u64, |b| b.size));
            let data_allocated = match (&st.data, &st.fixed) {
                (Some(Data::Clusters(s)), None) => {
                    Some(s.runs.allocated_in(0, s.runs.clusters()) * cs)
                }
                (Some(Data::Resident(_)), None) => Some(0),
                _ => None,
            };
            // A file whose last name went has none, as POSIX has it.
            let links = if st.removed { 0 } else { st.links.max(1) };
            (st.info, links, dir_size, st.allocated, data_allocated)
        };
        let (size, modified) = match (&self.cache, self.kind) {
            (Some(cache), _) => (cache.size(), cache.modified()),
            (None, Kind::Symlink) => (self.target().map_or(0, |t| t.len() as u64), info.modified),
            (None, _) => (dir_size, info.modified),
        };
        Stat {
            kind: self.kind,
            size,
            allocated: data_allocated.unwrap_or(allocated),
            id: self.record,
            created: info.created,
            modified: names::coarse(modified),
            accessed: info.accessed,
            changed: info.changed.max(names::coarse(modified)),
            links,
            device: 0,
        }
    }

    fn kind(&self) -> Kind {
        self.kind
    }

    fn lookup(&self, name: &str) -> Result<NodeRef> {
        self.is_dir()?;
        let found = {
            let mut ns = self.fs.ns.lock();
            let scratch = &mut **ns;
            let n = names::units(name, &mut scratch.units)?;
            let units = scratch.units;
            self.find(scratch, &units[..n])?
        };
        let found = found.ok_or(Error::NotFound)?;
        Ok(Inner::node_for(&self.fs, found.reference)?)
    }

    // NTFS keeps Windows' permissions, which Radix does not map: as
    // Linux's drivers show them with the default umask of 022, everything
    // is user 0's and 0755, and a read-only file loses its write bits.
    fn access(&self) -> Access {
        let mut mode = if self.kind == Kind::Symlink { 0o777 } else { 0o755 };
        if self.kind == Kind::File && self.state.lock().info.attributes & attributes::READ_ONLY != 0
        {
            mode &= !0o222;
        }
        Access { owner: 0, group: 0, mode }
    }

    // As Linux's ntfs3: a file without write bits is read-only for
    // Windows (its attribute); any other change of the bits is taken and
    // changes nothing, and a change of owner or group fails.
    fn set_access(&self, change: &AccessChange) -> Result<()> {
        self.writable()?;
        if !change.fits_none_kept(self.access()) {
            return Err(Error::NotSupported);
        }
        let Some(mode) = change.mode.filter(|_| self.kind == Kind::File) else { return Ok(()) };
        {
            let mut st = self.state.lock();
            let attributes = &mut st.info.attributes;
            let new = if mode & 0o222 == 0 {
                *attributes | attributes::READ_ONLY
            } else {
                *attributes & !attributes::READ_ONLY
            };
            if new == *attributes {
                return Ok(());
            }
            *attributes = new;
            st.info.changed = now_coarse();
        }
        self.changed();
        Ok(())
    }

    fn create(&self, name: &str, kind: Kind, _access: Access) -> Result<NodeRef> {
        self.writable()?;
        check_name(name)?;
        self.is_dir()?;
        if !matches!(kind, Kind::File | Kind::Directory) {
            return Err(Error::NotSupported);
        }
        names::check_windows(name)?;
        let units = units_vec(name)?;
        let mut ns = self.fs.ns.lock();
        if self.removed() {
            return Err(Error::NotFound);
        }
        self.editable()?;
        if self.find(&mut ns, &units)?.is_some() {
            return Err(Error::Exists);
        }
        self.fs.changing()?;
        let (reference, key) = self.new_record(kind, &units, names::namespace_of(name))?;
        let entry = tree::make_entry(reference, &key, None)?;
        let upcase = &self.fs.upcase;
        let put = self.change_index(Some(entry.len()), &mut |root, store| {
            tree::insert(root, store, upcase, &entry)
        });
        if let Err(err) = put {
            let _ = NtfsNode::free_record(&self.fs, record_of(reference));
            return Err(err);
        }
        self.touch();
        let node = Inner::node_for(&self.fs, reference)?;
        drop(ns);
        Ok(node)
    }

    fn link(&self, name: &str, node: &NodeRef) -> Result<()> {
        self.writable()?;
        check_name(name)?;
        self.is_dir()?;
        let other = radix_vfs::downcast::<NtfsNode>(&**node).ok_or(Error::CrossMount)?;
        if !KArc::ptr_eq(&other.fs, &self.fs) {
            return Err(Error::CrossMount);
        }
        if other.kind == Kind::Directory {
            return Err(Error::IsDirectory);
        }
        names::check_windows(name)?;
        let units = units_vec(name)?;
        let mut ns = self.fs.ns.lock();
        if self.removed() || other.removed() {
            return Err(Error::NotFound);
        }
        self.editable()?;
        if other.state.lock().links >= MAX_LINKS {
            return Err(Error::TooManyNames);
        }
        if self.find(&mut ns, &units)?.is_some() {
            return Err(Error::Exists);
        }
        self.fs.changing()?;
        self.add_name(other, &units, names::namespace_of(name))?;
        {
            let mut st = other.state.lock();
            st.info.changed = now_coarse();
            st.behind();
        }
        let _ = other.pin();
        self.touch();
        Ok(())
    }

    fn read_link(&self, out: &mut dyn Out) -> Result<usize> {
        if self.kind != Kind::Symlink {
            return Err(Error::NotLink);
        }
        let path = self.target()?;
        let n = path.len().min(out.len());
        out.put(0, &path[..n])?;
        Ok(path.len())
    }

    fn remove(&self, name: &str) -> Result<()> {
        self.writable()?;
        check_name(name)?;
        self.is_dir()?;
        let units = units_vec(name)?;
        let mut ns = self.fs.ns.lock();
        let found = self.find(&mut ns, &units)?.ok_or(Error::NotFound)?;
        let node = Inner::node_for(&self.fs, found.reference)?;
        if node.kind == Kind::Directory && !node.is_empty_dir()? {
            return Err(Error::NotEmpty);
        }
        self.editable()?;
        self.fs.changing()?;
        self.remove_name(&node, &units)?;
        self.touch();
        // Its record and clusters go when the last reference to it does:
        // now, unless a handle has it.
        drop(node);
        drop(ns);
        Ok(())
    }

    fn rename(&self, from: &str, to_dir: &dyn Node, to: &str) -> Result<()> {
        self.writable()?;
        check_name(from)?;
        check_name(to)?;
        self.is_dir()?;
        let target = radix_vfs::downcast::<NtfsNode>(to_dir).ok_or(Error::CrossMount)?;
        if !KArc::ptr_eq(&target.fs, &self.fs) {
            return Err(Error::CrossMount);
        }
        target.is_dir()?;
        names::check_windows(to)?;
        let (from_units, to_units) = (units_vec(from)?, units_vec(to)?);
        let mut ns = self.fs.ns.lock();
        if self.removed() || target.removed() {
            return Err(Error::NotFound);
        }
        let same_dir = core::ptr::eq(self, target);
        let source = self.find(&mut ns, &from_units)?.ok_or(Error::NotFound)?;
        let moving = Inner::node_for(&self.fs, source.reference)?;
        let stored = self.stored_name(&moving, &from_units)?;
        if same_dir && stored[..] == to_units[..] {
            return Ok(());
        }
        if moving.kind == Kind::Directory && !same_dir && moving.holds(target)? {
            return Err(Error::BadName);
        }
        let upcase = &self.fs.upcase;
        let replaced = match target.find(&mut ns, &to_units)? {
            Some(old) if record_of(old.reference) == moving.record => {
                let case_only = same_dir
                    && upcase.compare(stored.iter().copied(), to_units.iter().copied())
                        == Order::Equal;
                if !case_only {
                    // Two names of one file: as POSIX has it, nothing
                    // changes.
                    return Ok(());
                }
                None
            }
            Some(old) => {
                let node = Inner::node_for(&self.fs, old.reference)?;
                check_replace(moving.kind, &node)?;
                Some(node)
            }
            None => None,
        };
        self.editable()?;
        target.editable()?;
        self.fs.changing()?;
        if let Some(old) = &replaced {
            target.remove_name(old, &to_units)?;
        }
        target.add_name(&moving, &to_units, names::namespace_of(to))?;
        self.remove_name(&moving, &stored)?;
        {
            let mut st = moving.state.lock();
            if record_of(st.parent) == self.record {
                st.parent = target.reference();
            }
        }
        self.touch();
        if !same_dir {
            target.touch();
        }
        drop(replaced);
        drop(moving);
        drop(ns);
        Ok(())
    }

    fn list(&self, cursor: u64, f: ListFn<'_>) -> Result<()> {
        self.is_dir()?;
        let mut ns = self.fs.ns.lock();
        let Scratch { units: last, text, .. } = &mut **ns;
        let mut st = self.state.lock();
        let dir = st.dir.as_mut().ok_or(Error::NotDirectory)?;
        // Where the listing goes on from: after the name a mark keeps, past
        // as many entries after it as the cursor says it gave.
        let id = (cursor >> 32) as u32;
        let given = cursor as u32;
        let (start, base) = if cursor == 0 {
            (None, 0)
        } else {
            let Some(mark) = dir.marks.iter().find(|m| m.id == id) else {
                // A mark forgotten for newer ones: the listing ends.
                return Ok(());
            };
            match &mark.last {
                Some(name) if mark.count == given => (Some(collect(name.iter().copied())?), given),
                _ => match &mark.start {
                    Some(name) => (Some(collect(name.iter().copied())?), mark.base),
                    None => (None, mark.base),
                },
            }
        };
        let skip = given.saturating_sub(base);
        let new_id = dir.next_mark;
        dir.next_mark = dir.next_mark.wrapping_add(1).max(1);
        let mut count = base + skip;
        let mut skipped = 0;
        let mut last_len = None;
        let mut last_count = 0;
        let fs = &self.fs;
        {
            let mut store =
                DirStore { fs, blocks: &mut dir.blocks, bitmap: &mut dir.bitmap, others: 0 };
            tree::walk(&dir.root, &mut store, &fs.upcase, start.as_deref(), &mut |e| {
                let reference = u64_at(e, 0);
                let key_len = usize::from(u16_at(e, 0x0a));
                let name = FileName::new(e.get(0x10..0x10 + key_len).ok_or(Error::Corrupt)?)?;
                // A short name repeats a long one; the system's files, and
                // the root's entry for itself, are left out, as Linux's
                // drivers leave them.
                if name.namespace() == namespace::DOS || record_of(reference) < records::FIRST_USER
                {
                    return Ok(true);
                }
                if skipped < skip {
                    skipped += 1;
                    return Ok(true);
                }
                count += 1;
                let kind = self.kind_of(reference, &name);
                let len = utf8(name.units(), text);
                let next = (u64::from(new_id) << 32) | u64::from(count);
                if !f(next, core::str::from_utf8(&text[..len]).unwrap_or(""), kind) {
                    return Ok(false);
                }
                for (slot, unit) in last.iter_mut().zip(name.units()) {
                    *slot = unit;
                }
                last_len = Some(name.len());
                last_count = count;
                Ok(true)
            })?;
        }
        let mark = Mark {
            id: new_id,
            start,
            base,
            last: match last_len {
                Some(n) => Some(collect(last[..n].iter().copied())?),
                None => None,
            },
            count: last_count,
        };
        if dir.marks.len() >= MARKS {
            dir.marks.remove(0);
        }
        mem::push(&mut dir.marks, mark)?;
        Ok(())
    }

    fn read(&self, offset: u64, out: &mut dyn Out) -> Result<usize> {
        let cache = self.cache.as_ref().ok_or(Error::IsDirectory)?;
        if let Some(Data::Unreadable(err)) = &self.state.lock().data {
            return Err(*err);
        }
        cache.read(offset, out, &FileBacking(self))
    }

    fn write(&self, offset: u64, data: &dyn In) -> Result<usize> {
        self.writable()?;
        let cache = self.file()?;
        self.data_writable()?;
        let result = cache.write(offset, data, &FileBacking(self));
        self.data_changed();
        result
    }

    fn set_size(&self, size: u64) -> Result<()> {
        self.writable()?;
        let cache = self.file()?;
        self.data_writable()?;
        let result = cache.set_size(size, &FileBacking(self));
        self.data_changed();
        result
    }

    fn set_times(&self, accessed: Option<u64>, modified: Option<u64>) -> Result<()> {
        self.writable()?;
        // As NTFS keeps them, so that what `stat` says now is what the
        // disk will say: to 100 ns. A file's change time is its cache's;
        // the cache's lock comes before the node's (write-back takes it),
        // so they are taken one at a time.
        let modified = modified.map(names::coarse);
        if let (Some(modified), Some(cache)) = (modified, &self.cache) {
            cache.set_modified(modified);
        }
        {
            let mut st = self.state.lock();
            if let Some(accessed) = accessed {
                st.info.accessed = names::coarse(accessed);
            }
            if let (Some(modified), None) = (modified, &self.cache) {
                st.info.modified = modified;
            }
            st.info.changed = now_coarse();
        }
        self.changed();
        Ok(())
    }

    fn sync(&self) -> Result<()> {
        // The whole volume: the bitmaps a file's clusters are in are
        // shared, and the volume is then consistent on the disk.
        self.fs.sync_all()
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

impl Drop for NtfsNode {
    fn drop(&mut self) {
        // A removed file's record and clusters were kept for it; they go
        // with it.
        let st = self.state.get_mut();
        if self.fs.read_only || !st.removed || st.listed {
            return;
        }
        if let Some(Data::Clusters(stream)) = &st.data
            && st.fixed.is_none()
        {
            for run in stream.runs.as_slice() {
                if run.lcn != HOLE {
                    let _ = self.fs.give_clusters(run.lcn, run.len, false);
                }
            }
        }
        if let Some(Dir { blocks: Some(blocks), .. }) = &st.dir {
            for run in blocks.runs.as_slice() {
                if run.lcn != HOLE {
                    let _ = self.fs.give_clusters(run.lcn, run.len, true);
                }
            }
        }
        let _ = NtfsNode::free_record(&self.fs, self.record);
    }
}

/// A file's page cache backing: its clusters, or its record.
struct FileBacking<'a>(&'a NtfsNode);

impl FileBacking<'_> {
    /// Reads the first `len` bytes of page `index` of the uncompressed
    /// `stream`; the rest of the page is zeros.
    fn plain(&self, stream: &Stream, index: u64, page: &mut Page, len: usize) -> Result<()> {
        let vol = &self.0.fs.vol;
        let cs = u64::from(vol.geo.cluster_size);
        let ss = vol.geo.sector_size as usize;
        let pos = index * PAGE;
        let written = stream.initialized.saturating_sub(pos).min(len as u64) as usize;
        let device = vol.cache.device();
        let mut done = 0;
        while done < written {
            let at = pos + done as u64;
            let (lcn, n) = stream.runs.map(at / cs).ok_or(Error::Corrupt)?;
            let within = at % cs;
            let span = (n * cs - within).min((written - done) as u64) as usize;
            if lcn == HOLE {
                page.bytes_mut()[done..done + span].fill(0);
            } else {
                // Whole sectors, which the cluster holds.
                let whole = span.next_multiple_of(ss).min(PAGE_SIZE - done);
                let sector = (vol.geo.cluster_offset(lcn) + within) / SECTOR;
                device.read(
                    sector,
                    &mut [SegmentMut { page: &mut *page, offset: done, len: whole }],
                )?;
                radix_hw::device_wrote(&mut page.bytes_mut()[done..done + whole]);
            }
            done += span;
        }
        page.bytes_mut()[written..].fill(0);
        Ok(())
    }

    /// Reads page `index` of the compressed `stream`, of `len` bytes in
    /// the file, by its unit: a unit all on the disk is not compressed, a
    /// unit all holes is zeros, and the others are uncompressed whole,
    /// their other pages offered to `more`.
    fn compressed(
        &self,
        stream: &Stream,
        index: u64,
        page: &mut Page,
        len: usize,
        more: &mut dyn FnMut(u64, &[u8]) -> bool,
    ) -> Result<()> {
        let vol = &self.0.fs.vol;
        let cs = u64::from(vol.geo.cluster_size);
        let per_unit = 1u64 << stream.unit;
        let unit_bytes = cs * per_unit;
        let pos = index * PAGE;
        let unit = pos / unit_bytes;
        let first = unit * per_unit;
        let stored = stream.runs.allocated_in(first, per_unit);
        if stored == per_unit {
            return self.plain(stream, index, page, len);
        }
        if stored == 0 {
            page.bytes_mut().fill(0);
            return Ok(());
        }
        // The unit's data is in its first clusters, the rest holes.
        let mut packed = zeroed((stored * cs) as usize)?;
        let mut vcn = first;
        while vcn < first + stored {
            let (lcn, n) = stream.runs.map(vcn).ok_or(Error::Corrupt)?;
            if lcn == HOLE {
                return Err(Error::Corrupt);
            }
            let n = n.min(first + stored - vcn);
            let at = ((vcn - first) * cs) as usize;
            read_data(vol, lcn, &mut packed[at..at + (n * cs) as usize])?;
            vcn += n;
        }
        let mut plain = zeroed(unit_bytes as usize)?;
        lznt1::decompress(&packed, &mut plain)?;
        // What was not written, and what is past the file's end, is zeros.
        let start = unit * unit_bytes;
        let keep = stream.initialized.min(stream.size).saturating_sub(start).min(unit_bytes);
        plain[keep as usize..].fill(0);
        let at = (pos - start) as usize;
        page.bytes_mut()[..len].copy_from_slice(&plain[at..at + len]);
        page.bytes_mut()[len..].fill(0);
        let pages = unit_bytes / PAGE;
        let first_page = start / PAGE;
        for p in first_page..first_page + pages {
            if p == index || p * PAGE >= stream.size {
                continue;
            }
            let at = ((p - first_page) * PAGE) as usize;
            if !more(p, &plain[at..at + PAGE_SIZE]) {
                break;
            }
        }
        Ok(())
    }

    /// Reads compression unit `unit` of the compressed `stream` into
    /// `plain`, uncompressed, with `packed` to read it in: what lies past
    /// the data written is zeros.
    fn unit_plain(
        &self,
        stream: &Stream,
        unit: u64,
        plain: &mut [u8],
        packed: &mut [u8],
    ) -> Result<()> {
        let vol = &self.0.fs.vol;
        let cs = u64::from(vol.geo.cluster_size);
        let per_unit = stream.grain();
        let first = unit * per_unit;
        let stored = stream.runs.allocated_in(first, per_unit);
        if stored == 0 {
            plain.fill(0);
            return Ok(());
        }
        // A unit all on the disk is not compressed; one in fewer clusters
        // is, in its first ones.
        let into: &mut [u8] = if stored == per_unit { plain } else { &mut *packed };
        let mut vcn = first;
        while vcn < first + stored {
            let (lcn, n) = stream.runs.map(vcn).ok_or(Error::Corrupt)?;
            if lcn == HOLE {
                return Err(Error::Corrupt);
            }
            let n = n.min(first + stored - vcn);
            let at = ((vcn - first) * cs) as usize;
            read_data(vol, lcn, &mut into[at..at + (n * cs) as usize])?;
            vcn += n;
        }
        if stored < per_unit {
            packed[(stored * cs) as usize..].fill(0);
            lznt1::decompress(&packed[..(stored * cs) as usize], plain)?;
        }
        let start = unit * cs * per_unit;
        let keep = stream.initialized.saturating_sub(start).min(plain.len() as u64);
        plain[keep as usize..].fill(0);
        Ok(())
    }

    /// Writes compression unit `unit` of the compressed `stream` as
    /// `plain` holds it: holes if it is all zeros, compressed in fewer
    /// clusters if it compresses into them and the record has room for
    /// where they are, else as it is. It goes to clusters taken for it,
    /// before those it had are given back.
    fn store_unit(
        &self,
        stream: &mut Stream,
        unit: u64,
        plain: &[u8],
        packed: &mut [u8],
        matcher: &mut lznt1::Matcher,
    ) -> Result<()> {
        let node = self.0;
        let cs = u64::from(node.fs.vol.geo.cluster_size);
        let per_unit = stream.grain();
        let first = unit * per_unit;
        // Where the unit is now, to put back if the new one does not fit.
        let mut old: KVec<(u64, u64)> = mem::vec();
        let mut vcn = first;
        while vcn < first + per_unit {
            let (lcn, n) = stream.runs.map(vcn).ok_or(Error::Corrupt)?;
            let n = n.min(first + per_unit - vcn);
            mem::push(&mut old, (lcn, n))?;
            vcn += n;
        }
        let near = old.iter().find(|(l, _)| *l != HOLE).map(|&(l, _)| l).or_else(|| {
            first.checked_sub(1).and_then(|v| stream.runs.map(v)).and_then(|(l, _)| {
                (l != HOLE).then(|| l + 1)
            })
        });
        let room = ((per_unit - 1) * cs) as usize;
        let zeros = plain.iter().all(|&b| b == 0);
        let compressed = if zeros { Some(0) } else { lznt1::compress(plain, &mut packed[..room], matcher) };
        for try_packed in [true, false] {
            let (bytes, k): (&[u8], u64) = match compressed {
                Some(n) if try_packed => {
                    let k = (n as u64).div_ceil(cs);
                    packed[n..(k * cs) as usize].fill(0);
                    (&packed[..(k * cs) as usize], k)
                }
                None if try_packed => continue,
                _ => (plain, per_unit),
            };
            let got = if k == 0 { mem::vec() } else { node.fs.take_clusters(k, near)? };
            let mut with = mem::vec_with_capacity(got.len() + 1)?;
            for &run in &got {
                let _ = with.push_within_capacity(run);
            }
            if k < per_unit {
                let _ = with.push_within_capacity((HOLE, per_unit - k));
            }
            let give_back = |got: &[(u64, u64)]| {
                for &(lcn, n) in got {
                    let _ = node.fs.give_clusters(lcn, n, false);
                }
            };
            if let Err(err) = write_clusters(&node.fs, &got, bytes) {
                give_back(&got);
                return Err(err);
            }
            let freed = match stream.runs.replace(first, per_unit, &with) {
                Ok(freed) => freed,
                Err(err) => {
                    give_back(&got);
                    return Err(err);
                }
            };
            match node.runs_fit(stream) {
                Ok(()) => {
                    for run in &freed {
                        let _ = node.fs.give_clusters(run.lcn, run.len, false);
                    }
                    return Ok(());
                }
                Err(err) => {
                    // Put the unit back as it was; stored as it is, it may
                    // join its neighbours' runs.
                    stream.runs.replace(first, per_unit, &old)?;
                    give_back(&got);
                    if !try_packed {
                        return Err(err);
                    }
                }
            }
        }
        Err(Error::NoSpace)
    }

    /// Rewrites the compression units of the compressed `stream` that
    /// bytes `from..to` fall in, each read, changed by `change` (given the
    /// unit's first byte and its bytes) and stored again; one the change
    /// covers whole is not read.
    fn rewrite_units(
        &self,
        stream: &mut Stream,
        from: u64,
        to: u64,
        change: &mut dyn FnMut(u64, &mut [u8]),
    ) -> Result<()> {
        let cs = u64::from(self.0.fs.vol.geo.cluster_size);
        let unit_bytes = cs * stream.grain();
        let mut plain = zeroed(unit_bytes as usize)?;
        let mut packed = zeroed(unit_bytes as usize)?;
        let mut matcher = lznt1::Matcher::new()?;
        let mut unit = from / unit_bytes;
        while unit * unit_bytes < to {
            let start = unit * unit_bytes;
            if from <= start && to >= start + unit_bytes {
                plain.fill(0);
            } else {
                self.unit_plain(stream, unit, &mut plain, &mut packed)?;
            }
            change(start, &mut plain);
            self.store_unit(stream, unit, &plain, &mut packed, &mut matcher)?;
            unit += 1;
        }
        Ok(())
    }

    /// Writes pages from byte `pos` on of the compressed `stream`, the
    /// last `len` bytes long, a compression unit at a time.
    fn write_units(&self, stream: &mut Stream, pos: u64, pages: &[&Page], len: usize) -> Result<()> {
        let cs = u64::from(self.0.fs.vol.geo.cluster_size);
        let unit_bytes = cs * stream.grain();
        let last = pages.len() - 1;
        let end = pos + last as u64 * PAGE + len as u64;
        // What lies between the data written and these pages reads as
        // zeros: the units it is in must hold zeros there, or none.
        if pos > stream.initialized {
            let from = stream.initialized;
            let gap_end = (pos / unit_bytes * unit_bytes).max(from);
            let whole_from = from.next_multiple_of(unit_bytes);
            if from < gap_end && from % unit_bytes != 0 {
                let unit_end = whole_from.min(gap_end);
                self.rewrite_units(stream, from, unit_end, &mut |_, _| {})?;
            }
            // The units past it, before these pages', are holes.
            let mut unit = whole_from / unit_bytes;
            while (unit + 1) * unit_bytes <= gap_end {
                if stream.runs.allocated_in(unit * stream.grain(), stream.grain()) != 0 {
                    let mut packed = zeroed(unit_bytes as usize)?;
                    let mut matcher = lznt1::Matcher::new()?;
                    let plain = zeroed(unit_bytes as usize)?;
                    self.store_unit(stream, unit, &plain, &mut packed, &mut matcher)?;
                }
                unit += 1;
            }
        }
        self.rewrite_units(stream, pos, end, &mut |start, plain| {
            for (i, page) in pages.iter().enumerate() {
                let at = pos + i as u64 * PAGE;
                let n = if i == last { len } else { PAGE_SIZE } as u64;
                let (a, b) = (at.max(start), (at + n).min(start + plain.len() as u64));
                if a < b {
                    plain[(a - start) as usize..(b - start) as usize]
                        .copy_from_slice(&page.bytes()[(a - at) as usize..(b - at) as usize]);
                }
            }
        })?;
        if end > stream.initialized {
            stream.initialized = end;
        }
        Ok(())
    }

    /// Writes the first `len` bytes of page `index` alone, through its
    /// pieces.
    fn write_one(&self, stream: &Stream, index: u64, page: &Page, len: usize) -> Result<()> {
        let geo = &self.0.fs.vol.geo;
        let whole = sectors_of(len, geo.sector_size);
        let device = self.0.fs.vol.cache.device();
        for (sector, offset, len) in pieces(geo, stream, index * PAGE, whole)?.iter() {
            device.write(sector, &[Segment { page, offset, len }])?;
        }
        Ok(())
    }

    /// Writes zeros over bytes `from..to` of `stream` on the disk: what
    /// lies between the data written and data about to be. `to` is a
    /// whole page.
    fn zero_disk(&self, stream: &Stream, from: u64, to: u64) -> Result<()> {
        let geo = &self.0.fs.vol.geo;
        let device = self.0.fs.vol.cache.device();
        let ss = u64::from(geo.sector_size);
        let mut page = CachePage::new()?;
        let mut at = from - from % ss;
        let in_hole = |at: u64| {
            let cs = u64::from(geo.cluster_size);
            stream.runs.map(at / cs).is_some_and(|(lcn, _)| lcn == HOLE)
        };
        if at != from && in_hole(at) {
            at += ss;
        } else if at != from {
            // The sector the data ends in keeps the data.
            let within = (from - at) as usize;
            let pieces = pieces(geo, stream, at, ss as usize)?;
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
        let cs = u64::from(geo.cluster_size);
        while at < to {
            // A sparse file's holes read as zeros already.
            let (lcn, run) = stream.runs.map(at / cs).ok_or(Error::Corrupt)?;
            let run_end = (at / cs + run) * cs;
            if lcn == HOLE {
                at = run_end.min(to);
                continue;
            }
            let n = (to - at).min(PAGE - at % PAGE).min(run_end - at) as usize;
            for (sector, offset, len) in pieces(geo, stream, at, n)?.iter() {
                device.write(sector, &[Segment { page: &page, offset, len }])?;
            }
            at += n as u64;
        }
        Ok(())
    }
}

impl Backing for FileBacking<'_> {
    fn persistent(&self) -> bool {
        true
    }

    fn read_page(&self, index: u64, page: &mut Page, len: usize) -> Result<()> {
        self.read_pages(index, page, len, &mut |_, _| false)
    }

    fn read_pages(
        &self,
        index: u64,
        page: &mut Page,
        len: usize,
        more: &mut dyn FnMut(u64, &[u8]) -> bool,
    ) -> Result<()> {
        let st = self.0.state.lock();
        match &st.data {
            Some(Data::Resident(bytes)) => {
                let pos = (index * PAGE) as usize;
                let n = bytes.len().saturating_sub(pos).min(len);
                page.bytes_mut()[..n].copy_from_slice(&bytes[pos.min(bytes.len())..][..n]);
                page.bytes_mut()[n..].fill(0);
                Ok(())
            }
            Some(Data::Clusters(stream)) if stream.flags & attr_flags::COMPRESSED != 0 => {
                self.compressed(stream, index, page, len, more)
            }
            Some(Data::Clusters(stream)) => self.plain(stream, index, page, len),
            Some(Data::Unreadable(err)) => Err(*err),
            None => Err(Error::IsDirectory),
        }
    }

    fn read_run(
        &self,
        index: u64,
        pages: &mut [CachePage],
        size: u64,
        more: &mut dyn FnMut(u64, &[u8]) -> bool,
    ) -> Result<usize> {
        let single = |pages: &mut [CachePage], more: &mut dyn FnMut(u64, &[u8]) -> bool| {
            let page = pages.first_mut().ok_or(Error::Io)?;
            self.read_pages(index, page, page_len(index, size), more)?;
            Ok(1)
        };
        let vol = &self.0.fs.vol;
        let cs = u64::from(vol.geo.cluster_size);
        let pos = index * PAGE;
        // Where the run is, read under the node's lock and let go of for
        // the read: the clusters a page of the cache is in do not move
        // while the cache holds it.
        let found = {
            let st = self.0.state.lock();
            match &st.data {
                Some(Data::Clusters(stream)) if stream.flags & attr_flags::COMPRESSED == 0 => {
                    stream.runs.map(pos / cs).map(|(lcn, n)| (lcn, n, stream.initialized))
                }
                _ => None,
            }
        };
        let Some((lcn, n, initialized)) = found else { return single(pages, more) };
        // Whole pages in a row on the disk, all written, all in the file.
        let in_row = (n * cs - pos % cs) / PAGE;
        let written = initialized.min(size).saturating_sub(pos) / PAGE;
        let device = vol.cache.device();
        let max = device.info().max_segments.clamp(1, READ_RUN) as u64;
        let count = in_row.min(written).min(pages.len() as u64).min(max) as usize;
        if lcn == HOLE || count < 2 {
            return single(pages, more);
        }
        {
            let mut segments = mem::vec_with_capacity(count)?;
            for page in &mut pages[..count] {
                let _ =
                    segments.push_within_capacity(SegmentMut { page, offset: 0, len: PAGE_SIZE });
            }
            let sector = (vol.geo.cluster_offset(lcn) + pos % cs) / SECTOR;
            device.read(sector, &mut segments)?;
        }
        for page in &mut pages[..count] {
            radix_hw::device_wrote(page.bytes_mut());
        }
        Ok(count)
    }

    fn run_pages(&self) -> usize {
        READ_RUN
    }

    fn write_page(&self, index: u64, page: &Page, len: usize) -> Result<()> {
        self.write_pages(index, &[page], len)
    }

    fn write_pages(&self, index: u64, pages: &[&Page], len: usize) -> Result<()> {
        let node = self.0;
        let Some(last) = pages.len().checked_sub(1) else { return Ok(()) };
        let pos = index * PAGE;
        let end = pos + last as u64 * PAGE + len as u64;
        let mut st = node.state.lock();
        if let Some(err) = st.fixed {
            return Err(err);
        }
        let stream = match &mut st.data {
            Some(Data::Resident(bytes)) => {
                // In the record: the bytes go there when it is stored.
                for (i, page) in pages.iter().enumerate() {
                    let at = (pos + i as u64 * PAGE) as usize;
                    let n = if i == last { len } else { PAGE_SIZE };
                    let stop = (at + n).min(bytes.len());
                    if at < stop {
                        bytes[at..stop].copy_from_slice(&page.bytes()[..stop - at]);
                    }
                }
                st.behind();
                return Ok(());
            }
            Some(Data::Clusters(stream)) => stream,
            _ => return Err(Error::Corrupt),
        };
        if stream.compressed() {
            self.write_units(stream, pos, pages, len)?;
            st.behind();
            return Ok(());
        }
        // What lies between the data written and these pages reads as
        // zeros: it must be zeros on the disk before the data written
        // reaches past it.
        if pos > stream.initialized {
            self.zero_disk(stream, stream.initialized, pos)?;
        }
        // A sparse file's holes the pages fall in get clusters first.
        if stream.packed() {
            node.fill_holes(stream, pos, end)?;
        }
        let geo = &node.fs.vol.geo;
        let cs = u64::from(geo.cluster_size);
        let device = node.fs.vol.cache.device();
        let max = device.info().max_segments.clamp(1, MAX_SEGMENTS);
        let mut k = 0;
        while k < pages.len() {
            let at = pos + k as u64 * PAGE;
            // Whole pages in a row on the disk go in one request; the
            // written data does not limit them.
            let (lcn, n) = stream.runs.map(at / cs).ok_or(Error::Corrupt)?;
            let room = n * cs - at % cs;
            let run = (room / PAGE) as usize;
            let count = run.min(pages.len() - k).min(max);
            if count == 0 || lcn == HOLE {
                let n = if k == last { len } else { PAGE_SIZE };
                self.write_one(stream, index + k as u64, pages[k], n)?;
                k += 1;
                continue;
            }
            let mut segments = mem::vec_with_capacity(count)?;
            for (i, &page) in pages.iter().enumerate().skip(k).take(count) {
                let len = if i == last { sectors_of(len, geo.sector_size) } else { PAGE_SIZE };
                let _ = segments.push_within_capacity(Segment { page, offset: 0, len });
            }
            device.write((geo.cluster_offset(lcn) + at % cs) / SECTOR, &segments)?;
            k += count;
        }
        if end > stream.initialized {
            stream.initialized = end;
        }
        st.behind();
        Ok(())
    }

    fn reserve(&self, size: u64) -> Result<()> {
        let node = self.0;
        let geo = &node.fs.vol.geo;
        let cs = u64::from(geo.cluster_size);
        // The file grows, which its record must say. It is pinned before
        // it takes clusters, so that a sync meanwhile keeps the volume
        // marked dirty until they do.
        if !node.pin() {
            return Err(Error::NoMemory);
        }
        let mut st = node.state.lock();
        if let Some(err) = st.fixed {
            return Err(err);
        }
        node.fs.changing()?;
        if let Some(Data::Resident(bytes)) = &st.data {
            let len = bytes.len();
            if size <= len as u64 {
                return Ok(());
            }
            // A small file stays in its record while the record has room
            // for it, as Windows keeps it.
            if size < u64::from(geo.record_size) && node.resident_fits(size as usize)? {
                let Some(Data::Resident(bytes)) = &mut st.data else { return Err(Error::Corrupt) };
                bytes.try_reserve_exact(size as usize - len).map_err(|_| Error::NoMemory)?;
                for _ in len..size as usize {
                    let _ = bytes.push_within_capacity(0);
                }
                st.behind();
                return Ok(());
            }
            return node.to_clusters(&mut st, size);
        }
        let Some(Data::Clusters(stream)) = &mut st.data else { return Err(Error::Corrupt) };
        let have = stream.runs.clusters();
        let need = size.div_ceil(cs).next_multiple_of(stream.grain());
        if need <= have {
            stream.size = stream.size.max(size);
            return Ok(());
        }
        // Compressed and sparse data grows by a hole, which takes clusters
        // as it is written.
        if stream.packed() {
            stream.runs.append(HOLE, need - have)?;
            if let Err(err) = node.runs_fit(stream) {
                let _ = stream.runs.truncate(have);
                return Err(err);
            }
            stream.size = stream.size.max(size);
            st.behind();
            return Ok(());
        }
        let near = stream.runs.as_slice().last().map(|r| r.lcn + r.len);
        let got = node.fs.take_clusters(need - have, near)?;
        let fits = (|| {
            for &(lcn, n) in &got {
                stream.runs.append(lcn, n)?;
            }
            // Its record must have room for where they are.
            node.runs_fit(stream)
        })();
        if let Err(err) = fits {
            let _ = stream.runs.truncate(have);
            for &(lcn, n) in &got {
                let _ = node.fs.give_clusters(lcn, n, false);
            }
            return Err(err);
        }
        stream.size = stream.size.max(size);
        st.behind();
        Ok(())
    }

    fn zero(&self, from: u64, to: u64) -> Result<()> {
        let mut st = self.0.state.lock();
        match &mut st.data {
            Some(Data::Resident(bytes)) => {
                let len = bytes.len();
                bytes[(from as usize).min(len)..(to as usize).min(len)].fill(0);
                Ok(())
            }
            // What lies past the data written reads as zeros already.
            Some(Data::Clusters(stream)) if from < stream.initialized && stream.compressed() => {
                let to = to.min(stream.initialized);
                self.rewrite_units(stream, from, to, &mut |start, plain| {
                    let a = from.saturating_sub(start).min(plain.len() as u64) as usize;
                    let b = to.saturating_sub(start).min(plain.len() as u64) as usize;
                    plain[a..b].fill(0);
                })
            }
            Some(Data::Clusters(stream)) if from < stream.initialized => {
                let to = to.min(stream.initialized.next_multiple_of(PAGE));
                self.zero_disk(stream, from, to)
            }
            _ => Ok(()),
        }
    }

    fn truncate(&self, size: u64) -> Result<()> {
        let node = self.0;
        let cs = u64::from(node.fs.vol.geo.cluster_size);
        if !node.pin() {
            return Err(Error::NoMemory);
        }
        let mut st = node.state.lock();
        if let Some(err) = st.fixed {
            return Err(err);
        }
        node.fs.changing()?;
        match &mut st.data {
            Some(Data::Resident(bytes)) => bytes.truncate(size as usize),
            Some(Data::Clusters(stream)) => {
                // Compressed data keeps the unit its end is in whole.
                let keep = size.div_ceil(cs).next_multiple_of(stream.grain());
                let freed = stream.runs.truncate(keep)?;
                for run in &freed {
                    let _ = node.fs.give_clusters(run.lcn, run.len, false);
                }
                stream.initialized = stream.initialized.min(size);
                stream.size = size;
            }
            _ => return Err(Error::Corrupt),
        }
        st.behind();
        Ok(())
    }

    /// As on Windows, a file has fewer than 2^32 clusters: a sparse or
    /// compressed one may be larger than its volume, and a plain one
    /// runs out of room first.
    fn max_size(&self) -> u64 {
        u64::from(u32::MAX) * u64::from(self.0.fs.vol.geo.cluster_size)
    }
}
