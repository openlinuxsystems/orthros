// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! A file, directory or link of a mounted Btrfs filesystem: an inode of
//! one of its subvolumes.
//!
//! A directory finds a name by its hash (`DIR_ITEM`) and lists its
//! entries in the order they were made (`DIR_INDEX`). A file's data goes
//! through its page cache, whose backing finds each page's extent and
//! reads it straight from the disk into the page, checking it against the
//! checksum tree and reading the next copy when it does not match, as
//! Linux does.
//!
//! On a writable mount (ADR 0130) a change goes through the running
//! transaction ([`crate::files`]). A file's data waits in its page cache
//! until the next sync, when each run of pages next to each other goes to
//! a new extent; its inode item follows its size and times then.

use alloc::sync::Weak;
use core::any::Any;
use core::sync::atomic::{AtomicBool, Ordering};

use radix_block::mem::CachePage;
use radix_block::{PAGE_SIZE, Page, SegmentMut};
use radix_sync::{Mutex, Once};
use radix_vfs::cache::{Backing, FileCache, page_len};
use radix_vfs::mem::{self, Alloc, KArc, KVec};
use radix_vfs::path::check_name;
use radix_vfs::{Error, In, Kind, ListFn, Node, NodeRef, Out, Result, SocketBinding, Stat, now};

use crate::dir::Entries;
use crate::extent::{self, Compression, Extent, Regular};
use crate::files::{Files, at, compression, file_type, put_time, put_u32, put_u64};
use crate::format::{Key, item, name_hash, tree};
use crate::fs::{Inner, Txn};
use crate::inode::{self, Inode};
use crate::pack::{CHUNK, Packer};
use crate::tree::{Cursor, Root};
use crate::unpack::{self, Workspace};

const PAGE: u64 = PAGE_SIZE as u64;

/// The modes of the nodes a writable mount makes: their type and
/// permissions, as Linux makes them with the usual umask.
mod mode {
    pub const FILE: u32 = 0o100_644;
    pub const DIR: u32 = 0o040_755;
    pub const LINK: u32 = 0o120_777;
}

/// The longest link Btrfs keeps: inline, in less than a sector.
const LINK_MAX: usize = PAGE_SIZE - 1;

/// How many entries a listing on a writable mount reads at a time: it lets
/// go of the transaction to hand them over.
const LIST_BATCH: usize = 64;

/// A node of a mounted Btrfs filesystem.
pub struct BtrfsNode {
    pub(crate) fs: KArc<Inner>,
    /// Its subvolume: the number of its tree, and the tree's root on a
    /// read-only mount.
    pub(crate) subvol: u64,
    pub(crate) tree: Root,
    /// Its inode number in the subvolume.
    pub(crate) ino: u64,
    /// Its inode item, as the running transaction has it.
    inode: Mutex<Inode>,
    kind: Kind,
    /// A file's page cache.
    cache: Option<FileCache>,
    /// Whether it has no names left: its items go when it does.
    removed: AtomicBool,
    /// Whether its items went with its last name, as no one had it open:
    /// none are left for it to free.
    freed: AtomicBool,
    /// Whether the filesystem keeps it for the next sync.
    pinned: AtomicBool,
    /// Itself, to be kept.
    me: Once<Weak<BtrfsNode, Alloc>>,
}

/// A number for the inode `ino` of subvolume `subvol`, unique among the
/// filesystem's nodes: the inode number in the top subvolume, as Linux's
/// `st_ino` is, and with the subvolume's number from bit 40 in the others
/// (Linux tells them apart by `st_dev` instead).
pub fn id_of(subvol: u64, ino: u64) -> u64 {
    if subvol == tree::FS { ino } else { ino ^ (subvol << 40) }
}

impl BtrfsNode {
    /// The node of inode `ino` of subvolume `subvol`, whose tree is
    /// `tree`, read from its inode item. On a writable mount the tree is
    /// read as the running transaction has it, and the caller holds it.
    pub(crate) fn load(fs: KArc<Inner>, subvol: u64, tree: Root, ino: u64) -> Result<BtrfsNode> {
        let key = Key::new(ino, item::INODE_ITEM, 0);
        let now = if fs.writable() { fs.volume.tree(subvol)?.ok_or(Error::Corrupt)? } else { tree };
        let cursor = Cursor::seek(&fs.volume, &now, &key)?;
        let inode = match cursor.get() {
            Some((found, data)) if found == key => Inode::read(data)?,
            _ => return Err(Error::Corrupt),
        };
        drop(cursor);
        let kind = inode.kind()?;
        let cache = (kind == Kind::File).then(|| FileCache::new(inode.size, inode.mtime));
        Ok(BtrfsNode {
            fs,
            subvol,
            tree,
            ino,
            inode: Mutex::new(inode),
            kind,
            cache,
            removed: AtomicBool::new(false),
            freed: AtomicBool::new(false),
            pinned: AtomicBool::new(false),
            me: Once::new(),
        })
    }

    /// Notes where it lives, once it is on the heap.
    pub(crate) fn placed(node: &KArc<BtrfsNode>) {
        let _ = node.me.set(KArc::downgrade(node));
    }

    fn is_dir(&self) -> Result<()> {
        if self.kind == Kind::Directory { Ok(()) } else { Err(Error::NotDirectory) }
    }

    /// The inode item, as the running transaction has it.
    pub fn inode(&self) -> Inode {
        *self.inode.lock()
    }

    /// Its subvolume's tree, as the running transaction has it; the caller
    /// holds the transaction on a writable mount.
    fn tree(&self) -> Result<Root> {
        if self.fs.writable() {
            self.fs.volume.tree(self.subvol)?.ok_or(Error::Corrupt)
        } else {
            Ok(self.tree)
        }
    }

    fn removed(&self) -> bool {
        self.removed.load(Ordering::Acquire)
    }

    /// Reads its inode item again, after a change.
    fn reload(&self, files: &Files<'_>) -> Result<()> {
        if files.subvol == self.subvol
            && let Some(inode) = files.changed(self.ino)
        {
            *self.inode.lock() = inode;
            return Ok(());
        }
        let key = Key::new(self.ino, item::INODE_ITEM, 0);
        let data = files.get(self.subvol, &key)?.ok_or(Error::Corrupt)?;
        *self.inode.lock() = Inode::read(&data)?;
        Ok(())
    }

    /// Keeps it until the next sync; false if there was no memory to.
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

    /// The filesystem no longer keeps it for the next sync.
    pub(crate) fn unpinned(&self) {
        self.pinned.store(false, Ordering::Release);
    }

    /// Notes that its data or size changed in memory: the next sync writes
    /// it, or it is written now if there is no memory to keep it.
    fn changed(&self) -> Result<()> {
        if self.pin() { Ok(()) } else { self.flush() }
    }

    /// How many of its pages wait to be written.
    pub(crate) fn dirty_pages(&self) -> usize {
        self.cache.as_ref().map_or(0, FileCache::dirty_pages)
    }

