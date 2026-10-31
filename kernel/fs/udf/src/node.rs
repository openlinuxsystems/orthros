// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! UDF files and directories as VFS nodes.
//!
//! A node is made from its file entry (or extended file entry): its
//! owner, permissions, times and size, and its data, held in the entry
//! itself or in extents its allocation descriptors list. A file's data
//! goes through its page cache, read from its extents straight into the
//! cache's pages; a directory's file identifier descriptors and a link's
//! path are read through the filesystem's cache of sectors.

use core::any::Any;

use radix_block::cache::BlockCache;
use radix_block::mem::CachePage;
use radix_block::{PAGE_SIZE, Page, SECTOR, SegmentMut};
use radix_vfs::cache::{Backing, FileCache, page_len};
use radix_vfs::mem::{self, KArc, KVec};
use radix_vfs::{Access, Error, Kind, ListFn, Node, NodeRef, Out, PATH_MAX, Result, Stat};

use crate::format::{self, LongAd, NameBuf, ad_kind, ad_len, ident, kind, le16, le32, le64, tag};
use crate::fs::Inner;
use crate::volume::{Volume, zeroed};

const PAGE: u64 = PAGE_SIZE as u64;
/// The most runs of one file.
const MAX_RUNS: usize = 1 << 16;
/// The most extents of allocation descriptors one file's go on in.
const MAX_CONTINUATIONS: usize = 4096;
/// The most indirect entries followed to a file's entry.
const MAX_INDIRECT: usize = 16;
/// The most implementation use bytes of a file identifier descriptor.
const MAX_IMPLEMENTATION_USE: usize = 2048;

/// What a file identifier descriptor's characteristics say.
mod characteristic {
    /// Not shown.
    pub const HIDDEN: u8 = 1;
    /// A directory.
    pub const DIRECTORY: u8 = 2;
    /// Removed.
    pub const DELETED: u8 = 4;
    /// The parent directory's entry.
    pub const PARENT: u8 = 8;
}

/// Part of a file: `len` bytes from byte `pos` of it, at byte `at` of the
/// device, or none for a hole, which reads as zeros.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Run {
    pub(crate) pos: u64,
    pub(crate) len: u64,
    pub(crate) at: Option<u64>,
}

/// A file entry, read.
pub(crate) struct Entry {
    /// Its file type (ECMA-167 4/14.6.6).
    pub(crate) file_type: u8,
    /// Its ICB tag's flags.
    flags: u16,
    owner: u32,
    group: u32,
    permissions: u32,
    links: u16,
    pub(crate) size: u64,
    accessed: u64,
    modified: u64,
    created: u64,
    changed: u64,
    /// Its data's runs, in order, unless the data is in the entry.
    pub(crate) runs: KVec<Run>,
    pub(crate) embedded: Option<KVec<u8>>,
}

impl Entry {
    /// Reads the entry the ICB `icb` points to, following indirect
    /// entries to the latest one.
    pub(crate) fn read(volume: &Volume, cache: &BlockCache, mut icb: LongAd) -> Result<Entry> {
        let mut buf = zeroed(volume.block as usize)?;
        for _ in 0..MAX_INDIRECT {
            let (at, _) = volume.locate(icb.partition, icb.block)?;
            cache.read(at, &mut buf)?;
            match tag(&buf, Some(icb.block)) {
                Some(ident::INDIRECT) => icb = format::long_ad(&buf, 36),
                Some(id @ (ident::FILE_ENTRY | ident::EXTENDED_FILE_ENTRY)) => {
                    // Strategy 4096 (write-once media): a newer entry may
                    // follow through an indirect entry in the next block.
                    if le16(&buf, 20) == 4096
                        && let Some(next) = newer(volume, cache, icb)?
                    {
                        icb = next;
                        continue;
                    }
                    return Entry::parse(volume, cache, &buf, id, icb);
                }
                _ => return Err(Error::Corrupt),
            }
        }
        Err(Error::Corrupt)
    }

