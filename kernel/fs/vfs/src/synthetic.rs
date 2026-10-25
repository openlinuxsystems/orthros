// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Files the kernel makes up: procfs, sysfs and devfs.
//!
//! - [`Generated`] is a file of plain text written by a function when the
//!   file is opened: each handle reads the snapshot it was opened with
//!   ([`Snapshot`]), so a program reading a long file in pieces sees one
//!   consistent text, as with Linux's `seq_file`.
//! - [`FixedDir`] is a directory of entries the kernel adds (sysfs's tree,
//!   devfs); [`ListedDir`] is one whose entries a [`Listing`] works out
//!   each time (procfs's one directory per process).
//! - [`Null`], [`Zero`] and [`Random`] are the character devices of
//!   `/dev`, and [`BlockNode`] a block device, which `mount` takes.
//! - [`SyntheticFs`] makes a tree of them a read-only filesystem.
//!
//! Text formats follow radix-proc(5): one `key: value` per line, so that
//! programs and people read them alike.

use core::any::Any;
use core::fmt;

use radix_sync::Mutex;

use crate::mem::{self, KArc, KBox, KVec};
use crate::{
    Error, FileSystem, FsInfo, FsKind, In, Kind, ListFn, Node, NodeRef, Out, Result, Stat, Text,
};

/// Writes a generated file's text.
pub type Generator = KBox<dyn Fn(&mut Text) -> fmt::Result + Send + Sync>;

/// A generated text file.
pub struct Generated {
    id: u64,
    generate: Generator,
}

impl Generated {
    /// A file whose text `generate` writes, numbered `id`.
    pub fn make(id: u64, generate: Generator) -> Result<NodeRef> {
        Ok(mem::arc(Generated { id, generate })?)
    }

    /// A file of `generate`'s text, from a plain function.
    pub fn from_fn(id: u64, generate: fn(&mut Text) -> fmt::Result) -> Result<NodeRef> {
        let generate: Generator = mem::boxed(generate)?;
        Generated::make(id, generate)
    }
}

impl Node for Generated {
    fn stat(&self) -> Stat {
        // As on Linux, a generated file's size is unknown until read.
        Stat {
            kind: Kind::File,
            size: 0,
            allocated: 0,
            id: self.id,
            created: 0,
            modified: 0,
            accessed: 0,
        }
    }

    fn open(&self) -> Result<Option<NodeRef>> {
        let mut text = Text::new();
        let written = (self.generate)(&mut text);
        if text.failed() {
            return Err(Error::NoMemory);
        }
        written.map_err(|_| Error::Io)?;
        let bytes = text.finish()?;
        Ok(Some(mem::arc(Snapshot { id: self.id, bytes, taken: crate::now() })?))
    }

