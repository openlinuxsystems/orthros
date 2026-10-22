// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! The boot archive as a read-only filesystem, mounted at `/system`.
//!
//! The archive (`radix-bootfs`) is checked whole before it is mounted;
//! then a node is made for each of its entries, once, so that each file
//! has one node (the mount table tells mount points by their node), and
//! file reads copy straight from the archive's bytes.

use alloc::sync::Weak;
use core::any::Any;

use radix_bootfs::{Archive, Entry, Kind as EntryKind};

use crate::mem::{self, Alloc, KArc, KVec};
use crate::synthetic::read_bytes;
use crate::{
    Error, FileSystem, FsInfo, FsKind, In, Kind, ListFn, Node, NodeRef, Out, Result, Stat,
};

/// What the nodes of one archive share.
struct Shared {
    nodes: radix_sync::Once<KVec<KArc<BootNode>>>,
    size: u64,
}

/// The boot archive, mounted.
pub struct BootFs {
    shared: KArc<Shared>,
    root: KArc<BootNode>,
}

/// A file or directory of the archive.
pub struct BootNode {
    shared: Weak<Shared, Alloc>,
    /// Its entry; none for the root.
    entry: Option<Entry<'static>>,
    /// The index of its directory's node; none for the root and what is
    /// in the root.
    parent: Option<usize>,
    id: u64,
}

impl BootFs {
    /// Mounts the archive in `bytes`.
    pub fn new(bytes: &'static [u8]) -> Result<KArc<BootFs>> {
        let archive: Archive<'static> = radix_bootfs::open(bytes).map_err(|_| Error::Corrupt)?;
        let shared = mem::arc(Shared { nodes: radix_sync::Once::new(), size: bytes.len() as u64 })?;
        let mut nodes = mem::vec_with_capacity(archive.len())?;
        let mut entries: KVec<Entry<'static>> = mem::vec_with_capacity(archive.len())?;
        for entry in archive.entries() {
            // Room was made for every entry.
            let _ = entries.push_within_capacity(entry);
        }
        for (i, entry) in entries.iter().enumerate() {
            let depth = entry.path().components().count();
            // The archive is in order, a directory before what it holds,
            // so the parent is an earlier entry one component shorter.
            let parent = (0..i).rev().find(|&j| {
                let p = entries[j].path();
                p.components().count() + 1 == depth
                    && p.components().zip(entry.path().components()).all(|(a, b)| a == b)
            });
            let node = mem::arc(BootNode {
                shared: KArc::downgrade(&shared),
                entry: Some(*entry),
                parent,
                id: i as u64 + 2,
            })?;
            let _ = nodes.push_within_capacity(node);
        }
        let _ = shared.nodes.set(nodes);
        let root = mem::arc(BootNode {
            shared: KArc::downgrade(&shared),
            entry: None,
            parent: None,
            id: 1,
        })?;
        Ok(mem::arc(BootFs { shared, root })?)
    }
}

impl FileSystem for BootFs {
    fn kind(&self) -> FsKind {
        FsKind::Boot
    }

    fn root(&self) -> NodeRef {
        self.root.clone()
    }

    fn info(&self) -> FsInfo {
        FsInfo { block_size: 512, blocks: self.shared.size.div_ceil(512), free: 0 }
    }

    fn read_only(&self) -> bool {
        true
    }

    fn light_drops(&self) -> bool {
        true
    }
}

impl BootNode {
    fn is_dir(&self) -> bool {
        self.entry.is_none_or(|e| e.kind == EntryKind::Directory)
    }

    /// The index of this node among the archive's, none for the root.
    fn index(&self) -> Option<usize> {
        self.entry.map(|_| self.id as usize - 2)
    }

    /// Calls `f` with each entry directly inside this directory.
    fn children(&self, mut f: impl FnMut(&KArc<BootNode>) -> bool) -> Result<()> {
        if !self.is_dir() {
            return Err(Error::NotDirectory);
        }
        let shared = self.shared.upgrade().ok_or(Error::NotFound)?;
        let Some(nodes) = shared.nodes.get() else { return Ok(()) };
        let me = self.index();
        for node in nodes.iter().filter(|node| node.parent == me) {
            if !f(node) {
                break;
            }
        }
        Ok(())
    }

    fn name(&self) -> &'static str {
        self.entry.map_or("", |e| e.path().file_name())
    }
}

impl Node for BootNode {
    fn stat(&self) -> Stat {
        let (kind, size, mtime) = match self.entry {
            Some(e) if e.kind == EntryKind::File => (Kind::File, e.data.len() as u64, e.mtime),
            Some(e) => (Kind::Directory, 0, e.mtime),
            None => (Kind::Directory, 0, 0),
        };
        let time = mtime.saturating_mul(1_000_000_000);
        Stat { kind, size, allocated: size, id: self.id, created: time, modified: time }
    }

    fn lookup(&self, name: &str) -> Result<NodeRef> {
        let mut found = None;
        self.children(|node| {
            if node.name() == name {
                found = Some(node.clone());
                false
            } else {
                true
            }
        })?;
        match found {
            Some(node) => Ok(node),
            None => Err(Error::NotFound),
        }
    }

    fn create(&self, _name: &str, _kind: Kind) -> Result<NodeRef> {
        if self.is_dir() { Err(Error::ReadOnly) } else { Err(Error::NotDirectory) }
    }

    fn remove(&self, _name: &str) -> Result<()> {
        if self.is_dir() { Err(Error::ReadOnly) } else { Err(Error::NotDirectory) }
    }

    fn rename(&self, _from: &str, _to_dir: &dyn Node, _to: &str) -> Result<()> {
        Err(Error::ReadOnly)
    }

    fn list(&self, cursor: u64, f: ListFn<'_>) -> Result<()> {
        let mut i = 0u64;
        self.children(|node| {
            i += 1;
            let kind = if node.is_dir() { Kind::Directory } else { Kind::File };
            i <= cursor || f(i, node.name(), kind)
        })
    }

    fn read(&self, offset: u64, out: &mut dyn Out) -> Result<usize> {
        match self.entry {
            Some(e) if e.kind == EntryKind::File => read_bytes(e.data, offset, out),
            _ => Err(Error::IsDirectory),
        }
    }

    fn write(&self, _offset: u64, _data: &dyn In) -> Result<usize> {
        if self.is_dir() { Err(Error::IsDirectory) } else { Err(Error::ReadOnly) }
    }

    fn set_size(&self, _size: u64) -> Result<()> {
        if self.is_dir() { Err(Error::IsDirectory) } else { Err(Error::ReadOnly) }
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}