    /// The entry in `buf`, an extended one if `id` says so, whose ICB is
    /// `icb`.
    fn parse(
        volume: &Volume,
        cache: &BlockCache,
        buf: &[u8],
        id: u16,
        icb: LongAd,
    ) -> Result<Entry> {
        let extended = id == ident::EXTENDED_FILE_ENTRY;
        // Where its times are: last read, last changed, made (an extended
        // entry only) and its attributes last changed.
        let (accessed, modified, created, changed) =
            if extended { (80, 92, 104, 116) } else { (72, 84, 0, 96) };
        let time = |at: usize| if at == 0 { 0 } else { format::timestamp(&buf[at..]) };
        let (extended_len, ad_len_at, base): (usize, usize, usize) =
            if extended { (208, 212, 216) } else { (168, 172, 176) };
        let ea = le32(buf, extended_len) as usize;
        let ad = le32(buf, ad_len_at) as usize;
        let from = base.checked_add(ea).ok_or(Error::Corrupt)?;
        let area =
            buf.get(from..from.checked_add(ad).ok_or(Error::Corrupt)?).ok_or(Error::Corrupt)?;
        let flags = le16(buf, 34);
        let mut entry = Entry {
            file_type: buf[27],
            flags,
            owner: le32(buf, 36),
            group: le32(buf, 40),
            permissions: le32(buf, 44),
            links: le16(buf, 48),
            size: le64(buf, 56),
            accessed: time(accessed),
            modified: time(modified),
            created: time(created),
            changed: time(changed),
            runs: mem::vec(),
            embedded: None,
        };
        match flags & 7 {
            3 => {
                let len = usize::try_from(entry.size).unwrap_or(usize::MAX).min(area.len());
                entry.embedded = Some(mem::copy(&area[..len])?);
            }
            ads @ 0..=2 => entry.runs = runs(volume, cache, area, ads, icb.partition)?,
            _ => return Err(Error::Corrupt),
        }
        Ok(entry)
    }

    /// Reads from byte `offset` of its data into `out`, through `cache`;
    /// returns how many bytes it read, fewer at its end.
    pub(crate) fn read_at(&self, cache: &BlockCache, offset: u64, out: &mut [u8]) -> Result<usize> {
        let want = self.size.saturating_sub(offset).min(out.len() as u64) as usize;
        let out = &mut out[..want];
        if let Some(data) = &self.embedded {
            let from = (offset as usize).min(data.len());
            let n = (data.len() - from).min(want);
            out[..n].copy_from_slice(&data[from..from + n]);
            out[n..].fill(0);
            return Ok(want);
        }
        let mut done = 0;
        while done < want {
            let pos = offset + done as u64;
            let i = self.runs.partition_point(|run| run.pos + run.len <= pos);
            let part = match self.runs.get(i).filter(|run| run.pos <= pos) {
                Some(run) => {
                    let part = ((run.pos + run.len - pos) as usize).min(want - done);
                    match run.at {
                        Some(at) => {
                            cache.read(at + (pos - run.pos), &mut out[done..done + part])?;
                        }
                        None => out[done..done + part].fill(0),
                    }
                    part
                }
                // Past its extents: zeros.
                None => {
                    out[done..].fill(0);
                    want - done
                }
            };
            done += part;
        }
        Ok(want)
    }

    fn kind(&self) -> Kind {
        kind_of(self.file_type)
    }
}

/// The kind of file of file type `file_type`.
pub(crate) fn kind_of(file_type: u8) -> Kind {
    match file_type {
        4 => Kind::Directory,
        6 => Kind::BlockDevice,
        7 => Kind::CharDevice,
        9 => Kind::Pipe,
        10 => Kind::Socket,
        12 => Kind::Symlink,
        _ => Kind::File,
    }
}

/// The newer entry an indirect entry in the block after `icb` points to.
fn newer(volume: &Volume, cache: &BlockCache, icb: LongAd) -> Result<Option<LongAd>> {
    let Some(block) = icb.block.checked_add(1) else { return Ok(None) };
    let Ok((at, _)) = volume.locate(icb.partition, block) else { return Ok(None) };
    let mut buf = [0u8; 52];
    if cache.read(at, &mut buf).is_err() || tag(&buf, Some(block)) != Some(ident::INDIRECT) {
        return Ok(None);
    }
    let next = format::long_ad(&buf, 36);
    Ok((ad_len(next.len) > 0).then_some(next))
}

