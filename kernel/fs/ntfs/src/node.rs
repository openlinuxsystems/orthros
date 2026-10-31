// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! NTFS files, directories and links as VFS nodes.
//!
//! A node is made from its file's records when it is first looked up,
//! and kept while in use, one for each file however many names it has. A
//! file's data goes through its page cache, whose backing reads pages
//! straight from the clusters on the disk, a run of them in one request
//! where they lie in a row; holes, and what lies past what was written of
//! it, read as zeros. A compressed file's unit is read whole and
//! uncompressed ([`crate::lznt1`]), and its other pages go to the cache
//! with the one asked for.
//!
//! Some files' data Radix does not have: an encrypted (EFS) file's reads
//! fail with [`Error::Denied`], as on Linux, and those of a file whose
//! data is elsewhere (in the cloud, as OneDrive leaves a file not yet
//! downloaded, or in deduplication's chunk store) with
//! [`Error::NotSupported`], rather than read as zeros.

use core::any::Any;

use radix_block::mem::CachePage;
use radix_block::{PAGE_SIZE, Page, SECTOR, SegmentMut};
use radix_sync::Once;
use radix_vfs::cache::{Backing, FileCache, READ_RUN, page_len};
use radix_vfs::mem::{self, KArc, KVec};
use radix_vfs::{Access, Error, Kind, ListFn, Node, NodeRef, Out, Result, Stat};

use crate::fs::{Info, Inner, Loaded, Scratch, Stream, Value, records, zeroed};
use crate::index::{self, BLOCK_HEADER, FileName, NAME_MAX, ROOT_HEADER, attributes, namespace};
use crate::lznt1;
use crate::record::{self, attr_flags, fix_up, record_of, u32_at, u64_at};
use crate::reparse::{self, Target};
use crate::runs::HOLE;

const PAGE: u64 = PAGE_SIZE as u64;

/// The largest reparse point: Windows' own limit, 16 KiB.
const MAX_REPARSE: u64 = 16 << 10;

/// The largest Interix link Radix reads.
const MAX_INTERIX: u64 = 4096;

/// The largest compression unit Radix reads: 16 clusters of 4 KiB, all
/// Windows makes.
const MAX_UNIT: u64 = 64 << 10;

/// The deepest a link's directory may be below the root.
const MAX_DEPTH: usize = 256;

/// A file's data.
enum Data {
    /// In its record.
    Resident(KVec<u8>),
    /// In clusters.
    Clusters(Stream),
    /// Not here, or not readable by Radix: why its reads fail.
    Unreadable(Error),
}

/// A directory's index.
struct Dir {
    /// `$I30`'s root.
    root: KVec<u8>,
    /// Its blocks, if it has any.
    blocks: Option<Value>,
    /// Which blocks are in use.
    bitmap: KVec<u8>,
    /// How many blocks there are.
    count: u64,
}

/// An NTFS file, directory or link.
pub struct NtfsNode {
    pub(crate) fs: KArc<Inner>,
    /// Its base record's number.
    record: u64,
    pub(crate) kind: Kind,
    info: Info,
    links: u32,
    /// The directory of its first name.
    parent: u64,
    /// A file's or directory's size.
    size: u64,
    /// The bytes its clusters take.
    allocated: u64,
    data: Option<Data>,
    cache: Option<FileCache>,
    dir: Option<Dir>,
    /// A link's target, as the disk has it, and as a path once worked out.
    link: Option<Target>,
    path: Once<KVec<u8>>,
}

