// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! ISO 9660 files and directories as VFS nodes.
//!
//! A node is made from its directory records: where its extents are, how
//! big it is, and what Rock Ridge says of its mode, owner, times and
//! links. A file's data goes through its page cache, read from its
//! extents straight into the cache's pages; a directory's records are
//! read through the filesystem's cache of sectors.

use core::any::Any;

use radix_block::mem::CachePage;
use radix_block::{PAGE_SIZE, Page, SECTOR, SegmentMut};
use radix_vfs::cache::{Backing, FileCache, page_len};
use radix_vfs::mem::{self, KArc, KVec};
use radix_vfs::{Access, Error, Kind, ListFn, Node, NodeRef, Out, Result, Stat};

use crate::format::{self, NameBuf, RECORD_MAX, Volume, flag};
use crate::fs::{Inner, MAX_EXTENTS, Tree, read_record};
use crate::rock::{Attributes, Target};

const PAGE: u64 = PAGE_SIZE as u64;

/// What a node is made from: its records, read.
pub(crate) struct Loaded {
    kind: Kind,
    /// Its extents: where each starts on the device, in bytes, and its
    /// length.
    extents: KVec<(u64, u64)>,
    size: u64,
    recorded: u64,
    attributes: Attributes,
    /// A symbolic link's target.
    target: Option<KVec<u8>>,
    /// Whether it is interleaved, which is not read.
    interleaved: bool,
}

impl Loaded {
    /// Reads the records of the file whose first record is at byte `at`.
    pub(crate) fn read(fs: &Inner, at: u64) -> Result<Loaded> {
        let mut buf = [0u8; RECORD_MAX];
        let mut extents = mem::vec_with_capacity(1)?;
        let mut pos = at;
        let mut size = 0u64;
        let mut interleaved = false;
        // Each extent's record, the last without the multi-extent flag.
        loop {
            let len = read_record(&fs.cache, pos, &mut buf)?;
            let record = format::record(&buf[..len]).ok_or(Error::Corrupt)?;
            interleaved |= record.unit != 0 || record.gap != 0;
            let start = record.data_block() * fs.block;
            mem::push(&mut extents, (start, u64::from(record.size)))?;
            size += u64::from(record.size);
            if record.flags & flag::MULTI_EXTENT == 0 {
                break;
            }
            if extents.len() == MAX_EXTENTS {
                return Err(Error::Corrupt);
            }
            pos += len as u64;
            // The next record may begin the next block.
            if pos % fs.block + 34 > fs.block || read_record(&fs.cache, pos, &mut buf)? == 0 {
                pos = (pos / fs.block + 1) * fs.block;
            }
        }
        // The first record says what the file is.
        let len = read_record(&fs.cache, at, &mut buf)?;
        let record = format::record(&buf[..len]).ok_or(Error::Corrupt)?;
        let mut name = NameBuf::default();
        let attributes = fs.attributes(&record, &mut name, None)?;
        let kind = Inner::kind_of(&record, &attributes);
        let mut loaded = Loaded {
            kind,
            extents,
            size,
            recorded: record.recorded,
            attributes,
            target: None,
            interleaved,
        };
        if kind == Kind::Symlink {
            // Its target, read again with room for it.
            let mut target = mem::boxed(Target::default())?;
            fs.attributes(&record, &mut name, Some(&mut target))?;
            let bytes = mem::copy(target.bytes().ok_or(Error::Corrupt)?)?;
            loaded.size = bytes.len() as u64;
            loaded.target = Some(bytes);
        }
        if let Some(child) = attributes.child {
            // A relocated directory: its data, and its own record's size,
            // are where `CL` says.
            let start = u64::from(child) * fs.block;
            let len = read_record(&fs.cache, start, &mut buf)?;
            let own = format::record(&buf[..len]).ok_or(Error::Corrupt)?;
            loaded.extents.clear();
            mem::push(&mut loaded.extents, (own.data_block() * fs.block, u64::from(own.size)))?;
            loaded.size = u64::from(own.size);
        }
        Ok(loaded)
    }
}

