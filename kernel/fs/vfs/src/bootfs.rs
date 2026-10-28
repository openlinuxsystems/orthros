// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! The boot archive as a read-only filesystem, mounted at `/system`.
//!
//! The archive (`radix-bootfs`) is checked whole before it is mounted;
//! then a node is made for each of its entries, once, so that each file
//! has one node (the mount table tells mount points by their node), and
//! file reads copy straight from the archive's bytes. Each directory knows
//! its entries, in the order of their names, as the archive has them, so
//! a lookup is a binary search among them, not a look at every file.

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
    /// The nodes' indices grouped by directory, each group in the order
    /// of their names: a directory's entries are those in its
    /// [`BootNode::entries`] range.
    entries: KVec<u32>,
    size: u64,
}

/// The boot archive, mounted.
pub struct BootFs {
    shared: KArc<Shared>,
    root: KArc<BootNode>,
}

/// A file, directory or symbolic link of the archive.
pub struct BootNode {
    shared: Weak<Shared, Alloc>,
    /// Its entry; none for the root.
    entry: Option<Entry<'static>>,
    /// Its name, the last of its path's; empty for the root.
    name: &'static str,
    /// Where its entries are in [`Shared::entries`], if it is a directory.
    entries: core::ops::Range<u32>,
    id: u64,
}

