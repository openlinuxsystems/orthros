// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! tmpfs: a filesystem in memory.
//!
//! Directories are sorted lists of names; files are page caches with no
//! disk below ([`FileCache`] with a non-persistent backing), so a file's
//! pages are its data and holes read as zeros. A filesystem has a size
//! limit, charged by the bytes its files are long, and its files' memory
//! goes when the last reference to them does: a file removed while a
//! program has it open lives until the program lets go, as on Linux.
//!
//! The root filesystem of every Radix system is a tmpfs (spec 7.1), as
//! are `/system/etc` and `/system/var` on top of the read-only boot
//! archive until Phase 10 brings Btrfs.

use alloc::sync::Weak;
use core::any::Any;
use core::sync::atomic::{AtomicU64, Ordering};

use radix_sync::Mutex;

use crate::cache::{Backing, FileCache};
use crate::mem::{self, Alloc, KArc, KVec};
use crate::path::check_name;
use crate::{
    Error, FileSystem, FsInfo, FsKind, In, Kind, ListFn, Node, NodeRef, Out, Result, Stat, now,
};

/// What every node of one tmpfs shares.
struct Shared {
    /// The most bytes its files may hold together.
    max: u64,
    /// The bytes they hold.
    used: AtomicU64,
    next_id: AtomicU64,
    /// Held while names are made, removed or moved, so that a move sees
    /// both directories unchanged and two moves cannot make a loop. Only
    /// one directory is locked at a time under it.
    namespace: Mutex<()>,
}

/// A tmpfs.
pub struct Tmpfs {
    root: KArc<TmpNode>,
    shared: KArc<Shared>,
}

struct Entry {
    name: KVec<u8>,
    node: KArc<TmpNode>,
}

impl Entry {
    fn name(&self) -> &str {
        core::str::from_utf8(&self.name).unwrap_or("")
    }
}

struct DirState {
    /// Sorted by name.
    entries: KVec<Entry>,
    modified: u64,
}

enum Body {
    Dir(Mutex<DirState>),
    File { cache: FileCache, charged: AtomicU64 },
}

/// A tmpfs file or directory.
pub struct TmpNode {
    id: u64,
    created: u64,
    shared: KArc<Shared>,
    /// The directory it is in; none for the root, or once it is removed.
    parent: Mutex<Option<Weak<TmpNode, Alloc>>>,
    /// Itself, so that it can be its entries' parent.
    me: radix_sync::Once<Weak<TmpNode, Alloc>>,
    body: Body,
}

impl Tmpfs {
    /// An empty tmpfs whose files may hold `max` bytes together.
    pub fn new(max: u64) -> Result<KArc<Tmpfs>> {
        let shared = mem::arc(Shared {
            max,
            used: AtomicU64::new(0),
            next_id: AtomicU64::new(2),
            namespace: Mutex::new(()),
        })?;
        let root = TmpNode::new(1, shared.clone(), Kind::Directory)?;
        Ok(mem::arc(Tmpfs { root, shared })?)
    }
}

impl FileSystem for Tmpfs {
    fn kind(&self) -> FsKind {
        FsKind::Tmpfs
    }

    fn root(&self) -> NodeRef {
        self.root.clone()
    }

    fn info(&self) -> FsInfo {
        let page = radix_block::PAGE_SIZE as u64;
        FsInfo {
            block_size: page as u32,
            blocks: self.shared.max / page,
            free: self.shared.max.saturating_sub(self.shared.used.load(Ordering::Relaxed)) / page,
        }
    }
}

impl TmpNode {
    fn new(id: u64, shared: KArc<Shared>, kind: Kind) -> Result<KArc<TmpNode>> {
        let created = now();
        let body = match kind {
            Kind::Directory => {
                Body::Dir(Mutex::new(DirState { entries: mem::vec(), modified: created }))
            }
            Kind::File => {
                Body::File { cache: FileCache::new(0, created), charged: AtomicU64::new(0) }
            }
            _ => return Err(Error::NotSupported),
        };
        let node = mem::arc(TmpNode {
            id,
            created,
            shared,
            parent: Mutex::new(None),
            me: radix_sync::Once::new(),
            body,
        })?;
        let _ = node.me.set(KArc::downgrade(&node));
        Ok(node)
    }

    /// A weak reference to itself.
    fn weak(&self) -> Option<Weak<TmpNode, Alloc>> {
        self.me.get().cloned()
    }

    fn dir(&self) -> Result<&Mutex<DirState>> {
        match &self.body {
            Body::Dir(dir) => Ok(dir),
            Body::File { .. } => Err(Error::NotDirectory),
        }
    }