/// An ISO 9660 file, directory or symbolic link.
pub struct IsoNode {
    fs: KArc<Inner>,
    id: u64,
    kind: Kind,
    extents: KVec<(u64, u64)>,
    size: u64,
    recorded: u64,
    attributes: Attributes,
    target: Option<KVec<u8>>,
    interleaved: bool,
    /// A file's data.
    cache: Option<FileCache>,
}

impl IsoNode {
    /// The root directory of `volume`; its own record (`.`) has its Rock
    /// Ridge attributes.
    pub(crate) fn root(fs: &KArc<Inner>, volume: &Volume) -> Result<KArc<IsoNode>> {
        let start = u64::from(volume.root_extent) * fs.block;
        let mut attributes = Attributes::default();
        let mut recorded = volume.root_recorded;
        let mut buf = [0u8; RECORD_MAX];
        if matches!(fs.tree, Tree::Rock(_)) {
            let len = read_record(&fs.cache, start, &mut buf)?;
            let own = format::record(&buf[..len]).ok_or(Error::Corrupt)?;
            let mut name = NameBuf::default();
            attributes = fs.attributes(&own, &mut name, None)?;
            recorded = own.recorded;
        }
        let mut extents = mem::vec_with_capacity(1)?;
        mem::push(&mut extents, (start, u64::from(volume.root_size)))?;
        Ok(mem::arc(IsoNode {
            fs: fs.clone(),
            id: start,
            kind: Kind::Directory,
            extents,
            size: u64::from(volume.root_size),
            recorded,
            attributes,
            target: None,
            interleaved: false,
            cache: None,
        })?)
    }

    /// The node of the file whose first record is at byte `at`.
    pub(crate) fn new(fs: &KArc<Inner>, at: u64, loaded: Loaded) -> Result<KArc<IsoNode>> {
        let modified = loaded.attributes.modified.unwrap_or(loaded.recorded);
        let cache = (loaded.kind == Kind::File).then(|| FileCache::new(loaded.size, modified));
        Ok(mem::arc(IsoNode {
            fs: fs.clone(),
            id: at,
            kind: loaded.kind,
            extents: loaded.extents,
            size: loaded.size,
            recorded: loaded.recorded,
            attributes: loaded.attributes,
            target: loaded.target,
            interleaved: loaded.interleaved,
            cache,
        })?)
    }

    /// Drops up to `max` of its cached pages.
    pub(crate) fn shrink(&self, max: usize) -> usize {
        self.cache.as_ref().map_or(0, |cache| cache.shrink(max, &FileBacking(self)))
    }

    fn is_dir(&self) -> Result<()> {
        if self.kind == Kind::Directory { Ok(()) } else { Err(Error::NotDirectory) }
    }

    /// Where the directory's data is: its first byte on the device, and
    /// its size.
    fn dir_data(&self) -> (u64, u64) {
        self.extents.first().copied().unwrap_or((0, 0))
    }

    /// Where the file's byte `pos` is on the device, and how many bytes
    /// of its extent follow it.
    fn place(&self, pos: u64) -> Option<(u64, u64)> {
        let mut base = 0;
        for &(start, len) in &self.extents {
            if pos < base + len {
                return Some((start + (pos - base), len - (pos - base)));
            }
            base += len;
        }
        None
    }
}

/// A file's data, below its page cache: its extents on the device.
struct FileBacking<'a>(&'a IsoNode);

impl FileBacking<'_> {
    /// Reads `len` bytes from byte `at` of the device into `page`'s first
    /// bytes, its other bytes zero: straight into the page when `at` is a
    /// whole number of the device's blocks, else through the cache.
    fn read_into(&self, at: u64, page: &mut Page, len: usize) -> Result<()> {
        let fs = &self.0.fs;
        let device = fs.cache.device();
        let unit = device.info().unit() * SECTOR;
        let whole = (len as u64).div_ceil(unit) * unit;
        if at.is_multiple_of(unit) && whole <= PAGE {
            let segment = SegmentMut { page: &mut *page, offset: 0, len: whole as usize };
            device.read(at / SECTOR, &mut [segment])?;
            radix_hw::device_wrote(&mut page.bytes_mut()[..whole as usize]);
        } else {
            fs.cache.read(at, &mut page.bytes_mut()[..len])?;
        }
        page.bytes_mut()[len..].fill(0);
        Ok(())
    }
}