impl BootFs {
    /// Mounts the archive in `bytes`.
    pub fn new(bytes: &'static [u8]) -> Result<KArc<BootFs>> {
        let archive: Archive<'static> = radix_bootfs::open(bytes).map_err(|_| Error::Corrupt)?;
        let count = archive.len();
        let mut entries: KVec<Entry<'static>> = mem::vec_with_capacity(count)?;
        for entry in archive.entries() {
            // Room was made for every entry.
            let _ = entries.push_within_capacity(entry);
        }
        // Each entry's directory, as a slot: 0 for the root, 1 + its index
        // for an entry. The archive is in the order of its paths' names, a
        // directory just before what it holds, so an entry's directory is
        // the last entry one name shorter, if its path begins with that
        // one's (an archive may leave a directory out: what it holds is
        // then in the root, as when the archive was searched).
        let mut parents: KVec<u32> = mem::vec_with_capacity(count)?;
        let mut last = [None::<u32>; radix_bootfs::MAX_PATH / 2 + 1];
        for (i, entry) in entries.iter().enumerate() {
            let depth = entry.path().components().count();
            let parent =
                depth.checked_sub(2).and_then(|up| last.get(up).copied().flatten()).filter(|&j| {
                    let above = entries[j as usize].path();
                    above.components().zip(entry.path().components()).all(|(a, b)| a == b)
                });
            if let Some(slot) = depth.checked_sub(1).and_then(|at| last.get_mut(at)) {
                *slot = Some(i as u32);
            }
            let _ = parents.push_within_capacity(parent.map_or(0, |j| j + 1));
        }
        // Where each directory's group starts, counted, then filled in the
        // archive's order, which is the order of the names within a group.
        let mut starts: KVec<u32> = mem::vec_with_capacity(count + 2)?;
        for _ in 0..count + 2 {
            let _ = starts.push_within_capacity(0);
        }
        for &parent in &parents {
            starts[parent as usize + 2] += 1;
        }
        for slot in 2..count + 2 {
            starts[slot] += starts[slot - 1];
        }
        // `starts[slot + 1]` is where the group of `slot` starts; it moves
        // along the group as the group is filled, ending where the group
        // of `slot + 1` starts.
        let mut grouped: KVec<u32> = mem::vec_with_capacity(count)?;
        for _ in 0..count {
            let _ = grouped.push_within_capacity(0);
        }
        for (i, &parent) in parents.iter().enumerate() {
            let at = &mut starts[parent as usize + 1];
            grouped[*at as usize] = i as u32;
            *at += 1;
        }
        // So the group of `slot` is now from `starts[slot]` on.
        let range = |slot: usize| starts[slot]..starts[slot + 1];
        let shared = mem::arc(Shared {
            nodes: radix_sync::Once::new(),
            entries: grouped,
            size: bytes.len() as u64,
        })?;
        let mut nodes = mem::vec_with_capacity(count)?;
        for (i, entry) in entries.iter().enumerate() {
            let node = mem::arc(BootNode {
                shared: KArc::downgrade(&shared),
                entry: Some(*entry),
                name: entry.path().file_name(),
                entries: if entry.kind == EntryKind::Directory { range(i + 1) } else { 0..0 },
                id: i as u64 + 2,
            })?;
            let _ = nodes.push_within_capacity(node);
        }
        let _ = shared.nodes.set(nodes);
        let root = mem::arc(BootNode {
            shared: KArc::downgrade(&shared),
            entry: None,
            name: "",
            entries: range(0),
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
        // Its entries and its root, all there is room for.
        let nodes = self.shared.nodes.get().map_or(0, |nodes| nodes.len() as u64);
        FsInfo {
            block_size: 512,
            blocks: self.shared.size.div_ceil(512),
            free: 0,
            files: nodes + 1,
            free_files: 0,
        }
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

    fn node_kind(&self) -> Kind {
        match self.entry.map(|e| e.kind) {
            None | Some(EntryKind::Directory) => Kind::Directory,
            Some(EntryKind::File) => Kind::File,
            Some(EntryKind::Symlink) => Kind::Symlink,
        }
    }

    /// Calls `f` with the nodes of the archive and this directory's
    /// entries among them, in the order of their names.
    fn with_entries<R>(&self, f: impl FnOnce(&[KArc<BootNode>], &[u32]) -> R) -> Result<R> {
        if !self.is_dir() {
            return Err(Error::NotDirectory);
        }
        let shared = self.shared.upgrade().ok_or(Error::NotFound)?;
        let Some(nodes) = shared.nodes.get() else { return Ok(f(&[], &[])) };
        let range = self.entries.start as usize..self.entries.end as usize;
        Ok(f(nodes, shared.entries.get(range).unwrap_or(&[])))
    }

    /// Calls `f` with each entry directly inside this directory.
    fn children(&self, mut f: impl FnMut(&KArc<BootNode>) -> bool) -> Result<()> {
        self.with_entries(|nodes, entries| {
            for node in entries.iter().filter_map(|&i| nodes.get(i as usize)) {
                if !f(node) {
                    break;
                }
            }
        })
    }

    fn name(&self) -> &'static str {
        self.name
    }
}

impl Node for BootNode {
    fn stat(&self) -> Stat {
        // A link's data is its target, and its size the target's length.
        let (kind, size, mtime) = match self.entry {
            Some(e) if e.kind == EntryKind::Directory => (Kind::Directory, 0, e.mtime),
            Some(e) => (self.node_kind(), e.data.len() as u64, e.mtime),
            None => (Kind::Directory, 0, 0),
        };
        let time = mtime.saturating_mul(1_000_000_000);
        Stat {
            kind,
            size,
            allocated: size,
            id: self.id,
            created: time,
            modified: time,
            accessed: time,
            changed: time,
            links: 1,
            device: 0,
        }
    }

    fn kind(&self) -> Kind {
        self.node_kind()
    }

    fn lookup(&self, name: &str) -> Result<NodeRef> {
        let found = self.with_entries(|nodes, entries| {
            let name_of = |&i: &u32| nodes.get(i as usize).map_or("", |node| node.name);
            let at = entries.binary_search_by(|i| name_of(i).cmp(name)).ok()?;
            let node: NodeRef = nodes.get(*entries.get(at)? as usize)?.clone();
            Some(node)
        })?;
        found.ok_or(Error::NotFound)
    }

    fn create(&self, _name: &str, _kind: Kind) -> Result<NodeRef> {
        if self.is_dir() { Err(Error::ReadOnly) } else { Err(Error::NotDirectory) }
    }

    fn make_link(&self, _name: &str, _target: &str) -> Result<NodeRef> {
        if self.is_dir() { Err(Error::ReadOnly) } else { Err(Error::NotDirectory) }
    }

    fn read_link(&self, out: &mut dyn Out) -> Result<usize> {
        match self.entry {
            Some(e) if e.kind == EntryKind::Symlink => {
                let room = out.len().min(e.data.len());
                out.put(0, &e.data[..room])?;
                Ok(e.data.len())
            }
            _ => Err(Error::NotLink),
        }
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
            i <= cursor || f(i, node.name(), node.node_kind())
        })
    }

    fn read(&self, offset: u64, out: &mut dyn Out) -> Result<usize> {
        match self.entry {
            Some(e) if e.kind == EntryKind::File => read_bytes(e.data, offset, out),
            Some(e) if e.kind == EntryKind::Symlink => Err(Error::NotSupported),
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