    fn file(&self) -> Result<(&FileCache, Charge<'_>)> {
        match &self.body {
            Body::File { cache, charged } => Ok((cache, Charge { shared: &self.shared, charged })),
            Body::Dir(_) => Err(Error::IsDirectory),
        }
    }

    /// Whether it is a directory with nothing in it. Called with its
    /// parent's (or the directory it would replace's) lock held, and the
    /// namespace lock, so only one thread ever takes two directory locks
    /// at once, the outer one first.
    fn is_empty_dir(&self) -> bool {
        matches!(&self.body, Body::Dir(dir) if dir.lock_nested(1).entries.is_empty())
    }
}

/// The backing of a tmpfs file: no disk, and growth charged to the
/// filesystem's limit.
struct Charge<'a> {
    shared: &'a Shared,
    /// The bytes this file is charged.
    charged: &'a AtomicU64,
}

impl Backing for Charge<'_> {
    fn persistent(&self) -> bool {
        false
    }

    fn reserve(&self, size: u64) -> Result<()> {
        let had = self.charged.load(Ordering::Relaxed);
        if size <= had {
            return Ok(());
        }
        let more = size - had;
        let used = self.shared.used.fetch_add(more, Ordering::Relaxed);
        if used + more > self.shared.max {
            self.shared.used.fetch_sub(more, Ordering::Relaxed);
            return Err(Error::NoSpace);
        }
        self.charged.store(size, Ordering::Relaxed);
        Ok(())
    }

    fn truncate(&self, size: u64) -> Result<()> {
        let had = self.charged.load(Ordering::Relaxed);
        if size < had {
            self.shared.used.fetch_sub(had - size, Ordering::Relaxed);
            self.charged.store(size, Ordering::Relaxed);
        }
        Ok(())
    }
}

impl Drop for TmpNode {
    fn drop(&mut self) {
        if let Body::File { charged, .. } = &self.body {
            self.shared.used.fetch_sub(charged.load(Ordering::Relaxed), Ordering::Relaxed);
        }
    }
}

/// Where `name` is, or would go, in sorted `entries`.
fn find(entries: &[Entry], name: &str) -> core::result::Result<usize, usize> {
    entries.binary_search_by(|entry| entry.name.as_slice().cmp(name.as_bytes()))
}

impl Node for TmpNode {
    fn stat(&self) -> Stat {
        let (size, modified, pages) = match &self.body {
            Body::Dir(dir) => (0, dir.lock().modified, 0),
            Body::File { cache, .. } => cache.status(),
        };
        let allocated = (pages * radix_block::PAGE_SIZE) as u64;
        Stat { kind: self.kind(), size, allocated, id: self.id, created: self.created, modified }
    }

    fn kind(&self) -> Kind {
        match self.body {
            Body::Dir(_) => Kind::Directory,
            Body::File { .. } => Kind::File,
        }
    }

    fn lookup(&self, name: &str) -> Result<NodeRef> {
        let dir = self.dir()?.lock();
        match find(&dir.entries, name) {
            Ok(i) => Ok(dir.entries[i].node.clone()),
            Err(_) => Err(Error::NotFound),
        }
    }

    fn create(&self, name: &str, kind: Kind) -> Result<NodeRef> {
        check_name(name)?;
        let dir = self.dir()?;
        let id = self.shared.next_id.fetch_add(1, Ordering::Relaxed);
        let node = TmpNode::new(id, self.shared.clone(), kind)?;
        let name_bytes = mem::copy(name.as_bytes())?;
        let _namespace = self.shared.namespace.lock();
        let mut dir = dir.lock();
        let at = match find(&dir.entries, name) {
            Ok(_) => return Err(Error::Exists),
            Err(at) => at,
        };
        *node.parent.lock() = self.weak();
        mem::insert(&mut dir.entries, at, Entry { name: name_bytes, node: node.clone() })?;
        dir.modified = now();
        Ok(node)
    }

    fn remove(&self, name: &str) -> Result<()> {
        check_name(name)?;
        let _namespace = self.shared.namespace.lock();
        let mut dir = self.dir()?.lock();
        let at = find(&dir.entries, name).map_err(|_| Error::NotFound)?;
        let node = &dir.entries[at].node;
        if matches!(node.body, Body::Dir(_)) && !node.is_empty_dir() {
            return Err(Error::NotEmpty);
        }
        *node.parent.lock() = None;
        dir.entries.remove(at);
        dir.modified = now();
        Ok(())
    }