/// The runs the allocation descriptors in `area` list, short (`ads` 0,
/// in `partition`), long (1) or extended (2).
fn runs(
    volume: &Volume,
    cache: &BlockCache,
    area: &[u8],
    ads: u16,
    partition: u16,
) -> Result<KVec<Run>> {
    let size = match ads {
        0 => 8,
        1 => 16,
        _ => 20,
    };
    let mut runs = mem::vec();
    let mut pos = 0u64;
    let mut more: Option<KVec<u8>> = None;
    let mut continuations = 0;
    let mut at = 0;
    loop {
        let bytes: &[u8] = more.as_deref().unwrap_or(area);
        let Some(ad) = bytes.get(at..at + size) else { break };
        let (len, block, part) = match ads {
            0 => (le32(ad, 0), le32(ad, 4), partition),
            1 => (le32(ad, 0), le32(ad, 4), le16(ad, 8)),
            _ => (le32(ad, 0), le32(ad, 12), le16(ad, 16)),
        };
        at += size;
        let bytes_long = u64::from(ad_len(len));
        if bytes_long == 0 {
            break;
        }
        match ad_kind(len) {
            kind::NEXT => {
                continuations += 1;
                if continuations > MAX_CONTINUATIONS {
                    return Err(Error::Corrupt);
                }
                let (byte, _) = volume.locate(part, block)?;
                let mut aed = zeroed(volume.block as usize)?;
                cache.read(byte, &mut aed)?;
                if tag(&aed, Some(block)) != Some(ident::ALLOCATION_EXTENT) {
                    return Err(Error::Corrupt);
                }
                let len = (le32(&aed, 20) as usize).min(aed.len() - 24);
                more = Some(mem::copy(&aed[24..24 + len])?);
                at = 0;
                continue;
            }
            kind::RECORDED => {
                let mut done = 0;
                let mut b = block;
                while done < bytes_long {
                    let (byte, row) = volume.locate(part, b)?;
                    let n = (u64::from(row) * volume.block).min(bytes_long - done);
                    push_run(&mut runs, Run { pos: pos + done, len: n, at: Some(byte) })?;
                    done += n;
                    b = b.checked_add(row).ok_or(Error::Corrupt)?;
                }
            }
            _ => push_run(&mut runs, Run { pos, len: bytes_long, at: None })?,
        }
        pos += bytes_long;
    }
    Ok(runs)
}

/// Adds `run`, joined to the last if it goes on from it.
fn push_run(runs: &mut KVec<Run>, run: Run) -> Result<()> {
    if let Some(last) = runs.last_mut()
        && last.pos + last.len == run.pos
        && match (last.at, run.at) {
            (Some(a), Some(b)) => a + last.len == b,
            (None, None) => true,
            _ => false,
        }
    {
        last.len += run.len;
        return Ok(());
    }
    if runs.len() == MAX_RUNS {
        return Err(Error::Corrupt);
    }
    Ok(mem::push(runs, run)?)
}

/// One entry of a directory, as a scan finds it.
pub(crate) struct Found<'a> {
    /// Its ICB.
    pub(crate) icb: LongAd,
    /// Where the scan goes on after it, in bytes into the directory.
    pub(crate) next: u64,
    pub(crate) name: &'a str,
    pub(crate) kind: Kind,
}

/// A UDF file, directory or symbolic link.
pub struct UdfNode {
    fs: KArc<Inner>,
    id: u64,
    entry: Entry,
    /// A symbolic link's target.
    target: Option<KVec<u8>>,
    /// A file's data.
    cache: Option<FileCache>,
}

impl UdfNode {
    /// The node of the file whose entry `entry` is, at byte `id`.
    pub(crate) fn new(fs: &KArc<Inner>, id: u64, entry: Entry) -> Result<KArc<UdfNode>> {
        let target =
            if entry.kind() == Kind::Symlink { Some(link_target(fs, &entry)?) } else { None };
        let cache =
            (entry.kind() == Kind::File).then(|| FileCache::new(entry.size, entry.modified));
        Ok(mem::arc(UdfNode { fs: fs.clone(), id, entry, target, cache })?)
    }

    /// Drops up to `max` of its cached pages.
    pub(crate) fn shrink(&self, max: usize) -> usize {
        self.cache.as_ref().map_or(0, |cache| cache.shrink(max, &FileBacking(self)))
    }

    fn is_dir(&self) -> Result<()> {
        if self.entry.kind() == Kind::Directory { Ok(()) } else { Err(Error::NotDirectory) }
    }