    fn read(&self, offset: u64, out: &mut dyn Out) -> Result<usize> {
        // Read without a handle (no `open` first): a fresh snapshot.
        let mut text = Text::new();
        (self.generate)(&mut text).map_err(|_| Error::NoMemory)?;
        read_bytes(text.bytes(), offset, out)
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

/// A generated file's text as one handle saw it.
pub struct Snapshot {
    id: u64,
    bytes: KVec<u8>,
    taken: u64,
}

impl Node for Snapshot {
    fn stat(&self) -> Stat {
        Stat {
            kind: Kind::File,
            size: self.bytes.len() as u64,
            allocated: 0,
            id: self.id,
            created: self.taken,
            modified: self.taken,
            accessed: self.taken,
        }
    }

    fn read(&self, offset: u64, out: &mut dyn Out) -> Result<usize> {
        read_bytes(&self.bytes, offset, out)
    }

    fn write(&self, _offset: u64, _data: &dyn In) -> Result<usize> {
        Err(Error::ReadOnly)
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

/// Reads from `offset` of `bytes`.
pub fn read_bytes(bytes: &[u8], offset: u64, out: &mut dyn Out) -> Result<usize> {
    let Ok(offset) = usize::try_from(offset) else { return Ok(0) };
    if offset >= bytes.len() {
        return Ok(0);
    }
    let n = out.len().min(bytes.len() - offset);
    out.put(0, &bytes[offset..offset + n])?;
    Ok(n)
}

struct Fixed {
    name: KVec<u8>,
    node: NodeRef,
}

/// A directory whose entries the kernel adds.
pub struct FixedDir {
    id: u64,
    entries: Mutex<KVec<Fixed>>,
}

impl FixedDir {
    /// An empty directory numbered `id`.
    pub fn new(id: u64) -> Result<KArc<FixedDir>> {
        Ok(mem::arc(FixedDir { id, entries: Mutex::new(mem::vec()) })?)
    }

    /// Adds `node` as `name`, or replaces what had that name.
    pub fn add(&self, name: &str, node: NodeRef) -> Result<()> {
        crate::path::check_name(name)?;
        let name_bytes = mem::copy(name.as_bytes())?;
        let mut entries = self.entries.lock();
        match entries.binary_search_by(|e| e.name.as_slice().cmp(name.as_bytes())) {
            Ok(i) => entries[i].node = node,
            Err(at) => {
                mem::insert(&mut entries, at, Fixed { name: name_bytes, node })?;
            }
        }
        Ok(())
    }

    /// Adds a new empty directory `name` and returns it.
    pub fn add_dir(&self, name: &str, id: u64) -> Result<KArc<FixedDir>> {
        let dir = FixedDir::new(id)?;
        self.add(name, dir.clone())?;
        Ok(dir)
    }

    /// Takes `name` out.
    pub fn remove_entry(&self, name: &str) {
        let mut entries = self.entries.lock();
        if let Ok(i) = entries.binary_search_by(|e| e.name.as_slice().cmp(name.as_bytes())) {
            entries.remove(i);
        }
    }
}

fn kind_of(node: &NodeRef) -> Kind {
    node.kind()
}

impl Node for FixedDir {
    fn stat(&self) -> Stat {
        Stat {
            kind: Kind::Directory,
            size: 0,
            allocated: 0,
            id: self.id,
            created: 0,
            modified: 0,
            accessed: 0,
        }
    }

    fn lookup(&self, name: &str) -> Result<NodeRef> {
        let entries = self.entries.lock();
        match entries.binary_search_by(|e| e.name.as_slice().cmp(name.as_bytes())) {
            Ok(i) => Ok(entries[i].node.clone()),
            Err(_) => Err(Error::NotFound),
        }
    }

    fn create(&self, _name: &str, _kind: Kind) -> Result<NodeRef> {
        Err(Error::ReadOnly)
    }

    fn remove(&self, _name: &str) -> Result<()> {
        Err(Error::ReadOnly)
    }

    fn rename(&self, _from: &str, _to_dir: &dyn Node, _to: &str) -> Result<()> {
        Err(Error::ReadOnly)
    }

    fn list(&self, cursor: u64, f: ListFn<'_>) -> Result<()> {
        let entries = self.entries.lock();
        for (i, entry) in entries.iter().enumerate().skip(cursor as usize) {
            let name = core::str::from_utf8(&entry.name).unwrap_or("");
            if !f(i as u64 + 1, name, kind_of(&entry.node)) {
                break;
            }
        }
        Ok(())
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

/// The entries of a [`ListedDir`], worked out when asked for.
pub trait Listing: Send + Sync {
    /// Calls `f` with each entry, in an order that stays the same while
    /// the entries do, until it returns false.
    fn list(&self, f: &mut dyn FnMut(&str, Kind) -> bool);
    /// The entry `name`.
    fn lookup(&self, name: &str) -> Result<NodeRef>;
}

/// A directory whose entries a [`Listing`] gives.
pub struct ListedDir {
    id: u64,
    listing: KBox<dyn Listing>,
}

impl ListedDir {
    /// The directory numbered `id` of `listing`'s entries.
    pub fn make(id: u64, listing: KBox<dyn Listing>) -> Result<NodeRef> {
        Ok(mem::arc(ListedDir { id, listing })?)
    }
}

impl Node for ListedDir {
    fn stat(&self) -> Stat {
        Stat {
            kind: Kind::Directory,
            size: 0,
            allocated: 0,
            id: self.id,
            created: 0,
            modified: 0,
            accessed: 0,
        }
    }

    fn lookup(&self, name: &str) -> Result<NodeRef> {
        self.listing.lookup(name)
    }

    fn create(&self, _name: &str, _kind: Kind) -> Result<NodeRef> {
        Err(Error::ReadOnly)
    }

    fn remove(&self, _name: &str) -> Result<()> {
        Err(Error::ReadOnly)
    }

    fn rename(&self, _from: &str, _to_dir: &dyn Node, _to: &str) -> Result<()> {
        Err(Error::ReadOnly)
    }

    fn list(&self, cursor: u64, f: ListFn<'_>) -> Result<()> {
        let mut i = 0u64;
        self.listing.list(&mut |name, kind| {
            i += 1;
            if i <= cursor { true } else { f(i, name, kind) }
        });
        Ok(())
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

fn device_stat(id: u64, kind: Kind, size: u64) -> Stat {
    Stat { kind, size, allocated: 0, id, created: 0, modified: 0, accessed: 0 }
}

/// `/dev/null`: reads nothing, swallows writes.
pub struct Null(pub u64);

impl Node for Null {
    fn stat(&self) -> Stat {
        device_stat(self.0, Kind::CharDevice, 0)
    }

    fn read(&self, _offset: u64, _out: &mut dyn Out) -> Result<usize> {
        Ok(0)
    }

    fn write(&self, _offset: u64, data: &dyn In) -> Result<usize> {
        Ok(data.len())
    }

    fn set_size(&self, _size: u64) -> Result<()> {
        Ok(())
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

/// `/dev/zero`: reads zeros, swallows writes.
pub struct Zero(pub u64);

impl Node for Zero {
    fn stat(&self) -> Stat {
        device_stat(self.0, Kind::CharDevice, 0)
    }

    fn read(&self, _offset: u64, out: &mut dyn Out) -> Result<usize> {
        let n = out.len();
        out.zero(0, n)?;
        Ok(n)
    }

    fn write(&self, _offset: u64, data: &dyn In) -> Result<usize> {
        Ok(data.len())
    }

    fn set_size(&self, _size: u64) -> Result<()> {
        Ok(())
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

/// `/dev/random` and `/dev/urandom`: the kernel's random number
/// generator, through the function the kernel gives.
pub struct Random {
    /// Its number.
    pub id: u64,
    /// Fills a buffer with random bytes.
    pub fill: fn(&mut [u8]),
}

impl Node for Random {
    fn stat(&self) -> Stat {
        device_stat(self.id, Kind::CharDevice, 0)
    }

    fn read(&self, _offset: u64, out: &mut dyn Out) -> Result<usize> {
        let n = out.len().min(1 << 16);
        let mut chunk = [0u8; 256];
        let mut done = 0;
        while done < n {
            let len = (n - done).min(chunk.len());
            (self.fill)(&mut chunk[..len]);
            let put = out.put(done, &chunk[..len]);
            chunk.fill(0);
            put?;
            done += len;
        }
        Ok(n)
    }

    fn write(&self, _offset: u64, data: &dyn In) -> Result<usize> {
        // Writes add nothing to the generator, as on Linux without
        // privilege: they are accepted and ignored.
        Ok(data.len())
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

/// A block device in `/dev`.
pub struct BlockNode {
    id: u64,
    device: radix_block::DeviceRef,
}

impl BlockNode {
    /// The node numbered `id` of `device`.
    pub fn make(id: u64, device: radix_block::DeviceRef) -> Result<NodeRef> {
        Ok(mem::arc(BlockNode { id, device })?)
    }
}

impl Node for BlockNode {
    fn stat(&self) -> Stat {
        let size = self.device.info().sectors * radix_block::SECTOR;
        device_stat(self.id, Kind::BlockDevice, size)
    }

    fn kind(&self) -> Kind {
        Kind::BlockDevice
    }

    fn read(&self, _offset: u64, _out: &mut dyn Out) -> Result<usize> {
        // Raw access comes with the disk tools of Phase 10; until then a
        // block device is only something to mount.
        Err(Error::NotSupported)
    }

    fn write(&self, _offset: u64, _data: &dyn In) -> Result<usize> {
        Err(Error::NotSupported)
    }

    fn block_device(&self) -> Option<radix_block::DeviceRef> {
        Some(self.device.clone())
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

/// A read-only filesystem of synthetic nodes.
pub struct SyntheticFs {
    kind: FsKind,
    root: NodeRef,
}

impl SyntheticFs {
    /// The filesystem of kind `kind` whose root is `root`.
    pub fn make(kind: FsKind, root: NodeRef) -> Result<crate::FsRef> {
        Ok(mem::arc(SyntheticFs { kind, root })?)
    }
}

impl FileSystem for SyntheticFs {
    fn kind(&self) -> FsKind {
        self.kind
    }

    fn root(&self) -> NodeRef {
        self.root.clone()
    }

    fn info(&self) -> FsInfo {
        FsInfo::default()
    }

    fn read_only(&self) -> bool {
        // devfs's devices are written to, but its tree never changes.
        self.kind != FsKind::Dev
    }

    fn light_drops(&self) -> bool {
        true
    }
}