    fn rename(&self, from: &str, to_dir: &dyn Node, to: &str) -> Result<()> {
        check_name(from)?;
        check_name(to)?;
        let target = crate::downcast::<TmpNode>(to_dir).ok_or(Error::CrossMount)?;
        if !KArc::ptr_eq(&target.shared, &self.shared) {
            return Err(Error::CrossMount);
        }
        target.dir()?;
        let _namespace = self.shared.namespace.lock();
        if core::ptr::eq(self, target) {
            return rename_within(&mut self.dir()?.lock(), from, to);
        }
        // The entry to move, checked against what it would replace.
        let moving = {
            let source = self.dir()?.lock();
            let at = find(&source.entries, from).map_err(|_| Error::NotFound)?;
            source.entries[at].node.clone()
        };
        // A directory cannot move under itself.
        let target_ref = target.weak().and_then(|weak| weak.upgrade()).ok_or(Error::NotFound)?;
        if matches!(moving.body, Body::Dir(_)) && is_under(&target_ref, &moving) {
            return Err(Error::BadName);
        }
        let name = mem::copy(to.as_bytes())?;
        {
            let mut dest = target.dir()?.lock();
            match find(&dest.entries, to) {
                Ok(i) => {
                    check_replace(&moving, &dest.entries[i].node)?;
                    *dest.entries[i].node.parent.lock() = None;
                    dest.entries[i] = Entry { name, node: moving.clone() };
                }
                Err(at) => {
                    mem::insert(&mut dest.entries, at, Entry { name, node: moving.clone() })?;
                }
            }
            dest.modified = now();
        }
        *moving.parent.lock() = target.weak();
        // The namespace lock kept the entry where it was.
        let mut source = self.dir()?.lock();
        if let Ok(at) = find(&source.entries, from) {
            source.entries.remove(at);
        }
        source.modified = now();
        Ok(())
    }

    fn list(&self, cursor: u64, f: ListFn<'_>) -> Result<()> {
        let dir = self.dir()?.lock();
        for (i, entry) in dir.entries.iter().enumerate().skip(cursor as usize) {
            if !f(i as u64 + 1, entry.name(), entry.node.kind()) {
                break;
            }
        }
        Ok(())
    }

    fn read(&self, offset: u64, out: &mut dyn Out) -> Result<usize> {
        let (cache, backing) = self.file()?;
        cache.read(offset, out, &backing)
    }

    fn write(&self, offset: u64, data: &dyn In) -> Result<usize> {
        let (cache, backing) = self.file()?;
        cache.write(offset, data, &backing)
    }

    fn set_size(&self, size: u64) -> Result<()> {
        let (cache, backing) = self.file()?;
        cache.set_size(size, &backing)
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

/// Whether `node` is `dir` or lies under it.
fn is_under(node: &KArc<TmpNode>, dir: &KArc<TmpNode>) -> bool {
    let mut current = node.clone();
    loop {
        if KArc::ptr_eq(&current, dir) {
            return true;
        }
        let parent = current.parent.lock().as_ref().and_then(Weak::upgrade);
        match parent {
            Some(parent) => current = parent,
            None => return false,
        }
    }
}

/// Whether `moving` may replace `old`: a file replaces a file, a
/// directory an empty directory.
fn check_replace(moving: &TmpNode, old: &TmpNode) -> Result<()> {
    match (&moving.body, &old.body) {
        (_, Body::Dir(_)) if !old.is_empty_dir() => Err(Error::NotEmpty),
        (Body::Dir(_), Body::File { .. }) => Err(Error::NotDirectory),
        (Body::File { .. }, Body::Dir(_)) => Err(Error::IsDirectory),
        _ => Ok(()),
    }
}

/// A rename inside one directory.
fn rename_within(dir: &mut DirState, from: &str, to: &str) -> Result<()> {
    let at = find(&dir.entries, from).map_err(|_| Error::NotFound)?;
    if from == to {
        return Ok(());
    }
    let name = mem::copy(to.as_bytes())?;
    if let Ok(i) = find(&dir.entries, to) {
        check_replace(&dir.entries[at].node, &dir.entries[i].node)?;
        *dir.entries[i].node.parent.lock() = None;
        dir.entries.remove(i);
    }
    let at = find(&dir.entries, from).map_err(|_| Error::NotFound)?;
    let entry = dir.entries.remove(at);
    let at = find(&dir.entries, to).unwrap_or_else(|at| at);
    mem::insert(&mut dir.entries, at, Entry { name, node: entry.node })?;
    dir.modified = now();
    Ok(())
}