    /// Writes its data, then its size and times to its inode item.
    pub(crate) fn flush(&self) -> Result<()> {
        let Some(cache) = &self.cache else { return Ok(()) };
        if self.removed() || !self.fs.writable() {
            return Ok(());
        }
        cache.sync(&FileBacking(self))?;
        let (size, modified, _) = cache.status();
        let inode = self.inode();
        if inode.size == size && inode.mtime == modified {
            return Ok(());
        }
        self.fs.change(|txn| {
            let mut files = txn.files(&self.fs.volume, self.subvol);
            files.whole(|f| {
                f.change_inode(self.ino, |d| {
                    put_u64(d, at::SIZE, size);
                    put_time(d, at::MTIME, modified);
                    put_time(d, at::CTIME, modified.max(inode.ctime));
                })
            })?;
            self.reload(&files)
        })
    }

    /// What the entry `location` of this directory names: an inode here,
    /// or the top directory of another subvolume.
    fn open_entry(&self, location: &Key) -> Result<NodeRef> {
        let node = match location.kind {
            item::INODE_ITEM => {
                Inner::node_for(&self.fs, self.subvol, self.tree, location.objectid)?
            }
            item::ROOT_ITEM => {
                let (root, dir) =
                    self.fs.volume.subvolume(location.objectid)?.ok_or(Error::NotFound)?;
                Inner::node_for(&self.fs, location.objectid, root, dir)?
            }
            _ => return Err(Error::Corrupt),
        };
        Ok(node)
    }

    /// What the entry `name` of this directory names. The caller holds the
    /// transaction on a writable mount.
    fn entry(&self, name: &str) -> Result<Option<Key>> {
        let volume = &self.fs.volume;
        let key = Key::new(self.ino, item::DIR_ITEM, name_hash(name.as_bytes()));
        let cursor = Cursor::seek(volume, &self.tree()?, &key)?;
        let Some((found, data)) = cursor.get() else { return Ok(None) };
        if found != key {
            return Ok(None);
        }
        // Names whose hashes are the same share the item.
        for entry in Entries::of(data) {
            let entry = entry?;
            if entry.name == name.as_bytes() {
                return Ok(Some(entry.location));
            }
        }
        Ok(None)
    }

    /// Reads the file's page `index`, `len` bytes of it, into `page`;
    /// offers `more` the extent's other pages when it uncompresses one.
    fn read_page(
        &self,
        index: u64,
        page: &mut Page,
        len: usize,
        more: &mut dyn FnMut(u64, &[u8]) -> bool,
    ) -> Result<()> {
        let _reading = self.fs.reading();
        self.read_page_in(index, page, len, more)
    }

    /// [`BtrfsNode::read_page`], with the transaction held on a writable
    /// mount.
    fn read_page_in(
        &self,
        index: u64,
        page: &mut Page,
        len: usize,
        more: &mut dyn FnMut(u64, &[u8]) -> bool,
    ) -> Result<()> {
        let pos = index * PAGE;
        let volume = &self.fs.volume;
        let key = Key::new(self.ino, item::EXTENT_DATA, pos);
        // The extent that starts at `pos` or before it; when it ends
        // before `pos`, or there is none, the page is in a hole.
        let cursor = Cursor::seek_last(volume, &self.tree()?, &key)?;
        let found = match &cursor {
            Some(cursor) => match cursor.get() {
                Some((at, data)) if at.is(self.ino, item::EXTENT_DATA) => {
                    let extent = Extent::read(data)?;
                    let into = pos - at.offset;
                    (into < extent.len()).then_some((into, extent))
                }
                _ => None,
            },
            None => None,
        };
        let bytes = page.bytes_mut();
        match found {
            None
            | Some((
                _,
                Extent::Prealloc { .. } | Extent::Regular(Regular { disk_bytenr: 0, .. }),
            )) => bytes.fill(0),
            Some((into, Extent::Inline { compression, data, ram_bytes })) => {
                // An inline extent is the file's start, and its only extent.
                if compression == Compression::None {
                    copy_from(data, into, bytes);
                } else {
                    let len = usize::try_from(ram_bytes).map_err(|_| Error::Corrupt)?;
                    self.fs.with_workspace(|workspace| {
                        let data = workspace.unpack(compression, data, len, PAGE_SIZE)?;
                        copy_from(data, into, bytes);
                        Ok(())
                    })?;
                }
            }
            Some((into, Extent::Regular(extent))) => {
                drop(cursor);
                if extent.compression == Compression::None {
                    let segment = SegmentMut { page: &mut *page, offset: 0, len: PAGE_SIZE };
                    self.read_plain(&extent, into, &mut [segment])?;
                } else {
                    self.read_compressed(&extent, pos - into, index, page, more)?;
                }
            }
        }
        // The disk's bytes past the end of the file are not the file's.
        page.bytes_mut()[len..].fill(0);
        Ok(())
    }

    /// Reads the file's pages from `index` on into `pages`, in the file of
    /// `size` bytes: as many as the extent that holds page `index` has
    /// from there, up to one for each of `pages`, each run of sectors in
    /// one request to the disk; or, from a compressed extent, as many as
    /// it and the compressed extents after it in the file have, each
    /// read whole, half of them uncompressed on another CPU
    /// ([`BtrfsNode::read_packed`]). Returns how many; pages of the first
    /// extent outside them go to `more`.
    fn read_run(
        &self,
        index: u64,
        pages: &mut [CachePage],
        size: u64,
        more: &mut dyn FnMut(u64, &[u8]) -> bool,
    ) -> Result<usize> {
        let _reading = self.fs.reading();
        let pos = index * PAGE;
        let key = Key::new(self.ino, item::EXTENT_DATA, pos);
        let cursor = Cursor::seek_last(&self.fs.volume, &self.tree()?, &key)?;
        let found = match cursor.as_ref().and_then(Cursor::get) {
            Some((at, data)) if at.is(self.ino, item::EXTENT_DATA) => match Extent::read(data)? {
                Extent::Regular(extent)
                    if extent.disk_bytenr != 0 && pos - at.offset < extent.num_bytes =>
                {
                    Some((at.offset, extent))
                }
                _ => None,
            },
            _ => None,
        };
        let Some((start, extent)) = found else {
            drop(cursor);
            // A hole, or an inline or preallocated extent: a page.
            let page = pages.first_mut().ok_or(Error::Io)?;
            self.read_page_in(index, page, page_len(index, size), more)?;
            return Ok(1);
        };
        let read = if extent.compression == Compression::None {
            drop(cursor);
            let mut segments = mem::vec_with_capacity(pages.len())?;
            for page in pages.iter_mut() {
                mem::push(&mut segments, SegmentMut { page, offset: 0, len: PAGE_SIZE })?;
            }
            self.read_plain(&extent, pos - start, &mut segments)?
        } else {
            let end = (index + pages.len() as u64) * PAGE;
            let extents = packed_run(self.ino, cursor, (start, extent), end)?;
            self.read_packed(&extents, index, pages, more)?
        };
        for (at, page) in (index..).zip(&mut pages[..read]) {
            page.bytes_mut()[page_len(at, size)..].fill(0);
        }
        Ok(read)
    }