impl Backing for FileBacking<'_> {
    fn persistent(&self) -> bool {
        true
    }

    fn read_page(&self, index: u64, page: &mut Page, len: usize) -> Result<()> {
        let node = self.0;
        if node.interleaved {
            return Err(Error::NotSupported);
        }
        let pos = index * PAGE;
        let mut done = 0;
        page.bytes_mut().fill(0);
        // A page may span two extents of a file of several.
        while done < len {
            let (at, left) = node.place(pos + done as u64).ok_or(Error::Corrupt)?;
            let part = (len - done).min(left as usize);
            if done == 0 && part == len {
                return self.read_into(at, page, len);
            }
            node.fs.cache.read(at, &mut page.bytes_mut()[done..done + part])?;
            done += part;
        }
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
        let place = node.place(pos).filter(|_| !node.interleaved);
        let max = pages.len().min(device.info().max_segments.clamp(1, radix_vfs::cache::READ_RUN));
        // Whole pages in one extent, from a whole block of the device.
        let n = match place {
            Some((at, left)) if at.is_multiple_of(unit) => {
                let in_file = size.saturating_sub(pos).div_ceil(PAGE);
                let in_extent = left / PAGE;
                (max as u64).min(in_file).min(in_extent) as usize
            }
            _ => 0,
        };
        if n < 2 {
            let page = pages.first_mut().ok_or(Error::Io)?;
            self.read_page(index, page, page_len(index, size))?;
            return Ok(1);
        }
        let Some((at, _)) = place else { return Err(Error::Io) };
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

impl Node for IsoNode {
    fn stat(&self) -> Stat {
        let a = &self.attributes;
        let modified = a.modified.unwrap_or(self.recorded);
        Stat {
            kind: self.kind,
            size: self.size,
            allocated: self
                .extents
                .iter()
                .map(|&(_, len)| len.div_ceil(self.fs.block) * self.fs.block)
                .sum(),
            id: self.id,
            created: a.created.unwrap_or(modified),
            modified,
            accessed: a.accessed.unwrap_or(modified),
            changed: a.changed.unwrap_or(modified),
            links: if a.links > 0 { a.links } else { 1 },
            device: if matches!(self.kind, Kind::CharDevice | Kind::BlockDevice) {
                a.device.unwrap_or(0)
            } else {
                0
            },
        }
    }

    fn kind(&self) -> Kind {
        self.kind
    }

    fn read_only(&self) -> bool {
        true
    }

    fn lookup(&self, name: &str) -> Result<NodeRef> {
        self.is_dir()?;
        let (start, size) = self.dir_data();
        // A plain volume's names are shown in lower case, and found in
        // any case, as on Linux.
        let plain = self.fs.tree == Tree::Plain;
        let mut at = None;
        self.fs.scan(start, size, 0, &mut |found| {
            let same =
                if plain { found.name.eq_ignore_ascii_case(name) } else { found.name == name };
            if same {
                at = Some(found.at);
            }
            Ok(!same)
        })?;
        let at = at.ok_or(Error::NotFound)?;
        Ok(Inner::node_at(&self.fs, at)?)
    }

    fn access(&self) -> Access {
        let a = &self.attributes;
        match a.mode {
            Some(mode) => Access { owner: a.owner, group: a.group, mode: (mode & 0o7777) as u16 },
            // Without Rock Ridge, everyone may read and run everything,
            // as on Linux: any file may be a program.
            None => Access { owner: 0, group: 0, mode: 0o555 },
        }
    }

    fn read_link(&self, out: &mut dyn Out) -> Result<usize> {
        let target = self.target.as_ref().ok_or(Error::NotLink)?;
        let len = target.len().min(out.len());
        out.put(0, &target[..len])?;
        Ok(target.len())
    }

    fn list(&self, cursor: u64, f: ListFn<'_>) -> Result<()> {
        self.is_dir()?;
        let (start, size) = self.dir_data();
        self.fs.scan(start, size, cursor, &mut |found| Ok(f(found.next, found.name, found.kind)))
    }

    fn read(&self, offset: u64, out: &mut dyn Out) -> Result<usize> {
        match &self.cache {
            Some(cache) => cache.read(offset, out, &FileBacking(self)),
            None if self.kind == Kind::Directory => Err(Error::IsDirectory),
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