impl NtfsNode {
    /// The node of the file whose base record is `record`, from what was
    /// read of it.
    #[inline(never)]
    pub(crate) fn new(fs: KArc<Inner>, record: u64, loaded: Loaded) -> Result<KArc<NtfsNode>> {
        let Loaded { flags, info, links, parent, data, root, blocks, bitmap, reparse, .. } = loaded;
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
                file_data = Some(match unreadable {
                    Some(err) => Data::Unreadable(err),
                    None => data,
                });
            }
            Kind::Directory => {
                let root = root.ok_or(Error::Corrupt)?;
                // A directory's index is of names, collated as names are.
                if root.len() < ROOT_HEADER + 16 || u32_at(&root, 0) != record::kind::FILE_NAME {
                    return Err(Error::Corrupt);
                }
                let count = match &blocks {
                    Some(value @ Value::Clusters(_)) => {
                        value.size() / u64::from(vol.geo.index_size)
                    }
                    Some(Value::Resident(_)) => return Err(Error::Corrupt),
                    None => 0,
                };
                let bitmap = match &bitmap {
                    Some(value) => vol.read_whole(value, MAX_WHOLE_BITMAP)?,
                    None => mem::vec(),
                };
                size = blocks.as_ref().map_or(root.len() as u64, Value::size);
                allocated = blocks.as_ref().map_or(0, Value::size);
                dir = Some(Dir { root, blocks, bitmap, count });
            }
            _ => {}
        }
        let cache = file_data.as_ref().map(|_| FileCache::new(size, info.modified));
        Ok(mem::arc(NtfsNode {
            fs,
            record,
            kind,
            info,
            links,
            parent,
            size,
            allocated,
            data: file_data,
            cache,
            dir,
            link,
            path: Once::new(),
        })?)
    }

    fn dir(&self) -> Result<&Dir> {
        self.dir.as_ref().ok_or(Error::NotDirectory)
    }

    /// Drops up to `max` pages of its data, which can be read again.
    pub(crate) fn shrink(&self, max: usize) -> usize {
        self.cache.as_ref().map_or(0, |cache| cache.shrink(max, &FileBacking(self)))
    }

    /// Reads the index block at VCN `vcn` of `dir` into `buf`.
    fn read_block(&self, dir: &Dir, vcn: u64, buf: &mut [u8]) -> Result<()> {
        let geo = &self.fs.vol.geo;
        let blocks = dir.blocks.as_ref().ok_or(Error::Corrupt)?;
        let at = vcn.checked_mul(u64::from(geo.index_vcn_size())).ok_or(Error::Corrupt)?;
        if at.checked_add(buf.len() as u64).is_none_or(|end| end > blocks.size()) {
            return Err(Error::Corrupt);
        }
        self.fs.vol.read_value(blocks, at, buf)?;
        fix_up(buf, b"INDX")?;
        if u64_at(buf, 0x10) != vcn {
            return Err(Error::Corrupt);
        }
        Ok(())
    }

    /// Lists the entries of one node of its index, `bytes` with its header
    /// at `header`, node `node` of a listing's cursor, from offset `from`
    /// on. Returns false once `f` takes no more.
    fn list_node(
        &self,
        bytes: &[u8],
        header: usize,
        node: u64,
        from: usize,
        text: &mut [u8; NAME_MAX * 3],
        f: ListFn<'_>,
    ) -> Result<bool> {
        for entry in index::entries(bytes, header) {
            let entry = entry?;
            if entry.is_last() {
                break;
            }
            if entry.offset < from {
                continue;
            }
            let name = entry.name()?;
            // A short name repeats a long one; the system's files, and the
            // root's entry for itself, are left out, as Linux's drivers
            // leave them.
            if name.namespace() == namespace::DOS
                || record_of(entry.reference()) < records::FIRST_USER
            {
                continue;
            }
            let kind = self.kind_of(entry.reference(), &name);
            let len = utf8(name.units(), text);
            let next = index::cursor(node, entry.offset + entry.len());
            if !f(next, core::str::from_utf8(&text[..len]).unwrap_or(""), kind) {
                return Ok(false);
            }
        }
        Ok(true)
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
        let mut number = record_of(self.parent);
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

/// The most bitmap of a directory's index Radix reads: a bit for each
/// block, so room for 2^27 blocks.
const MAX_WHOLE_BITMAP: u64 = 16 << 20;

/// Puts the UTF-16 `units` in `out` as UTF-8; returns how many bytes.
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

/// Puts `name`'s UTF-16 units in `out`; returns how many.
fn units_of(name: &str, out: &mut [u16; NAME_MAX]) -> Result<usize> {
    let mut len = 0;
    for unit in name.encode_utf16() {
        *out.get_mut(len).ok_or(Error::NameTooLong)? = unit;
        len += 1;
    }
    Ok(len)
}

impl Node for NtfsNode {
    fn stat(&self) -> Stat {
        let size = match (&self.cache, self.kind) {
            (Some(cache), _) => cache.size(),
            (None, Kind::Symlink) => self.target().map_or(0, |t| t.len() as u64),
            (None, _) => self.size,
        };
        Stat {
            kind: self.kind,
            size,
            allocated: self.allocated,
            id: self.record,
            created: self.info.created,
            modified: self.info.modified,
            accessed: self.info.accessed,
            changed: self.info.changed,
            links: self.links.max(1),
            device: 0,
        }
    }

    fn kind(&self) -> Kind {
        self.kind
    }

    fn lookup(&self, name: &str) -> Result<NodeRef> {
        let dir = self.dir()?;
        let found = {
            let mut scratch = self.fs.scratch.lock();
            let Scratch { block, units, .. } = &mut **scratch;
            let n = units_of(name, units)?;
            let mut read = |vcn, buf: &mut [u8]| self.read_block(dir, vcn, buf);
            index::find(&dir.root, &self.fs.upcase, &units[..n], &mut read, block)?
        };
        let found = found.ok_or(Error::NotFound)?;
        Ok(Inner::node_for(&self.fs, found.reference)?)
    }

    // NTFS keeps Windows' permissions, which Radix does not map: as
    // Linux's drivers show them with the default umask of 022, everything
    // is user 0's and 0755, and a read-only file loses its write bits.
    fn access(&self) -> Access {
        let mut mode = if self.kind == Kind::Symlink { 0o777 } else { 0o755 };
        if self.kind == Kind::File && self.info.attributes & attributes::READ_ONLY != 0 {
            mode &= !0o222;
        }
        Access { owner: 0, group: 0, mode }
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

    fn list(&self, cursor: u64, f: ListFn<'_>) -> Result<()> {
        let dir = self.dir()?;
        let index_size = u64::from(self.fs.vol.geo.index_size);
        let vcn_size = u64::from(self.fs.vol.geo.index_vcn_size());
        let mut scratch = self.fs.scratch.lock();
        let Scratch { block, text, .. } = &mut **scratch;
        let mut node = index::cursor_node(cursor);
        let mut from = index::cursor_offset(cursor);
        if node == 0 {
            if !self.list_node(&dir.root, ROOT_HEADER, 0, from, text, f)? {
                return Ok(());
            }
            node = 1;
            from = 0;
        }
        while node - 1 < dir.count {
            let b = node - 1;
            let in_use =
                dir.bitmap.get((b / 8) as usize).is_some_and(|byte| byte & (1 << (b % 8)) != 0);
            if in_use {
                self.read_block(dir, b * index_size / vcn_size, block)?;
                if !self.list_node(block, BLOCK_HEADER, node, from, text, f)? {
                    return Ok(());
                }
            }
            node += 1;
            from = 0;
        }
        Ok(())
    }

    fn read(&self, offset: u64, out: &mut dyn Out) -> Result<usize> {
        let cache = self.cache.as_ref().ok_or(Error::IsDirectory)?;
        if let Some(Data::Unreadable(err)) = &self.data {
            return Err(*err);
        }
        cache.read(offset, out, &FileBacking(self))
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

/// A file's page cache backing: its clusters.
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
            vol.cache.read(vol.geo.cluster_offset(lcn), &mut packed[at..at + (n * cs) as usize])?;
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
        match &self.0.data {
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
        let Some(Data::Clusters(stream)) = &self.0.data else { return single(pages, more) };
        if stream.flags & attr_flags::COMPRESSED != 0 {
            return single(pages, more);
        }
        let vol = &self.0.fs.vol;
        let cs = u64::from(vol.geo.cluster_size);
        let pos = index * PAGE;
        let Some((lcn, n)) = stream.runs.map(pos / cs) else { return single(pages, more) };
        // Whole pages in a row on the disk, all written, all in the file.
        let in_row = (n * cs - pos % cs) / PAGE;
        let written = stream.initialized.min(size).saturating_sub(pos) / PAGE;
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

    // The volume is read-only: nothing grows or shrinks.
    fn reserve(&self, _size: u64) -> Result<()> {
        Err(Error::ReadOnly)
    }

    fn truncate(&self, _size: u64) -> Result<()> {
        Err(Error::ReadOnly)
    }
}