    /// Reads the pages `into` bytes into the uncompressed `extent` on into
    /// those of `segments`: as many as it has room for and the extent
    /// holds, at least one, up to one that cannot be read. Returns how
    /// many.
    fn read_plain(
        &self,
        extent: &Regular,
        into: u64,
        segments: &mut [SegmentMut<'_>],
    ) -> Result<usize> {
        // Within the extent on the disk, and a whole sector of it: damage
        // may say otherwise.
        let Regular { disk_bytenr, disk_num_bytes, offset, num_bytes, .. } = *extent;
        if offset.checked_add(num_bytes).is_none_or(|end| end > disk_num_bytes) {
            return Err(Error::Corrupt);
        }
        // `into` is below `num_bytes`, so this cannot overflow.
        let logical = disk_bytenr
            .checked_add(offset + into)
            .filter(|at| at.is_multiple_of(PAGE))
            .ok_or(Error::Corrupt)?;
        let left = usize::try_from((num_bytes - into) / PAGE).unwrap_or(usize::MAX);
        let n = segments.len().min(left.max(1));
        self.read_sectors(logical, &mut segments[..n])
    }

    /// Reads the file's pages from `index` on into `pages` from the
    /// compressed `extents` ([`packed_run`]), the first holding page
    /// `index`: the first half of them here, the rest on another CPU when
    /// one is free (`radix_vfs::join`), each half with a workspace of its
    /// own. Pages of the first extent outside `pages` go to `more`.
    /// Returns how many of `pages` from the first on are filled.
    fn read_packed(
        &self,
        extents: &[(u64, Regular)],
        index: u64,
        pages: &mut [CachePage],
        more: &mut dyn FnMut(u64, &[u8]) -> bool,
    ) -> Result<usize> {
        let mine = extents.len().div_ceil(2);
        let at = match extents.get(mine) {
            Some(&(start, _)) => usize::try_from((start / PAGE).saturating_sub(index))
                .ok()
                .filter(|&at| at > 0 && at < pages.len()),
            None => None,
        };
        self.fs.with_workspaces(at.is_some(), |own, other| {
            let (Some(at), Some(other)) = (at, other) else {
                return self.unpack(own, extents, index, Some(index), pages, more);
            };
            let (mine, theirs) = extents.split_at(mine);
            let (head, tail) = pages.split_at_mut(at);
            let (mut first, mut second) = (Ok(0), Ok(0));
            let from = index + at as u64;
            radix_vfs::join(
                || first = self.unpack(own, mine, index, Some(index), head, more),
                || second = self.unpack(other, theirs, from, None, tail, &mut |_, _| false),
            );
            let first = first?;
            // The second half's pages count when the first's are all
            // there; its failure leaves the run at the first's end.
            Ok(if first == at { at + second.unwrap_or(0) } else { first })
        })
    }

    /// Reads the compressed `extents`, which follow each other in the file,
    /// with `workspace`: those that follow each other on the disk too, as
    /// many as its pages hold, in one request, then each uncompressed. The
    /// whole pages of each in the file, and page `asked` whatever its
    /// data, go into `pages` (the file's pages from `base` on) or, when
    /// not among them, to `more`, until it returns false. Returns how many
    /// of `pages` from the first on are filled, at least the one asked
    /// for: an extent that cannot be read or uncompressed fails the read
    /// when none is, and ends it otherwise. A frame of its own, not one in
    /// each of the join's halves.
    #[inline(never)]
    fn unpack(
        &self,
        workspace: &mut Workspace,
        extents: &[(u64, Regular)],
        base: u64,
        asked: Option<u64>,
        pages: &mut [CachePage],
        more: &mut dyn FnMut(u64, &[u8]) -> bool,
    ) -> Result<usize> {
        let mut filled = 0;
        let mut offer = true;
        let stop = |filled: usize, e: Error| if filled > 0 { Ok(filled) } else { Err(e) };
        let mut next = 0;
        while let Some(&(_, first)) = extents.get(next) {
            // The extents from `next` that follow each other on the disk.
            let mut sectors = packed_sizes(&first)?.0;
            let mut count = 1;
            while let Some((_, extent)) = extents.get(next + count) {
                let (_, last) = extents[next + count - 1];
                let follows =
                    last.disk_bytenr.checked_add(last.disk_num_bytes) == Some(extent.disk_bytenr);
                let n = packed_sizes(extent)?.0;
                if !follows || sectors + n > workspace.pages.len() {
                    break;
                }
                sectors += n;
                count += 1;
            }
            let group = &extents[next..next + count];
            next += count;
            let mut segments = match mem::vec_with_capacity(sectors) {
                Ok(segments) => segments,
                Err(e) => return stop(filled, e.into()),
            };
            for page in workspace.pages.iter_mut().take(sectors) {
                let _ =
                    segments.push_within_capacity(SegmentMut { page, offset: 0, len: PAGE_SIZE });
            }
            let read = match self.read_sectors(first.disk_bytenr, &mut segments) {
                Ok(read) => read,
                Err(e) => return stop(filled, e),
            };
            drop(segments);
            let mut sector = 0;
            for &(start, extent) in group {
                let (n, len) = packed_sizes(&extent)?;
                if sector + n > read {
                    return stop(filled, Error::Io);
                }
                for (at, page) in workspace.pages[sector..sector + n].iter().enumerate() {
                    workspace.input[at * PAGE_SIZE..(at + 1) * PAGE_SIZE]
                        .copy_from_slice(page.bytes());
                }
                sector += n;
                let compressed = n * PAGE_SIZE;
                let data =
                    match workspace.unpack_input(extent.compression, compressed, len, PAGE_SIZE) {
                        Ok(data) => data,
                        Err(e) => return stop(filled, e),
                    };
                let end = start.checked_add(extent.num_bytes).ok_or(Error::Corrupt)?;
                let mut at = start.next_multiple_of(PAGE);
                while at < end {
                    let page = at / PAGE;
                    let whole = at.checked_add(PAGE).is_some_and(|after| after <= end);
                    let from = extent.offset.saturating_add(at - start);
                    let from =
                        usize::try_from(from).map_or(data.len(), |from| from.min(data.len()));
                    // The page asked for whatever it holds; others whole,
                    // and within the extent's data: the rest are read when
                    // asked for.
                    let given = (whole && from < data.len()) || asked == Some(page);
                    match page.checked_sub(base).and_then(|k| usize::try_from(k).ok()) {
                        Some(k) if k < pages.len() => {
                            if !given {
                                break;
                            }
                            if k == filled {
                                copy_from(data, from as u64, pages[k].bytes_mut());
                                filled += 1;
                            }
                        }
                        // The cache takes no more when memory is short.
                        _ if given && offer => {
                            offer = more(page, &data[from..(from + PAGE_SIZE).min(data.len())]);
                        }
                        _ => {}
                    }
                    let Some(after) = at.checked_add(PAGE) else { break };
                    at = after;
                }
            }
        }
        if filled == 0 && asked.is_some() {
            return Err(Error::Corrupt);
        }
        Ok(filled)
    }

    /// Reads the compressed `extent`, which starts at `start` in the file
    /// and holds its page `index`: the whole extent is read in one
    /// request, checked and uncompressed, page `index` copied into `page`,
    /// and each other whole page of the file it holds given to `put`, in
    /// order, until `put` returns false.
    fn read_compressed(
        &self,
        extent: &Regular,
        start: u64,
        index: u64,
        page: &mut Page,
        put: &mut dyn FnMut(u64, &[u8]) -> bool,
    ) -> Result<()> {
        let Regular { disk_bytenr, offset, num_bytes, compression, .. } = *extent;
        let (sectors, len) = packed_sizes(extent)?;
        let compressed = sectors * PAGE_SIZE;
        let end = start.checked_add(num_bytes).ok_or(Error::Corrupt)?;
        let into = (index * PAGE).checked_sub(start).ok_or(Error::Corrupt)?;
        self.fs.with_workspace(|workspace| {
            let mut segments = mem::vec_with_capacity(sectors)?;
            for page in workspace.pages.iter_mut().take(sectors) {
                mem::push(&mut segments, SegmentMut { page, offset: 0, len: PAGE_SIZE })?;
            }
            if self.read_sectors(disk_bytenr, &mut segments)? < sectors {
                return Err(Error::Io);
            }
            drop(segments);
            for (at, page) in workspace.pages.iter().take(sectors).enumerate() {
                workspace.input[at * PAGE_SIZE..(at + 1) * PAGE_SIZE].copy_from_slice(page.bytes());
            }
            let data = workspace.unpack_input(compression, compressed, len, PAGE_SIZE)?;
            copy_from(data, offset.saturating_add(into), page.bytes_mut());
            // The extent's other whole pages in the file, for its cache,
            // as far as its data goes.
            let mut at = start.next_multiple_of(PAGE);
            while at.checked_add(PAGE).is_some_and(|next| next <= end) {
                let Some(from) = offset
                    .checked_add(at - start)
                    .and_then(|from| usize::try_from(from).ok())
                    .filter(|&from| from < len)
                else {
                    break;
                };
                if at / PAGE != index && !put(at / PAGE, &data[from..(from + PAGE_SIZE).min(len)]) {
                    break;
                }
                at += PAGE;
            }
            Ok(())
        })
    }

    /// Reads the data sectors from logical address `logical` on into the
    /// pages of `segments`, one each, in as few requests to the disk as
    /// it takes, and checks them against their checksums. A sector whose checksum fails,
    /// or each of them when the request does, is read again on its own,
    /// from the first copy that holds it whole ([`BtrfsNode::read_data`]).
    /// Returns how many sectors from the first on it read: those up to one
    /// that no copy gives, which fails the read if it is the first. The
    /// pages from there on hold no data.
    fn read_sectors(&self, logical: u64, segments: &mut [SegmentMut<'_>]) -> Result<usize> {
        let volume = &self.fs.volume;
        let bytes = segments.len() as u64 * PAGE;
        let place = volume.place(logical)?;
        let read = match place.copies().first() {
            Some(&physical)
                if place.len >= bytes
                    && physical
                        .checked_add(bytes)
                        .is_some_and(|end| end <= volume.device_bytes()) =>
            {
                let device = volume.device();
                let most = device.info().max_segments.max(1);
                (physical..).step_by(most * PAGE_SIZE).zip(segments.chunks_mut(most)).all(
                    |(physical, part)| device.read(physical / radix_block::SECTOR, part).is_ok(),
                )
            }
            _ => false,
        };
        let again =
            |at: usize, page: &mut Page| match self.read_data(logical + at as u64 * PAGE, page) {
                Ok(()) => Ok(None),
                Err(e) if at == 0 => Err(e),
                Err(_) => Ok(Some(at)),
            };
        if !read {
            for (at, segment) in segments.iter_mut().enumerate() {
                if let Some(good) = again(at, segment.page)? {
                    return Ok(good);
                }
            }
            return Ok(segments.len());
        }
        for segment in segments.iter_mut() {
            radix_hw::device_wrote(segment.page.bytes_mut());
        }
        if !self.inode().has_sums() {
            return Ok(segments.len());
        }
        // Data without its checksum fails, as on Linux.
        let csums = self.fs.csums()?.ok_or(Error::Io)?;
        let mut bad = mem::vec();
        let mut noted = Ok(());
        extent::check_sums(volume, &csums, logical, segments, &mut |at| {
            if noted.is_ok() {
                noted = mem::push(&mut bad, at);
            }
        })?;
        noted?;
        for at in bad {
            if let Some(good) = again(at, segments[at].page)? {
                return Ok(good);
            }
        }
        Ok(segments.len())
    }

    /// Reads the data sector at logical address `logical` into `page`,
    /// from the first copy whose checksum matches.
    fn read_data(&self, logical: u64, page: &mut Page) -> Result<()> {
        let volume = &self.fs.volume;
        let sum = if self.inode().has_sums() {
            let csums = self.fs.csums()?.ok_or(Error::Io)?;
            // Data without its checksum fails, as on Linux.
            Some(extent::data_sum(volume, &csums, logical)?.ok_or(Error::Io)?)
        } else {
            None
        };
        let place = volume.place(logical)?;
        if place.len < PAGE {
            return Err(Error::Corrupt);
        }
        let device = volume.device();
        let csum = volume.superblock().csum_type;
        let mut failed = Error::Io;
        for &physical in place.copies() {
            if physical.checked_add(PAGE).is_none_or(|end| end > volume.device_bytes()) {
                failed = Error::Corrupt;
                continue;
            }
            let read = device.read(
                physical / radix_block::SECTOR,
                &mut [SegmentMut { page: &mut *page, offset: 0, len: PAGE_SIZE }],
            );
            if let Err(e) = read {
                failed = e.into();
                continue;
            }
            radix_hw::device_wrote(page.bytes_mut());
            match &sum {
                Some(sum) if !csum.matches(page.bytes(), sum) => failed = Error::Io,
                _ => return Ok(()),
            }
        }
        Err(failed)
    }
}

/// The writes of a writable mount.
impl BtrfsNode {
    /// Rewrites the file's inline extent, if it has one, as a regular one
    /// of one page: a file whose data changes past its start keeps no
    /// inline extent, as on Linux.
    fn convert_inline(&self, files: &mut Files<'_>) -> Result<()> {
        if !files.has_inline(self.ino)? {
            return Ok(());
        }
        let mut page = CachePage::new()?;
        let len = usize::try_from(self.inode().size).map_or(PAGE_SIZE, |size| size.min(PAGE_SIZE));
        self.read_page_in(0, &mut page, len, &mut |_, _| false)?;
        let sums = self.inode().has_sums();
        files.write_data(self.ino, 0, &[&*page], sums, 0)
    }

    /// Writes `pages`, the file's pages from `index` on (`len` bytes of
    /// the last one in the file), to new extents.
    fn write_run(&self, index: u64, pages: &[&Page], len: usize) -> Result<()> {
        let pos = index * PAGE;
        let end = pos + (pages.len() as u64 - 1) * PAGE + len as u64;
        let inode = self.inode();
        let sums = inode.has_sums();
        // As on Linux: never without checksums, never when the file says
        // not to, and always when it says to.
        let flags = inode.flags;
        if sums
            && flags & inode::flags::NOCOMPRESS == 0
            && (self.fs.compress() || flags & inode::flags::COMPRESS != 0)
        {
            return self.write_packed(index, pages, end);
        }
        self.fs.change(|txn| {
            let mut files = txn.files(&self.fs.volume, self.subvol);
            if pos != 0 {
                self.convert_inline(&mut files)?;
            }
            write_split(&mut files, self.ino, pos, pages, sums, end)?;
            self.reload(&files)
        })
    }

    /// [`BtrfsNode::write_run`] with the data compressed, a chunk of up to
    /// 128 KiB to an extent (ADR 0134): two chunks at a time, the second on
    /// another CPU when one is free, compressed before the transaction is
    /// taken. A chunk that would not save a sector is written as it is.
    /// The file ends at `end`.
    fn write_packed(&self, index: u64, pages: &[&Page], end: u64) -> Result<()> {
        // The bytes of the chunk of `n` pages from page `at` that are the
        // file's.
        let bytes = |at: u64, n: usize| {
            usize::try_from(end.saturating_sub(at * PAGE))
                .map_or(n * PAGE_SIZE, |b| b.min(n * PAGE_SIZE))
        };
        let mut done = 0;
        while done < pages.len() {
            let rest = &pages[done..];
            let at = index + done as u64;
            done += self.fs.with_packers(rest.len() > CHUNK, |own, other| {
                let (first, rest) = rest.split_at(rest.len().min(CHUNK));
                let second = &rest[..rest.len().min(CHUNK)];
                let (mut a, mut b) = (None, None);
                match other {
                    Some(other) if !second.is_empty() => {
                        let next = at + CHUNK as u64;
                        radix_vfs::join(
                            || a = own.pack(first, bytes(at, first.len())),
                            || b = other.pack(second, bytes(next, second.len())),
                        );
                        self.write_chunks(at, [(first, a, &*own), (second, b, &*other)], end)?;
                        Ok(first.len() + second.len())
                    }
                    _ => {
                        a = own.pack(first, bytes(at, first.len()));
                        self.write_chunks(at, [(first, a, &*own), (&[], None, &*own)], end)?;
                        Ok(first.len())
                    }
                }
            })?;
        }
        Ok(())
    }

    /// Writes `chunks`, the file's pages from `index` on, each compressed
    /// into its packer when its frame's length is given, in one change.
    #[inline(never)]
    fn write_chunks(
        &self,
        index: u64,
        chunks: [(&[&Page], Option<usize>, &Packer); 2],
        end: u64,
    ) -> Result<()> {
        self.fs.change(|txn| {
            let mut files = txn.files(&self.fs.volume, self.subvol);
            if index != 0 {
                self.convert_inline(&mut files)?;
            }
            let mut pos = index * PAGE;
            for (pages, made, packer) in chunks {
                let len = pages.len() as u64 * PAGE;
                let size = end.min(pos + len);
                match made {
                    Some(made) => {
                        let mut disk: [&Page; CHUNK] = [pages[0]; CHUNK];
                        let mut n = 0;
                        for (slot, page) in disk.iter_mut().zip(packer.pages(made)) {
                            *slot = page;
                            n += 1;
                        }
                        let kind = (len, compression::ZSTD);
                        files.write_packed(self.ino, pos, &disk[..n], kind, size)?;
                    }
                    None if !pages.is_empty() => {
                        write_split(&mut files, self.ino, pos, pages, true, size)?;
                    }
                    None => {}
                }
                pos += len;
            }
            self.reload(&files)
        })
    }

    /// Drops the file's extents from byte `from` to `to` (whole pages): it
    /// grows over them, and they read as zeros.
    fn zero(&self, from: u64, to: u64) -> Result<()> {
        self.fs.freeing(|txn| {
            let mut files = txn.files(&self.fs.volume, self.subvol);
            self.convert_inline(&mut files)?;
            files.whole(|f| {
                let lost = f.drop_extents(self.ino, from, to)?;
                f.change_inode(self.ino, |d| {
                    put_u64(
                        d,
                        at::NBYTES,
                        crate::format::u64_at(d, at::NBYTES).saturating_sub(lost),
                    );
                })
            })?;
            self.reload(&files)
        })
    }

    /// Cuts the file to `size` bytes: its extents past the sector holding
    /// its end go.
    fn truncate(&self, size: u64) -> Result<()> {
        let sector = u64::from(self.fs.volume.superblock().sectorsize);
        let time = now();
        self.fs.freeing(|txn| {
            let mut files = txn.files(&self.fs.volume, self.subvol);
            if size > 0 {
                self.convert_inline(&mut files)?;
            }
            files.whole(|f| {
                let lost = f.drop_extents(self.ino, size.next_multiple_of(sector), u64::MAX)?;
                f.change_inode(self.ino, |d| {
                    put_u64(d, at::SIZE, size);
                    put_u64(
                        d,
                        at::NBYTES,
                        crate::format::u64_at(d, at::NBYTES).saturating_sub(lost),
                    );
                    put_time(d, at::MTIME, time);
                    put_time(d, at::CTIME, time);
                })
            })?;
            self.reload(&files)
        })
    }

    /// Makes `name` in this directory: a new inode of `kind` with `mode`,
    /// and for a link, `target` inline.
    fn make(&self, name: &str, mode: u32, kind: Kind, target: Option<&[u8]>) -> Result<NodeRef> {
        self.is_dir()?;
        check_name(name)?;
        let time = now();
        let parent = self.inode();
        // A file made in a directory that copies in place does too, and
        // keeps no checksums, as `chattr +C` has Linux do.
        let flags = if kind == Kind::File {
            parent.flags & (inode::flags::NODATACOW | inode::flags::NODATASUM)
        } else {
            0
        };
        let node = self.fs.change(|txn: &mut Txn| {
            if self.removed() {
                return Err(Error::NotFound);
            }
            if self.entry(name)?.is_some() {
                return Err(Error::Exists);
            }
            let ino = txn.take_ino(&self.fs.volume, self.subvol)?;
            let mut files = txn.files(&self.fs.volume, self.subvol);
            let index = files.next_index(self.ino)?;
            let size = target.map_or(0, |t| t.len() as u64);
            files.whole(|f| {
                f.make_inode(ino, mode, flags, size, size, time)?;
                if let Some(target) = target {
                    f.inline_extent(ino, target)?;
                }
                f.add_entry(self.ino, name.as_bytes(), ino, file_type(kind), index, time)
            })?;
            self.reload(&files)?;
            Inner::node_for(&self.fs, self.subvol, self.tree, ino)
        })?;
        Ok(node)
    }

    /// Takes away the name the inode `ino` (`old`) has in directory `dir`
    /// as part of a change, and the inode with it once it has none; the
    /// inode stays while `live` is a node of it, as an orphan. Returns
    /// whether the inode has no names left.
    fn unname(
        files: &mut Files<'_>,
        dir: u64,
        name: &[u8],
        ino: u64,
        old: &Inode,
        live: bool,
        time: u64,
    ) -> Result<bool> {
        files.remove_entry(dir, name, ino, time)?;
        // A directory has one name, as Btrfs counts them.
        let nlink = if old.kind()? == Kind::Directory { 0 } else { old.nlink.saturating_sub(1) };
        files.change_inode(ino, |d| {
            put_u32(d, at::NLINK, nlink);
            put_time(d, at::CTIME, time);
        })?;
        if nlink == 0 {
            if live {
                files.add_orphan(ino)?;
            } else {
                // No orphan item: it had a name until now.
                files.free_inode(ino, false)?;
            }
        }
        Ok(nlink == 0)
    }

    /// What `name` in this directory names, checked as a change needs it:
    /// an inode of this subvolume with its name in its reference, and its
    /// inode item. The caller holds the transaction.
    fn named(&self, files: &Files<'_>, name: &str) -> Result<(u64, Inode)> {
        let location = self.entry(name)?.ok_or(Error::NotFound)?;
        // A subvolume's entry goes with the subvolume (10c.2).
        if location.kind != item::INODE_ITEM {
            return Err(Error::NotSupported);
        }
        let ino = location.objectid;
        let key = Key::new(ino, item::INODE_ITEM, 0);
        let inode = Inode::read(&files.get(self.subvol, &key)?.ok_or(Error::Corrupt)?)?;
        // A name kept in an extended reference (thousands of hard links in
        // one directory, from Linux) is not changed yet.
        files.index_of(ino, self.ino, name.as_bytes())?.ok_or(Error::NotSupported)?;
        Ok((ino, inode))
    }

    /// After a change to inode `ino`: its node in use reads its item again,
    /// and is marked removed if it has no names left.
    fn after(&self, files: &Files<'_>, ino: u64, gone: bool) -> Result<()> {
        let Some(node) = self.fs.live(self.subvol, ino) else { return Ok(()) };
        if gone {
            node.removed.store(true, Ordering::Release);
            return Ok(());
        }
        node.reload(files)
    }
}

/// Writes `pages` with [`Files::write_data`], in halves when there is no
/// one free range that holds them all.
fn write_split(
    files: &mut Files<'_>,
    ino: u64,
    pos: u64,
    pages: &[&Page],
    sums: bool,
    end: u64,
) -> Result<()> {
    match files.write_data(ino, pos, pages, sums, end) {
        Err(Error::NoSpace) if pages.len() > 1 => {
            let half = pages.len() / 2;
            let mid = pos + half as u64 * PAGE;
            write_split(files, ino, pos, &pages[..half], sums, end.min(mid))?;
            write_split(files, ino, mid, &pages[half..], sums, end)
        }
        result => result,
    }
}

/// A file's page cache backing: its extents.
struct FileBacking<'a>(&'a BtrfsNode);

impl Backing for FileBacking<'_> {
    fn persistent(&self) -> bool {
        true
    }