    /// Calls `f` with each entry of the directory from `from` bytes into
    /// it, but its parent, and those deleted or hidden (as Linux hides
    /// them), until `f` returns false.
    pub(crate) fn scan(
        &self,
        from: u64,
        f: &mut dyn FnMut(&Found<'_>) -> Result<bool>,
    ) -> Result<()> {
        let size = self.entry.size;
        let cache = &self.fs.cache;
        let mut fid = zeroed(38 + MAX_IMPLEMENTATION_USE + 256)?;
        let mut name = NameBuf::default();
        let mut pos = from;
        while pos + 38 <= size {
            self.entry.read_at(cache, pos, &mut fid[..38])?;
            if le16(&fid, 0) != ident::FILE_ID {
                return Err(Error::Corrupt);
            }
            let (chars, name_len) = (fid[18], usize::from(fid[19]));
            let use_len = usize::from(le16(&fid, 36));
            if use_len > MAX_IMPLEMENTATION_USE {
                return Err(Error::Corrupt);
            }
            let len = 38 + use_len + name_len;
            self.entry.read_at(cache, pos, &mut fid[..len])?;
            if tag(&fid[..len], None) != Some(ident::FILE_ID) {
                return Err(Error::Corrupt);
            }
            let next = pos + ((len as u64 + 3) & !3);
            let skip = characteristic::DELETED | characteristic::PARENT | characteristic::HIDDEN;
            if chars & skip == 0 && name_len > 0 {
                format::cs0(&fid[38 + use_len..len], &mut name);
                if let Some(text) = name.name() {
                    let icb = format::long_ad(&fid, 20);
                    let kind = if chars & characteristic::DIRECTORY != 0 {
                        Kind::Directory
                    } else {
                        self.fs.kind_at(icb)?
                    };
                    if !f(&Found { icb, next, name: text, kind })? {
                        break;
                    }
                }
            }
            pos = next;
        }
        Ok(())
    }
}

/// A symbolic link's target: its path components (ECMA-167 4/14.16)
/// joined with `/`.
fn link_target(fs: &Inner, entry: &Entry) -> Result<KVec<u8>> {
    let len = usize::try_from(entry.size).map_err(|_| Error::Corrupt)?;
    if len > PATH_MAX {
        return Err(Error::NotSupported);
    }
    let mut data = zeroed(len)?;
    entry.read_at(&fs.cache, 0, &mut data)?;
    let mut path = mem::vec_with_capacity(len + 16)?;
    let mut name = NameBuf::default();
    let mut at = 0;
    while at + 4 <= data.len() {
        let (kind, len) = (data[at], usize::from(data[at + 1]));
        let component = data.get(at + 4..at + 4 + len).ok_or(Error::Corrupt)?;
        match kind {
            // The root of something other than this filesystem, named.
            1 if len > 0 => {}
            // The root.
            1 | 2 => {
                path.clear();
                mem::push(&mut path, b'/')?;
            }
            3 => mem::extend(&mut path, b"../")?,
            4 => mem::extend(&mut path, b"./")?,
            5 => {
                format::cs0(component, &mut name);
                mem::extend(&mut path, name.text().as_bytes())?;
                mem::push(&mut path, b'/')?;
            }
            _ => return Err(Error::Corrupt),
        }
        at += 4 + len;
    }
    if path.len() > 1 && path.last() == Some(&b'/') {
        path.pop();
    }
    Ok(path)
}

/// A file's data, below its page cache: its runs on the device.
struct FileBacking<'a>(&'a UdfNode);

impl Backing for FileBacking<'_> {
    fn persistent(&self) -> bool {
        true
    }

    fn read_page(&self, index: u64, page: &mut Page, len: usize) -> Result<()> {
        let node = self.0;
        let pos = index * PAGE;
        let entry = &node.entry;
        let device = node.fs.cache.device();
        let unit = device.info().unit() * SECTOR;
        // A page inside one recorded run, from a whole block of the
        // device: read straight into it.
        let i = entry.runs.partition_point(|run| run.pos + run.len <= pos);
        if let Some(run) = entry.runs.get(i)
            && let Some(at) = run.at
            && run.pos <= pos
            && run.pos + run.len >= pos + len as u64
            && (at + (pos - run.pos)).is_multiple_of(unit)
        {
            let whole = (len as u64).div_ceil(unit) * unit;
            if whole <= PAGE {
                let segment = SegmentMut { page: &mut *page, offset: 0, len: whole as usize };
                device.read((at + (pos - run.pos)) / SECTOR, &mut [segment])?;
                radix_hw::device_wrote(&mut page.bytes_mut()[..whole as usize]);
                page.bytes_mut()[len..].fill(0);
                return Ok(());
            }
        }
        page.bytes_mut().fill(0);
        entry.read_at(&node.fs.cache, pos, &mut page.bytes_mut()[..len])?;
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
        let device = node.fs.cache.device();
        let unit = device.info().unit() * SECTOR;
        let pos = index * PAGE;
        let runs = &node.entry.runs;
        let i = runs.partition_point(|run| run.pos + run.len <= pos);
        let max = pages.len().min(device.info().max_segments.clamp(1, radix_vfs::cache::READ_RUN));
        // Whole pages in one recorded run, from a whole block of the
        // device.
        let found = runs.get(i).and_then(|run| {
            let at = run.at? + (pos - run.pos);
            (run.pos <= pos && at.is_multiple_of(unit)).then_some((at, run.pos + run.len - pos))
        });
        let n = match found {
            Some((_, left)) if node.entry.embedded.is_none() => {
                let in_file = size.saturating_sub(pos).div_ceil(PAGE);
                (max as u64).min(in_file).min(left / PAGE) as usize
            }
            _ => 0,
        };
        let Some((at, _)) = found.filter(|_| n >= 2) else {
            let page = pages.first_mut().ok_or(Error::Io)?;
            self.read_page(index, page, page_len(index, size))?;
            return Ok(1);
        };
        {
            let mut segments = mem::vec_with_capacity(n)?;
            for page in &mut pages[..n] {
                mem::push(&mut segments, SegmentMut { page, offset: 0, len: PAGE_SIZE })?;
            }
            device.read(at / SECTOR, &mut segments)?;
        }
        for (page_index, page) in (index..).zip(&mut pages[..n]) {
            radix_hw::device_wrote(page.bytes_mut());
            let keep = size.saturating_sub(page_index * PAGE).min(PAGE) as usize;
            page.bytes_mut()[keep..].fill(0);
        }
        Ok(n)
    }

    fn run_pages(&self) -> usize {
        radix_vfs::cache::READ_RUN
    }

    fn reserve(&self, _size: u64) -> Result<()> {
        Err(Error::ReadOnly)
    }

    fn truncate(&self, _size: u64) -> Result<()> {
        Err(Error::ReadOnly)
    }
}

