// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! The filesystem: the volume's descriptors, the tree it reads, its
//! directories, and the nodes in use.
//!
//! Directories are read through a [`BlockCache`] of the device, a record
//! at a time; a node is made for a file when it is looked up, from its
//! records (more than one for a file of several extents), and kept in a
//! table while it is in use, so that each file has one node.

use alloc::sync::Weak;
use core::fmt;

use radix_block::DeviceRef;
use radix_block::cache::BlockCache;
use radix_sync::Mutex;
use radix_vfs::mem::{self, Alloc, KArc, KVec};
use radix_vfs::{Error, FileSystem, FsInfo, FsKind, Kind, NodeRef, Result};

use crate::format::{
    self, Descriptor, FIRST_DESCRIPTOR, MAX_DESCRIPTORS, NameBuf, RECORD_HEADER, RECORD_MAX,
    Record, SECTOR, Volume, flag,
};
use crate::node::{IsoNode, Loaded};
use crate::rock::{self, Attributes, Collect, Target};

/// The most continuation areas followed for one record: a loop in a
/// damaged disc's ends there.
const MAX_CONTINUATIONS: usize = 8;
/// The most extents of one file.
pub(crate) const MAX_EXTENTS: usize = 64;

/// Which of the volume's trees is read.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Tree {
    /// The primary one, with Rock Ridge: each System Use area's first
    /// bytes skipped.
    Rock(u8),
    /// Joliet's.
    Joliet,
    /// The primary one, plain.
    Plain,
}

/// What the filesystem's nodes share.
pub(crate) struct Inner {
    pub(crate) cache: BlockCache,
    /// Its logical block size.
    pub(crate) block: u64,
    pub(crate) blocks: u64,
    pub(crate) tree: Tree,
    label: NameBuf,
    nodes: Mutex<KVec<(u64, Weak<IsoNode, Alloc>)>>,
}

/// An ISO 9660 volume, mounted read-only.
pub struct IsoFs {
    inner: KArc<Inner>,
    root: KArc<IsoNode>,
}

/// One entry of a directory, as a scan finds it.
pub(crate) struct Found<'a> {
    /// Where its first record is on the device, in bytes: its node's
    /// number.
    pub(crate) at: u64,
    /// Where the scan goes on after it, in bytes into the directory.
    pub(crate) next: u64,
    pub(crate) name: &'a str,
    pub(crate) kind: Kind,
}

impl IsoFs {
    /// Mounts the ISO 9660 volume on `device`. Fails with
    /// [`Error::UnknownFilesystem`] if it holds none, and with
    /// [`Error::ReadOnly`] when asked to write.
    pub fn mount(device: DeviceRef, read_only: bool) -> Result<KArc<IsoFs>> {
        let cache = BlockCache::new(device);
        let (primary, joliet) = descriptors(&cache)?;
        if !read_only {
            return Err(Error::ReadOnly);
        }
        let block = u64::from(primary.block_size);
        let rock = rock_indicator(&cache, u64::from(primary.root_extent) * block);
        let (tree, volume) = match (rock, joliet) {
            (Some(skip), _) => (Tree::Rock(skip), primary),
            (None, Some(joliet)) => (Tree::Joliet, joliet),
            (None, None) => (Tree::Plain, primary),
        };
        let mut label = NameBuf::default();
        let raw = &volume.label;
        match joliet {
            Some(joliet) => format::joliet_name(&joliet.label, &mut label),
            None => label.push_bytes(raw),
        }
        label.trim_end();
        let inner = mem::arc(Inner {
            cache,
            block: u64::from(volume.block_size),
            blocks: u64::from(volume.blocks),
            tree,
            label,
            nodes: Mutex::new(mem::vec()),
        })?;
        let root = IsoNode::root(&inner, &volume)?;
        Ok(mem::arc(IsoFs { inner, root })?)
    }
}

/// Whether the root directory's own record, at byte `at`, says Rock
/// Ridge is used: the bytes each System Use area skips if so.
#[inline(never)]
fn rock_indicator(cache: &BlockCache, at: u64) -> Option<u8> {
    let mut first = [0u8; RECORD_MAX];
    let len = read_record(cache, at, &mut first).ok()?;
    rock::indicator(format::record(&first[..len])?.system_use)
}