    fn read_page(&self, index: u64, page: &mut Page, len: usize) -> Result<()> {
        self.0.read_page(index, page, len, &mut |_, _| false)
    }

    fn read_pages(
        &self,
        index: u64,
        page: &mut Page,
        len: usize,
        more: &mut dyn FnMut(u64, &[u8]) -> bool,
    ) -> Result<()> {
        self.0.read_page(index, page, len, more)
    }

    fn read_run(
        &self,
        index: u64,
        pages: &mut [CachePage],
        size: u64,
        more: &mut dyn FnMut(u64, &[u8]) -> bool,
    ) -> Result<usize> {
        self.0.read_run(index, pages, size, more)
    }

    fn run_pages(&self) -> usize {
        radix_vfs::cache::READ_RUN
    }

    fn write_page(&self, index: u64, page: &Page, len: usize) -> Result<()> {
        self.0.write_run(index, &[page], len)
    }

    fn write_pages(&self, index: u64, pages: &[&Page], len: usize) -> Result<()> {
        self.0.write_run(index, pages, len)
    }

    /// Space is taken when the data is written, at the next sync; each
    /// page reserves its room as it changes ([`Backing::reserve_pages`]).
    fn reserve(&self, _size: u64) -> Result<()> {
        if self.0.fs.writable() { Ok(()) } else { Err(Error::ReadOnly) }
    }