/// An owner or group of none, which UDF writes as all ones.
const NONE: u32 = u32::MAX;

impl Node for UdfNode {
    fn stat(&self) -> Stat {
        let e = &self.entry;
        let block = self.fs.volume.block;
        let allocated =
            e.runs.iter().filter(|r| r.at.is_some()).map(|r| r.len.div_ceil(block) * block).sum();
        let created = if e.created != 0 { e.created } else { e.modified };
        Stat {
            kind: e.kind(),
            size: e.size,
            allocated,
            id: self.id,
            created,
            modified: e.modified,
            accessed: if e.accessed != 0 { e.accessed } else { e.modified },
            changed: if e.changed != 0 { e.changed } else { e.modified },
            links: u32::from(e.links).max(1),
            device: 0,
        }
    }

    fn kind(&self) -> Kind {
        self.entry.kind()
    }

    fn read_only(&self) -> bool {
        true
    }

    fn lookup(&self, name: &str) -> Result<NodeRef> {
        self.is_dir()?;
        let mut icb = None;
        self.scan(0, &mut |found| {
            let same = found.name == name;
            if same {
                icb = Some(found.icb);
            }
            Ok(!same)
        })?;
        let icb = icb.ok_or(Error::NotFound)?;
        Ok(Inner::node_at(&self.fs, icb)?)
    }

    fn access(&self) -> Access {
        let e = &self.entry;
        // Each of other, group and owner has five bits: execute, write,
        // read, and two POSIX has not.
        let p = e.permissions;
        let mut mode = ((p >> 10) & 7) << 6 | ((p >> 5) & 7) << 3 | (p & 7);
        // The ICB's set-user-ID, set-group-ID and sticky flags.
        for (flag, bit) in [(0x40, 0o4000), (0x80, 0o2000), (0x100, 0o1000)] {
            if e.flags & flag != 0 {
                mode |= bit;
            }
        }
        let id = |n: u32| if n == NONE { 0 } else { n };
        Access { owner: id(e.owner), group: id(e.group), mode: mode as u16 }
    }

    fn read_link(&self, out: &mut dyn Out) -> Result<usize> {
        let target = self.target.as_ref().ok_or(Error::NotLink)?;
        let len = target.len().min(out.len());
        out.put(0, &target[..len])?;
        Ok(target.len())
    }

    fn list(&self, cursor: u64, f: ListFn<'_>) -> Result<()> {
        self.is_dir()?;
        self.scan(cursor, &mut |found| Ok(f(found.next, found.name, found.kind)))
    }

    fn read(&self, offset: u64, out: &mut dyn Out) -> Result<usize> {
        match &self.cache {
            Some(cache) => cache.read(offset, out, &FileBacking(self)),
            None if self.entry.kind() == Kind::Directory => Err(Error::IsDirectory),
            None => Err(Error::NotSupported),
        }
    }

    fn write(&self, _offset: u64, _data: &dyn radix_vfs::In) -> Result<usize> {
        Err(Error::ReadOnly)
    }

    fn set_size(&self, _size: u64) -> Result<()> {
        Err(Error::ReadOnly)
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}