/// The volume's primary descriptor, and its Joliet one if it has one.
#[inline(never)]
fn descriptors(cache: &BlockCache) -> Result<(Volume, Option<Volume>)> {
    let size = cache.device().info().sectors * radix_block::SECTOR;
    let mut primary = None;
    let mut joliet: Option<(Volume, u8)> = None;
    let mut sector = zeroed(SECTOR as usize)?;
    for n in FIRST_DESCRIPTOR..FIRST_DESCRIPTOR + MAX_DESCRIPTORS {
        if (n + 1) * SECTOR > size {
            break;
        }
        cache.read(n * SECTOR, &mut sector)?;
        match format::descriptor(&sector) {
            None => break,
            Some(Descriptor::Terminator) => break,
            Some(Descriptor::Primary(volume)) => primary = primary.or(Some(volume)),
            Some(Descriptor::Joliet(volume, level)) => {
                if joliet.is_none_or(|(_, best)| level > best) {
                    joliet = Some((volume, level));
                }
            }
            Some(Descriptor::Other) => {}
        }
    }
    let primary = primary.ok_or(Error::UnknownFilesystem)?;
    let good = |v: &Volume| {
        matches!(v.block_size, 512 | 1024 | 2048)
            && v.root_size > 0
            && u64::from(v.root_extent) * u64::from(v.block_size) < size
    };
    if !good(&primary) {
        return Err(Error::Corrupt);
    }
    Ok((primary, joliet.map(|(v, _)| v).filter(good)))
}

/// `len` zero bytes on the heap.
fn zeroed(len: usize) -> Result<KVec<u8>> {
    let mut bytes = mem::vec_with_capacity(len)?;
    while bytes.len() < len {
        // Room was made above.
        let _ = bytes.push_within_capacity(0);
    }
    Ok(bytes)
}

/// Reads the record at byte `at` into `out`; returns its length, 0 where
/// a block's records end.
pub(crate) fn read_record(
    cache: &BlockCache,
    at: u64,
    out: &mut [u8; RECORD_MAX],
) -> Result<usize> {
    cache.read(at, &mut out[..1])?;
    let len = usize::from(out[0]);
    if len == 0 {
        return Ok(0);
    }
    if len < RECORD_HEADER + 1 {
        return Err(Error::Corrupt);
    }
    cache.read(at, &mut out[..len])?;
    Ok(len)
}

impl Inner {
    /// The System Use entries of `record`, skipped as the tree says, and
    /// those of the continuation areas it leads to, given to `f`.
    pub(crate) fn system_use(
        &self,
        record: &Record<'_>,
        f: &mut dyn FnMut([u8; 2], &[u8]) -> bool,
    ) -> Result<()> {
        let Tree::Rock(skip) = self.tree else { return Ok(()) };
        let area = record.system_use.get(usize::from(skip)..).unwrap_or(&[]);
        let mut next = rock::walk(area, f);
        let mut followed = 0;
        while let Some(more) = next {
            followed += 1;
            if followed > MAX_CONTINUATIONS || more.len as u64 > self.block {
                return Err(Error::Corrupt);
            }
            let mut bytes = zeroed(more.len as usize)?;
            self.cache
                .read(u64::from(more.block) * self.block + u64::from(more.offset), &mut bytes)?;
            next = rock::walk(&bytes, f);
        }
        Ok(())
    }

    /// What `record`'s Rock Ridge entries say, its name into `name` and a
    /// link's target into `target` if given.
    pub(crate) fn attributes(
        &self,
        record: &Record<'_>,
        name: &mut NameBuf,
        target: Option<&mut Target>,
    ) -> Result<Attributes> {
        let mut collect = Collect::new(name, target);
        self.system_use(record, &mut |signature, entry| {
            collect.entry(signature, entry);
            true
        })?;
        let attributes = collect.attributes;
        if !attributes.named {
            self.plain(record, collect.name);
        }
        Ok(attributes)
    }

    /// `record`'s name as its tree has it, without Rock Ridge's.
    fn plain(&self, record: &Record<'_>, name: &mut NameBuf) {
        match self.tree {
            Tree::Joliet => format::joliet_name(record.name, name),
            Tree::Rock(_) | Tree::Plain => format::plain_name(record.name, name),
        }
    }

    /// The kind of file `record` is, as its flags and `attributes` say.
    pub(crate) fn kind_of(record: &Record<'_>, attributes: &Attributes) -> Kind {
        if attributes.child.is_some() {
            return Kind::Directory;
        }
        if let Some(mode) = attributes.mode {
            return match mode & rock::mode::TYPE {
                rock::mode::DIRECTORY => Kind::Directory,
                rock::mode::LINK => Kind::Symlink,
                rock::mode::CHAR => Kind::CharDevice,
                rock::mode::BLOCK => Kind::BlockDevice,
                rock::mode::FIFO => Kind::Pipe,
                rock::mode::SOCKET => Kind::Socket,
                _ if record.is_dir() => Kind::Directory,
                _ => Kind::File,
            };
        }
        if attributes.link {
            Kind::Symlink
        } else if record.is_dir() {
            Kind::Directory
        } else {
            Kind::File
        }
    }