    fn reserve_pages(&self, pages: usize) -> Result<()> {
        self.0.fs.reserve_data(pages as u64 * PAGE)
    }

    fn release_pages(&self, pages: usize) {
        self.0.fs.release_data(pages as u64 * PAGE);
    }

    fn zero(&self, from: u64, to: u64) -> Result<()> {
        self.0.zero(from, to)
    }

    fn truncate(&self, size: u64) -> Result<()> {
        self.0.truncate(size)
    }
}

impl Node for BtrfsNode {
    fn stat(&self) -> Stat {
        // Whole sectors, as Linux counts them.
        let sector = u64::from(self.fs.volume.superblock().sectorsize);
        let inode = self.inode();
        // A file's size and time are its page cache's, which may be ahead
        // of its inode item.
        let (size, modified) = match &self.cache {
            Some(cache) => {
                let (size, modified, _) = cache.status();
                (size, modified)
            }
            None => (inode.size, inode.mtime),
        };
        Stat {
            kind: self.kind,
            size,
            allocated: inode.nbytes.next_multiple_of(sector),
            id: id_of(self.subvol, self.ino),
            created: inode.otime,
            modified,
            accessed: inode.atime,
        }
    }

    fn kind(&self) -> Kind {
        self.kind
    }

    fn lookup(&self, name: &str) -> Result<NodeRef> {
        self.is_dir()?;
        let _reading = self.fs.reading();
        let location = self.entry(name)?.ok_or(Error::NotFound)?;
        self.open_entry(&location)
    }

