// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! tmpfs: a filesystem in memory.
//!
//! Directories list their entries in the order they were made, with an
//! index sorted by name for lookups; files are page caches with no
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
    Error, FileSystem, FsInfo, FsKind, In, Kind, ListFn, Node, NodeRef, Out, Result, SocketBinding,
    Stat, now,
};

/// What every node of one tmpfs shares.
struct Shared {
    /// The most bytes its files may hold together.
    max: u64,
    /// The bytes they hold.
    used: AtomicU64,
    /// The most files and directories it may hold, its root included.
    max_files: u64,
    /// The files and directories it holds.
    files: AtomicU64,
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
    /// When it was made, in its directory: a listing goes in this order.
    seq: u64,
    name: KVec<u8>,
    node: KArc<TmpNode>,
}

impl Entry {
    fn name(&self) -> &str {
        core::str::from_utf8(&self.name).unwrap_or("")
    }
}

struct DirState {
    /// In the order they were made in, which listings follow. A listing's
    /// cursor is the sequence number to go on from, so an entry removed
    /// while a program lists the directory never makes it skip another,
    /// as POSIX `readdir` needs (`rm -r` removes as it lists).
    entries: KVec<Entry>,
    /// Indexes into `entries`, sorted by the entries' names.
    by_name: KVec<u32>,
    /// The sequence number of the next entry made.
    next_seq: u64,
    modified: u64,
}

impl DirState {
    fn new(modified: u64) -> DirState {
        DirState { entries: mem::vec(), by_name: mem::vec(), next_seq: 1, modified }
    }

    /// Where `name` is in `by_name`, or would go.
    fn find(&self, name: &str) -> core::result::Result<usize, usize> {
        self.by_name
            .binary_search_by(|&i| self.entries[i as usize].name.as_slice().cmp(name.as_bytes()))
    }

    /// The entry `name`.
    fn get(&self, name: &str) -> Result<&Entry> {
        let at = self.find(name).map_err(|_| Error::NotFound)?;
        Ok(self.at(at))
    }

    /// Adds `node` as `name`, which [`DirState::find`] said goes at `at`.
    fn insert(&mut self, at: usize, name: KVec<u8>, node: KArc<TmpNode>) -> Result<()> {
        let index = u32::try_from(self.entries.len()).map_err(|_| Error::NoSpace)?;
        // Room in both first, so that a failure changes nothing.
        self.by_name.try_reserve(1).map_err(|_| Error::NoMemory)?;
        mem::push(&mut self.entries, Entry { seq: self.next_seq, name, node })?;
        self.next_seq += 1;
        mem::insert(&mut self.by_name, at, index)?;
        Ok(())
    }

    /// Takes out the entry at `at` in `by_name`.
    fn remove(&mut self, at: usize) -> Entry {
        let index = self.by_name.remove(at);
        for i in &mut self.by_name {
            if *i > index {
                *i -= 1;
            }
        }
        self.entries.remove(index as usize)
    }

    /// The entry at `at` in `by_name`.
    fn at(&self, at: usize) -> &Entry {
        &self.entries[self.by_name[at] as usize]
    }

    /// The entry at `at` in `by_name`, to change.
    fn at_mut(&mut self, at: usize) -> &mut Entry {
        &mut self.entries[self.by_name[at] as usize]
    }
}

enum Body {
    Dir(Mutex<DirState>),
    File {
        cache: FileCache,
        charged: AtomicU64,
    },
    /// A symbolic link's target, charged to the limit like a file's
    /// bytes, and when it was last changed, as `set_times` sets it.
    Link {
        target: KVec<u8>,
        modified: AtomicU64,
    },
    /// A socket's name, with what the kernel keeps in it
    /// ([`Node::make_socket`]), and when it was last changed, as
    /// `set_times` sets it.
    Socket {
        binding: SocketBinding,
        modified: AtomicU64,
    },
}