    /// Calls `f` with each entry of the directory whose data is `size`
    /// bytes from byte `start`, from `from` bytes into it, but its own and
    /// its parent's, until `f` returns false. A file of several extents
    /// is one entry; associated files and relocated directories (listed
    /// where they belong) are left out.
    pub(crate) fn scan(
        &self,
        start: u64,
        size: u64,
        from: u64,
        f: &mut dyn FnMut(&Found<'_>) -> Result<bool>,
    ) -> Result<()> {
        let mut buf = [0u8; RECORD_MAX];
        let mut name = NameBuf::default();
        let mut pos = from;
        // A file whose first record came before.
        let mut open: Option<u64> = None;
        while pos < size {
            let len = read_record(&self.cache, start + pos, &mut buf)?;
            if len == 0 {
                // To the next block.
                pos = (pos / self.block + 1) * self.block;
                continue;
            }
            let at = start + pos;
            pos += len as u64;
            // A record may not cross a block.
            if (at % self.block) + len as u64 > self.block {
                return Err(Error::Corrupt);
            }
            let record = format::record(&buf[..len]).ok_or(Error::Corrupt)?;
            if record.is_dot() {
                continue;
            }
            let first = *open.get_or_insert(at);
            if record.flags & flag::MULTI_EXTENT != 0 {
                continue;
            }
            open = None;
            if record.flags & flag::ASSOCIATED != 0 {
                continue;
            }
            // The name and kind are the first record's.
            let record = if first == at {
                record
            } else {
                let len = read_record(&self.cache, first, &mut buf)?;
                format::record(&buf[..len]).ok_or(Error::Corrupt)?
            };
            let attributes = self.attributes(&record, &mut name, None)?;
            if attributes.relocated {
                continue;
            }
            let Some(text) = name.name() else { continue };
            let found = Found {
                at: first,
                next: pos,
                name: text,
                kind: Self::kind_of(&record, &attributes),
            };
            if !f(&found)? {
                break;
            }
        }
        Ok(())
    }

    /// The node in use for the file whose first record is at byte `at`.
    fn live(&self, at: u64) -> Option<KArc<IsoNode>> {
        let nodes = self.nodes.lock();
        let i = nodes.binary_search_by_key(&at, |(k, _)| *k).ok()?;
        nodes[i].1.upgrade()
    }

    /// The node for the file whose first record is at byte `at`: the one
    /// in use, or a new one.
    pub(crate) fn node_at(this: &KArc<Inner>, at: u64) -> Result<KArc<IsoNode>> {
        if let Some(node) = this.live(at) {
            return Ok(node);
        }
        let loaded = Loaded::read(this, at)?;
        let node = IsoNode::new(this, at, loaded)?;
        let weak = KArc::downgrade(&node);
        let mut nodes = this.nodes.lock();
        match nodes.binary_search_by_key(&at, |(k, _)| *k) {
            Ok(i) => match nodes[i].1.upgrade() {
                // Another thread made it meanwhile: its node is the one.
                Some(other) => return Ok(other),
                None => nodes[i].1 = weak,
            },
            Err(_) => {
                if nodes.len() == nodes.capacity() {
                    nodes.retain(|(_, weak)| weak.strong_count() > 0);
                }
                let i = nodes.binary_search_by_key(&at, |(k, _)| *k).unwrap_or_else(|i| i);
                mem::insert(&mut nodes, i, (at, weak))?;
            }
        }
        Ok(node)
    }

    /// The nodes in use, for shrinking their caches.
    fn live_nodes(&self) -> Result<KVec<KArc<IsoNode>>> {
        let nodes = self.nodes.lock();
        let mut live = mem::vec_with_capacity(nodes.len())?;
        for (_, weak) in nodes.iter() {
            if let Some(node) = weak.upgrade() {
                let _ = live.push_within_capacity(node);
            }
        }
        Ok(live)
    }
}

impl FileSystem for IsoFs {
    fn kind(&self) -> FsKind {
        FsKind::Iso9660
    }

    fn root(&self) -> NodeRef {
        self.root.clone()
    }

    fn info(&self) -> FsInfo {
        FsInfo {
            block_size: self.inner.block as u32,
            blocks: self.inner.blocks,
            free: 0,
            files: 0,
            free_files: 0,
        }
    }

    fn read_only(&self) -> bool {
        true
    }

    fn label(&self, out: &mut dyn fmt::Write) -> fmt::Result {
        out.write_str(self.inner.label.name().unwrap_or(""))
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