    fn list(&self, cursor: u64, f: ListFn<'_>) -> Result<()> {
        self.is_dir()?;
        if !self.fs.writable() {
            return self.list_from(cursor, &mut |next, name, kind| f(next, name, kind));
        }
        // A batch at a time, each handed over with the transaction let go:
        // the caller copies names to a program's memory, which may fault.
        let mut cursor = cursor;
        loop {
            let mut names: KVec<u8> = mem::vec();
            let mut entries: KVec<(u64, usize, usize, Kind)> = mem::vec();
            let mut failed = None;
            {
                let _reading = self.fs.reading();
                self.list_from(cursor, &mut |next, name, kind| {
                    let start = names.len();
                    let kept = mem::extend(&mut names, name.as_bytes()).is_ok()
                        && mem::push(&mut entries, (next, start, name.len(), kind)).is_ok();
                    if !kept {
                        failed = Some(Error::NoMemory);
                    }
                    kept && entries.len() < LIST_BATCH
                })?;
            }
            if let Some(err) = failed {
                return Err(err);
            }
            for &(next, start, len, kind) in &entries {
                let name =
                    core::str::from_utf8(&names[start..start + len]).map_err(|_| Error::Corrupt)?;
                if !f(next, name, kind) {
                    return Ok(());
                }
                cursor = next;
            }
            if entries.len() < LIST_BATCH {
                return Ok(());
            }
        }
    }