/// A tmpfs file, directory, symbolic link or socket's name.
pub struct TmpNode {
    id: u64,
    created: u64,
    /// When it was last read, as `set_times` set it; reads leave it.
    accessed: AtomicU64,
    shared: KArc<Shared>,
    /// The directory it is in; none for the root, or once it is removed.
    parent: Mutex<Option<Weak<TmpNode, Alloc>>>,
    /// Itself, so that it can be its entries' parent.
    me: radix_sync::Once<Weak<TmpNode, Alloc>>,
    body: Body,
}

impl Tmpfs {
    /// An empty tmpfs whose files may hold `max` bytes together, and
    /// which may hold `max_files` files and directories, its root
    /// included.
    pub fn new(max: u64, max_files: u64) -> Result<KArc<Tmpfs>> {
        let shared = mem::arc(Shared {
            max,
            used: AtomicU64::new(0),
            max_files,
            files: AtomicU64::new(0),
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

    fn light_drops(&self) -> bool {
        true
    }

    fn info(&self) -> FsInfo {
        let page = radix_block::PAGE_SIZE as u64;
        FsInfo {
            block_size: page as u32,
            blocks: self.shared.max / page,
            free: self.shared.max.saturating_sub(self.shared.used.load(Ordering::Relaxed)) / page,
            files: self.shared.max_files,
            free_files: self
                .shared
                .max_files
                .saturating_sub(self.shared.files.load(Ordering::Relaxed)),
        }
    }
}

impl TmpNode {
    fn new(id: u64, shared: KArc<Shared>, kind: Kind) -> Result<KArc<TmpNode>> {
        let created = now();
        let body = match kind {
            Kind::Directory => Body::Dir(Mutex::new(DirState::new(created))),
            Kind::File => {
                Body::File { cache: FileCache::new(0, created), charged: AtomicU64::new(0) }
            }
            _ => return Err(Error::NotSupported),
        };
        TmpNode::with_body(id, shared, created, body)
    }

    /// A symbolic link to `target`.
    fn link(id: u64, shared: KArc<Shared>, target: &str) -> Result<KArc<TmpNode>> {
        let bytes = target.len() as u64;
        let used = shared.used.fetch_add(bytes, Ordering::Relaxed);
        if used + bytes > shared.max {
            shared.used.fetch_sub(bytes, Ordering::Relaxed);
            return Err(Error::NoSpace);
        }
        let created = now();
        let body = match mem::copy(target.as_bytes()) {
            Ok(target) => Body::Link { target, modified: AtomicU64::new(created) },
            Err(error) => {
                shared.used.fetch_sub(bytes, Ordering::Relaxed);
                return Err(error.into());
            }
        };
        TmpNode::with_body(id, shared, created, body)
    }

    /// A socket's name, holding `binding`.
    fn socket(id: u64, shared: KArc<Shared>, binding: SocketBinding) -> Result<KArc<TmpNode>> {
        let created = now();
        let body = Body::Socket { binding, modified: AtomicU64::new(created) };
        TmpNode::with_body(id, shared, created, body)
    }

    /// A node with `body`; a node that cannot be made hands back what
    /// its body was charged, as one dropped does.
    fn with_body(id: u64, shared: KArc<Shared>, created: u64, body: Body) -> Result<KArc<TmpNode>> {
        // Each counts against the limit until it is dropped, as with
        // Linux's nr_inodes, so that empty files cannot use up the heap.
        let counted = shared.files.try_update(Ordering::Relaxed, Ordering::Relaxed, |files| {
            (files < shared.max_files).then_some(files + 1)
        });
        if counted.is_err() {
            if let Body::Link { target, .. } = &body {
                shared.used.fetch_sub(target.len() as u64, Ordering::Relaxed);
            }
            return Err(Error::NoSpace);
        }
        let node = mem::arc(TmpNode {
            id,
            created,
            accessed: AtomicU64::new(created),
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
            Body::File { .. } | Body::Link { .. } | Body::Socket { .. } => Err(Error::NotDirectory),
        }
    }

    fn file(&self) -> Result<(&FileCache, Charge<'_>)> {
        match &self.body {
            Body::File { cache, charged } => Ok((cache, Charge { shared: &self.shared, charged })),
            Body::Dir(_) => Err(Error::IsDirectory),
            // Resolution follows links, so only a caller that asked not
            // to reaches one here; a socket's name cannot be opened.
            Body::Link { .. } | Body::Socket { .. } => Err(Error::NotSupported),
        }
    }

    /// Adds `node`, new, to this directory as `name`.
    fn add(&self, name: &str, node: KArc<TmpNode>) -> Result<NodeRef> {
        let dir = self.dir()?;
        let name_bytes = mem::copy(name.as_bytes())?;
        let _namespace = self.shared.namespace.lock();
        let mut dir = dir.lock();
        let at = match dir.find(name) {
            Ok(_) => return Err(Error::Exists),
            Err(at) => at,
        };
        *node.parent.lock() = self.weak();
        dir.insert(at, name_bytes, node.clone())?;
        dir.modified = now();
        Ok(node)
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
        self.shared.files.fetch_sub(1, Ordering::Relaxed);
        let charged = match &self.body {
            Body::File { charged, .. } => charged.load(Ordering::Relaxed),
            Body::Link { target, .. } => target.len() as u64,
            Body::Dir(_) | Body::Socket { .. } => 0,
        };
        self.shared.used.fetch_sub(charged, Ordering::Relaxed);
    }
}

impl Node for TmpNode {
    fn stat(&self) -> Stat {
        let (size, modified, pages) = match &self.body {
            Body::Dir(dir) => (0, dir.lock().modified, 0),
            Body::File { cache, .. } => cache.status(),
            Body::Link { target, modified } => {
                (target.len() as u64, modified.load(Ordering::Relaxed), 0)
            }
            Body::Socket { modified, .. } => (0, modified.load(Ordering::Relaxed), 0),
        };
        let allocated = (pages * radix_block::PAGE_SIZE) as u64;
        Stat {
            kind: self.kind(),
            size,
            allocated,
            id: self.id,
            created: self.created,
            modified,
            accessed: self.accessed.load(Ordering::Relaxed),
        }
    }

    fn kind(&self) -> Kind {
        match self.body {
            Body::Dir(_) => Kind::Directory,
            Body::File { .. } => Kind::File,
            Body::Link { .. } => Kind::Symlink,
            Body::Socket { .. } => Kind::Socket,
        }
    }

    // A socket's name is connected through, never opened (ENXIO).
    fn open(&self, this: &NodeRef) -> Result<NodeRef> {
        match self.body {
            Body::Socket { .. } => Err(Error::NoDevice),
            _ => Ok(this.clone()),
        }
    }

    fn lookup(&self, name: &str) -> Result<NodeRef> {
        let dir = self.dir()?.lock();
        Ok(dir.get(name)?.node.clone())
    }

    fn create(&self, name: &str, kind: Kind) -> Result<NodeRef> {
        check_name(name)?;
        self.dir()?;
        let id = self.shared.next_id.fetch_add(1, Ordering::Relaxed);
        self.add(name, TmpNode::new(id, self.shared.clone(), kind)?)
    }

    fn make_link(&self, name: &str, target: &str) -> Result<NodeRef> {
        check_name(name)?;
        self.dir()?;
        let id = self.shared.next_id.fetch_add(1, Ordering::Relaxed);
        self.add(name, TmpNode::link(id, self.shared.clone(), target)?)
    }

    fn make_socket(&self, name: &str, binding: SocketBinding) -> Result<NodeRef> {
        check_name(name)?;
        self.dir()?;
        let id = self.shared.next_id.fetch_add(1, Ordering::Relaxed);
        self.add(name, TmpNode::socket(id, self.shared.clone(), binding)?)
    }

    fn socket_binding(&self) -> Option<SocketBinding> {
        match &self.body {
            Body::Socket { binding, .. } => Some(binding.clone()),
            _ => None,
        }
    }

    fn read_link(&self, out: &mut dyn Out) -> Result<usize> {
        match &self.body {
            Body::Link { target, .. } => {
                let room = out.len().min(target.len());
                out.put(0, &target[..room])?;
                Ok(target.len())
            }
            _ => Err(Error::NotLink),
        }
    }

    fn remove(&self, name: &str) -> Result<()> {
        check_name(name)?;
        let _namespace = self.shared.namespace.lock();
        let mut dir = self.dir()?.lock();
        let at = dir.find(name).map_err(|_| Error::NotFound)?;
        let node = &dir.at(at).node;
        if matches!(node.body, Body::Dir(_)) && !node.is_empty_dir() {
            return Err(Error::NotEmpty);
        }
        *node.parent.lock() = None;
        dir.remove(at);
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
            source.get(from)?.node.clone()
        };
        // A directory cannot move under itself.
        let target_ref = target.weak().and_then(|weak| weak.upgrade()).ok_or(Error::NotFound)?;
        if matches!(moving.body, Body::Dir(_)) && is_under(&target_ref, &moving) {
            return Err(Error::BadName);
        }
        let name = mem::copy(to.as_bytes())?;
        {
            let mut dest = target.dir()?.lock();
            match dest.find(to) {
                Ok(at) => {
                    let entry = dest.at_mut(at);
                    check_replace(&moving, &entry.node)?;
                    *entry.node.parent.lock() = None;
                    entry.node = moving.clone();
                }
                Err(at) => dest.insert(at, name, moving.clone())?,
            }
            dest.modified = now();
        }
        *moving.parent.lock() = target.weak();
        // The namespace lock kept the entry where it was.
        let mut source = self.dir()?.lock();
        if let Ok(at) = source.find(from) {
            source.remove(at);
        }
        source.modified = now();
        Ok(())
    }

    fn list(&self, cursor: u64, f: ListFn<'_>) -> Result<()> {
        let dir = self.dir()?.lock();
        let from = dir.entries.partition_point(|entry| entry.seq < cursor);
        for entry in &dir.entries[from..] {
            if !f(entry.seq + 1, entry.name(), entry.node.kind()) {
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

    fn set_times(&self, accessed: Option<u64>, modified: Option<u64>) -> Result<()> {
        if let Some(accessed) = accessed {
            self.accessed.store(accessed, Ordering::Relaxed);
        }
        if let Some(modified) = modified {
            match &self.body {
                Body::Dir(dir) => dir.lock().modified = modified,
                Body::File { cache, .. } => cache.set_modified(modified),
                Body::Link { modified: time, .. } | Body::Socket { modified: time, .. } => {
                    time.store(modified, Ordering::Relaxed)
                }
            }
        }
        Ok(())
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

/// Whether `moving` may replace `old`: a file, symbolic link or socket's
/// name replaces any of them, a directory an empty directory.
fn check_replace(moving: &TmpNode, old: &TmpNode) -> Result<()> {
    match (&moving.body, &old.body) {
        (Body::Dir(_), Body::Dir(_)) if !old.is_empty_dir() => Err(Error::NotEmpty),
        (Body::Dir(_), Body::Dir(_)) => Ok(()),
        (Body::Dir(_), _) => Err(Error::NotDirectory),
        (_, Body::Dir(_)) => Err(Error::IsDirectory),
        _ => Ok(()),
    }
}

/// A rename inside one directory. The entry keeps its place in listings
/// unless it replaces another, whose place it takes.
fn rename_within(dir: &mut DirState, from: &str, to: &str) -> Result<()> {
    let at = dir.find(from).map_err(|_| Error::NotFound)?;
    if from == to {
        return Ok(());
    }
    let mut name = mem::copy(to.as_bytes())?;
    match dir.find(to) {
        Ok(old) => {
            let moving = dir.at_mut(at).node.clone();
            let entry = dir.at_mut(old);
            check_replace(&moving, &entry.node)?;
            *entry.node.parent.lock() = None;
            entry.node = moving;
            dir.remove(at);
        }
        Err(to_at) => {
            // The index changes, and the entry's name with it; the index
            // has room already, as it loses one entry and gains one.
            let index = dir.by_name.remove(at);
            let to_at = if to_at > at { to_at - 1 } else { to_at };
            mem::insert(&mut dir.by_name, to_at, index)?;
            core::mem::swap(&mut dir.entries[index as usize].name, &mut name);
        }
    }
    dir.modified = now();
    Ok(())
}