    fn read(&self, offset: u64, out: &mut dyn Out) -> Result<usize> {
        match &self.cache {
            Some(cache) => cache.read(offset, out, &FileBacking(self)),
            None if self.kind == Kind::Directory => Err(Error::IsDirectory),
            None => Err(Error::NotSupported),
        }
    }

    fn write(&self, offset: u64, data: &dyn In) -> Result<usize> {
        let cache = match (&self.cache, self.kind) {
            (_, Kind::Directory) => return Err(Error::IsDirectory),
            (Some(cache), _) if self.fs.writable() => cache,
            (Some(_), _) => return Err(Error::ReadOnly),
            (None, _) => {
                return Err(if self.fs.writable() { Error::NotSupported } else { Error::ReadOnly });
            }
        };
        let wrote = cache.write(offset, data, &FileBacking(self))?;
        if wrote > 0 {
            self.changed()?;
        }
        Ok(wrote)
    }

    fn set_size(&self, size: u64) -> Result<()> {
        let cache = match (&self.cache, self.kind) {
            (_, Kind::Directory) => return Err(Error::IsDirectory),
            (Some(cache), _) if self.fs.writable() => cache,
            (Some(_), _) => return Err(Error::ReadOnly),
            (None, _) => {
                return Err(if self.fs.writable() { Error::NotSupported } else { Error::ReadOnly });
            }
        };
        cache.set_size(size, &FileBacking(self))?;
        self.changed()
    }

    fn set_times(&self, accessed: Option<u64>, modified: Option<u64>) -> Result<()> {
        let time = now();
        self.fs.change(|txn| {
            let mut files = txn.files(&self.fs.volume, self.subvol);
            files.whole(|f| {
                f.change_inode(self.ino, |d| {
                    if let Some(accessed) = accessed {
                        put_time(d, at::ATIME, accessed);
                    }
                    if let Some(modified) = modified {
                        put_time(d, at::MTIME, modified);
                    }
                    put_time(d, at::CTIME, time);
                })
            })?;
            self.reload(&files)
        })?;
        // The page cache keeps the time its data changed, which the next
        // flush writes: it takes the new one.
        if let (Some(cache), Some(modified)) = (&self.cache, modified) {
            cache.set_modified(modified);
        }
        Ok(())
    }

    fn sync(&self) -> Result<()> {
        if !self.fs.writable() {
            return Ok(());
        }
        // Until the log tree (10c.2), a file is durable with a commit.
        self.flush()?;
        self.fs.commit()
    }

    fn create(&self, name: &str, kind: Kind) -> Result<NodeRef> {
        self.is_dir()?;
        if !self.fs.writable() {
            return Err(Error::ReadOnly);
        }
        let mode = match kind {
            Kind::File => mode::FILE,
            Kind::Directory => mode::DIR,
            // Devices, pipes and sockets are made in 10c.3.
            _ => return Err(Error::NotSupported),
        };
        self.make(name, mode, kind, None)
    }

    fn make_link(&self, name: &str, target: &str) -> Result<NodeRef> {
        self.is_dir()?;
        if !self.fs.writable() {
            return Err(Error::ReadOnly);
        }
        if target.is_empty() {
            return Err(Error::NotFound);
        }
        if target.len() > LINK_MAX {
            return Err(Error::NameTooLong);
        }
        self.make(name, mode::LINK, Kind::Symlink, Some(target.as_bytes()))
    }

    fn make_socket(&self, _name: &str, _binding: SocketBinding) -> Result<NodeRef> {
        self.is_dir()?;
        if self.fs.writable() { Err(Error::NotSupported) } else { Err(Error::ReadOnly) }
    }

    fn remove(&self, name: &str) -> Result<()> {
        self.is_dir()?;
        check_name(name)?;
        let time = now();
        let mut taken = None;
        let removed = self.fs.freeing(|txn| {
            let mut files = txn.files(&self.fs.volume, self.subvol);
            let (ino, old) = self.named(&files, name)?;
            if old.kind()? == Kind::Directory && !files.is_empty_dir(ino)? {
                return Err(Error::NotEmpty);
            }
            let live = self.fs.live(self.subvol, ino);
            // A file no one has open, kept only for the sync, goes with its
            // last name, as Linux evicts an inode then: its pages are never
            // written, and its items go now, with no orphan item.
            let last = old.kind()? == Kind::Directory || old.nlink <= 1;
            let idle = last && live.as_ref().is_some_and(|node| self.fs.take_idle(node));
            if idle {
                taken.clone_from(&live);
            }
            let gone = files.whole(|f| {
                let open = live.is_some() && !idle;
                BtrfsNode::unname(f, self.ino, name.as_bytes(), ino, &old, open, time)
            })?;
            if let Some(node) = live.as_ref().filter(|_| idle) {
                node.removed.store(true, Ordering::Release);
                node.freed.store(true, Ordering::Release);
            }
            self.reload(&files)?;
            self.after(&files, ino, gone)
        });
        // Taken for nothing, the name staying: back to the sync, or written
        // now without the memory to keep it.
        if let Some(node) = taken
            && !node.freed.load(Ordering::Acquire)
            && !node.pin()
        {
            node.flush()?;
        }
        removed
    }

    fn rename(&self, from: &str, to_dir: &dyn Node, to: &str) -> Result<()> {
        self.is_dir()?;
        check_name(from)?;
        check_name(to)?;
        let target = radix_vfs::downcast::<BtrfsNode>(to_dir).ok_or(Error::CrossMount)?;
        // Linux moves nothing between subvolumes either.
        if !KArc::ptr_eq(&target.fs, &self.fs) || target.subvol != self.subvol {
            return Err(Error::CrossMount);
        }
        target.is_dir()?;
        let time = now();
        self.fs.change(|txn| {
            let mut files = txn.files(&self.fs.volume, self.subvol);
            let (moving, inode) = self.named(&files, from)?;
            if target.removed() {
                return Err(Error::NotFound);
            }
            if target.ino == self.ino && from == to {
                return Ok(());
            }
            let is_dir = inode.kind()? == Kind::Directory;
            // A directory cannot move under itself.
            if is_dir {
                let mut at = Some(target.ino);
                while let Some(dir) = at {
                    if dir == moving {
                        return Err(Error::BadName);
                    }
                    at = files.parent_of(dir)?;
                }
            }
            let replaced = match target.entry(to)? {
                Some(_) => {
                    let (ino, old) = target.named(&files, to)?;
                    if ino == moving {
                        // Two names of one file: nothing moves, as POSIX says.
                        return Ok(());
                    }
                    match (is_dir, old.kind()? == Kind::Directory) {
                        (true, true) if !files.is_empty_dir(ino)? => return Err(Error::NotEmpty),
                        (true, false) => return Err(Error::NotDirectory),
                        (false, true) => return Err(Error::IsDirectory),
                        _ => {}
                    }
                    Some((ino, old, self.fs.live(self.subvol, ino)))
                }
                None => None,
            };
            let index = files.next_index(target.ino)?;
            let ft = file_type(inode.kind()?);
            let gone = files.whole(|f| {
                let mut gone = false;
                if let Some((ino, old, live)) = &replaced {
                    gone = BtrfsNode::unname(
                        f,
                        target.ino,
                        to.as_bytes(),
                        *ino,
                        old,
                        live.is_some(),
                        time,
                    )?;
                }
                f.remove_entry(self.ino, from.as_bytes(), moving, time)?;
                f.add_entry(target.ino, to.as_bytes(), moving, ft, index, time)?;
                f.change_inode(moving, |d| put_time(d, at::CTIME, time))?;
                Ok(gone)
            })?;
            self.reload(&files)?;
            target.reload(&files)?;
            self.after(&files, moving, false)?;
            if let Some((ino, _, _)) = replaced {
                self.after(&files, ino, gone)?;
            }
            Ok(())
        })
    }

    fn read_link(&self, out: &mut dyn Out) -> Result<usize> {
        if self.kind != Kind::Symlink {
            return Err(Error::NotLink);
        }
        // Copied out first: `out` may be a program's memory, which may
        // fault, and the transaction is let go before it is touched.
        let path = {
            let _reading = self.fs.reading();
            self.link_path()?
        };
        let n = path.len().min(out.len());
        out.put(0, &path[..n])?;
        Ok(path.len())
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

impl BtrfsNode {
    /// Lists the directory from `cursor`, as [`Node::list`] does; the
    /// caller holds the transaction on a writable mount.
    fn list_from(&self, cursor: u64, f: &mut dyn FnMut(u64, &str, Kind) -> bool) -> Result<()> {
        let volume = &self.fs.volume;
        let mut at =
            Cursor::seek(volume, &self.tree()?, &Key::new(self.ino, item::DIR_INDEX, cursor))?;
        while let Some((key, data)) = at.get() {
            if !key.is(self.ino, item::DIR_INDEX) {
                break;
            }
            for entry in Entries::of(data) {
                let entry = entry?;
                // Radix's names are UTF-8; others are left out (Phase 10b).
                let Ok(name) = core::str::from_utf8(entry.name) else { continue };
                let kind = match entry.kind {
                    Some(kind) => kind,
                    None => self.open_entry(&entry.location)?.kind(),
                };
                if !f(key.offset + 1, name, kind) {
                    return Ok(());
                }
            }
            at.advance()?;
        }
        Ok(())
    }

    /// A link's path; the caller holds the transaction on a writable
    /// mount.
    fn link_path(&self) -> Result<KVec<u8>> {
        let volume = &self.fs.volume;
        let key = Key::new(self.ino, item::EXTENT_DATA, 0);
        let cursor = Cursor::seek(volume, &self.tree()?, &key)?;
        let Some((found, data)) = cursor.get() else { return Err(Error::Corrupt) };
        if found != key {
            return Err(Error::Corrupt);
        }
        // A link's path is inline, and not compressed. It is as long as
        // its inode says, as Linux reads it: `mkfs.btrfs` keeps a NUL
        // after it.
        let Extent::Inline { compression: Compression::None, data, .. } = Extent::read(data)?
        else {
            return Err(Error::Corrupt);
        };
        let len =
            usize::try_from(self.inode().size).map_or(data.len(), |size| size.min(data.len()));
        Ok(mem::copy(&data[..len])?)
    }
}

impl Drop for BtrfsNode {
    /// A node of an inode with no names left was all that kept its items:
    /// they go at the next sync, unless they went with its last name.
    fn drop(&mut self) {
        if *self.removed.get_mut() {
            // Its dirty pages are never written: their room goes back.
            let pages = self.dirty_pages();
            self.fs.release_data(pages as u64 * PAGE);
            if !*self.freed.get_mut() {
                self.fs.reaped(self.subvol, self.ino);
            }
        }
    }
}

impl BtrfsNode {
    /// How many pages the file's cache holds.
    #[cfg(test)]
    pub(crate) fn cached_pages(&self) -> usize {
        self.cache.as_ref().map_or(0, FileCache::pages)
    }

    /// Drops up to `max` pages of its cache; returns how many.
    pub(crate) fn shrink(&self, max: usize) -> usize {
        match &self.cache {
            Some(cache) => cache.shrink(max, &FileBacking(self)),
            None => 0,
        }
    }
}

/// The most compressed extents a run takes: 1 MiB of data.
const RUN_EXTENTS: usize = 8;

/// The sectors of the disk that the compressed `extent` takes, and its
/// data's size; fails with `Corrupt` when they are not what Linux makes.
fn packed_sizes(extent: &Regular) -> Result<(usize, usize)> {
    let compressed = usize::try_from(extent.disk_num_bytes).map_err(|_| Error::Corrupt)?;
    let len = usize::try_from(extent.ram_bytes).map_err(|_| Error::Corrupt)?;
    if compressed == 0
        || compressed > unpack::MAX_COMPRESSED
        || !compressed.is_multiple_of(PAGE_SIZE)
        || !extent.disk_bytenr.is_multiple_of(PAGE)
        || extent.disk_bytenr.checked_add(extent.disk_num_bytes).is_none()
        || len > unpack::MAX_UNCOMPRESSED
    {
        return Err(Error::Corrupt);
    }
    Ok((compressed / PAGE_SIZE, len))
}

/// The compressed extents of a run that ends at `end` in the file of
/// inode `ino`: `first`, (its start in the file, it), at `cursor`, and
/// those right after it in the file, compressed and whole before `end`,
/// up to [`RUN_EXTENTS`] in all. What does not read stops the run, for a
/// read of its own to find.
fn packed_run(
    ino: u64,
    cursor: Option<Cursor<'_>>,
    first: (u64, Regular),
    end: u64,
) -> Result<KVec<(u64, Regular)>> {
    let mut extents = mem::vec_with_capacity(RUN_EXTENTS)?;
    mem::push(&mut extents, first)?;
    let Some(mut cursor) = cursor else { return Ok(extents) };
    let mut next = first.0.checked_add(first.1.num_bytes).ok_or(Error::Corrupt)?;
    while extents.len() < RUN_EXTENTS && cursor.advance().is_ok() {
        let Some((at, data)) = cursor.get() else { break };
        if !at.is(ino, item::EXTENT_DATA) || at.offset != next || !at.offset.is_multiple_of(PAGE) {
            break;
        }
        let Ok(Extent::Regular(extent)) = Extent::read(data) else { break };
        let after = at.offset.checked_add(extent.num_bytes);
        if extent.compression == Compression::None
            || extent.disk_bytenr == 0
            || extent.num_bytes == 0
            || after.is_none_or(|after| after > end)
            || packed_sizes(&extent).is_err()
        {
            break;
        }
        mem::push(&mut extents, (at.offset, extent))?;
        next = at.offset + extent.num_bytes;
    }
    Ok(extents)
}

/// Copies `data` from byte `from` on into `to`, with zeros after its end.
fn copy_from(data: &[u8], from: u64, to: &mut [u8]) {
    let from = usize::try_from(from).map_or(data.len(), |from| from.min(data.len()));
    let n = (data.len() - from).min(to.len());
    to[..n].copy_from_slice(&data[from..from + n]);
    to[n..].fill(0);
}
