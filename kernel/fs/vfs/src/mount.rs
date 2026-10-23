// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! The mount table and path resolution.
//!
//! The [`Vfs`] joins filesystems into one tree: each [`Mount`] puts a
//! filesystem's root over a directory of another mount (its *mount
//! point*), and the root mount is the tree's root. A place in the tree is
//! a [`Location`]: a mount and a node of its filesystem. Directory
//! capabilities hold one, and every path is resolved beneath one
//! ([`Vfs::resolve`]).
//!
//! Resolution walks the path's components from the location, keeping the
//! chain of locations it passed: a name looks the next node up, and moves
//! onto the root of whatever is mounted there; `..` steps back along the
//! chain, so it never leaves the location the walk began at (an attempt
//! is [`Error::Outside`]); `.` stays. A path with no `..`, the common
//! case, needs no chain: the walk then holds only where it is, and takes
//! no references to the places it passes.
//!
//! Mount points are told by the node's address: every filesystem keeps
//! one node per file while it is referenced, and the mount keeps its
//! mount point referenced. Each mount also keeps a filter of the nodes
//! mounted on (a bit for each, chosen by address, as in a Bloom filter),
//! so that a walk past a node whose bit is clear knows, without the mount
//! table or its lock, that nothing is mounted there; Linux marks mount
//! points in its directory cache for the same reason (`DCACHE_MOUNTED`).

use core::fmt;
use core::sync::atomic::{AtomicU64, Ordering};

use radix_sync::Mutex;

use crate::mem::{self, KArc, KVec};
use crate::path::{self, Component};
use crate::{Error, FsRef, Kind, ListFn, NodeRef, Result, Stat, address};

/// A filesystem mounted on a directory.
pub struct Mount {
    /// Its number, unique since boot.
    pub id: u64,
    /// The filesystem.
    pub fs: FsRef,
    root: NodeRef,
    /// Where it is mounted: the mount and directory it covers; none for
    /// the root mount.
    on: Option<(MountRef, NodeRef)>,
    /// Whether it was mounted read-only.
    read_only: bool,
    /// Where it is in the tree, from the root, for `/proc/mounts`.
    path: KVec<u8>,
    /// What it was mounted from: a device's name, or the filesystem's.
    source: KVec<u8>,
    /// The filter of its nodes that something is mounted on: bit
    /// [`filter_bit`] of each is set. Changed only under the table's lock;
    /// a bit is set before its mount is in the table, and cleared after it
    /// has left.
    covered: [AtomicU64; FILTER_WORDS],
}

/// The words of a mount's filter of mount points: 128 bits, enough that
/// the root, with a dozen mounts on it, rarely sends a walk to the table.
const FILTER_WORDS: usize = 2;

/// The bit of a mount's filter that stands for the node at `address`:
/// Fibonacci hashing of the address, whose low bits are the same for
/// every node of a type.
fn filter_bit(address: usize) -> (usize, u64) {
    let hash = (address as u64).wrapping_mul(0x9e37_79b9_7f4a_7c15) >> 57;
    ((hash >> 6) as usize % FILTER_WORDS, 1 << (hash & 63))
}

/// A mount, shared.
pub type MountRef = KArc<Mount>;

impl Mount {
    /// A mount of `fs` that is in no tree, for files that live outside
    /// it, such as pipes: a [`Location`] needs a mount, and nothing is
    /// ever resolved through this one.
    pub fn detached(fs: FsRef, source: &str) -> Result<MountRef> {
        let root = fs.root();
        Ok(mem::arc(Mount {
            id: crate::next_id(),
            fs,
            root,
            on: None,
            read_only: false,
            path: mem::vec(),
            source: mem::copy(source.as_bytes())?,
            covered: [const { AtomicU64::new(0) }; FILTER_WORDS],
        })?)
    }

    /// Whether nothing on it may change.
    pub fn read_only(&self) -> bool {
        self.read_only || self.fs.read_only()
    }

    /// Where it is in the tree, from the root: `""` for the root mount.
    pub fn path(&self) -> &str {
        core::str::from_utf8(&self.path).unwrap_or("?")
    }

    /// What it was mounted from.
    pub fn source(&self) -> &str {
        core::str::from_utf8(&self.source).unwrap_or("?")
    }

    /// Whether something may be mounted on its node at `address`: false
    /// means nothing is.
    fn may_cover(&self, address: usize) -> bool {
        let (word, bit) = filter_bit(address);
        self.covered[word].load(Ordering::Acquire) & bit != 0
    }
}

/// A place in the tree.
#[derive(Clone)]
pub struct Location {
    /// The mount it is on.
    pub mount: MountRef,
    /// The node.
    pub node: NodeRef,
}

impl Location {
    /// Whether it is the same place as `other`.
    pub fn same(&self, other: &Location) -> bool {
        KArc::ptr_eq(&self.mount, &other.mount) && address(&self.node) == address(&other.node)
    }

    /// Fails with [`Error::ReadOnly`] if its mount is read-only.
    pub fn writable(&self) -> Result<()> {
        if self.mount.read_only() { Err(Error::ReadOnly) } else { Ok(()) }
    }
}

/// How `open` opens.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct OpenFlags {
    /// Make the file if it is not there.
    pub create: bool,
    /// With `create`: fail if it is there.
    pub exclusive: bool,
    /// Cut the file to nothing.
    pub truncate: bool,
    /// Fail unless it is a directory.
    pub directory: bool,
}

/// The tree of mounted filesystems.
pub struct Vfs {
    root: Location,
    mounts: Mutex<KVec<MountRef>>,
}

/// One step of a resolution: where it is, and the name that led there.
type Step<'p> = (Location, &'p str);

impl Vfs {
    /// A tree whose root is `fs`'s root, mounted from `source`.
    pub fn new(fs: FsRef, source: &str) -> Result<Vfs> {
        let root = fs.root();
        let mount = mem::arc(Mount {
            id: crate::next_id(),
            fs,
            root: root.clone(),
            on: None,
            read_only: false,
            path: mem::vec(),
            source: mem::copy(source.as_bytes())?,
            covered: [const { AtomicU64::new(0) }; FILTER_WORDS],
        })?;
        let mut mounts = mem::vec();
        mem::push(&mut mounts, mount.clone())?;
        Ok(Vfs { root: Location { mount, node: root }, mounts: Mutex::new(mounts) })
    }

    /// The root of the tree.
    pub fn root(&self) -> Location {
        self.root.clone()
    }

    /// What is mounted on `node` of `mount`, if anything.
    fn mounted_on(&self, mount: &Mount, node: &NodeRef) -> Option<MountRef> {
        let node = address(node);
        if !mount.may_cover(node) {
            return None;
        }
        let mounts = self.mounts.lock();
        mounts
            .iter()
            .find(|m| {
                m.on.as_ref().is_some_and(|(on, covered)| {
                    core::ptr::eq(KArc::as_ptr(on), mount) && address(covered) == node
                })
            })
            .cloned()
    }

    /// The root of what is mounted on `node` of `mount`, and so on up a
    /// stack of mounts; none if nothing is.
    fn mounted_root(&self, mount: &Mount, node: &NodeRef) -> Option<Location> {
        let mut top = self.mounted_on(mount, node)?;
        while let Some(next) = self.mounted_on(&top, &top.root) {
            top = next;
        }
        Some(Location { node: top.root.clone(), mount: top })
    }

    /// `at`, or the root of what is mounted there, and so on up a stack of
    /// mounts.
    fn cross(&self, at: Location) -> Location {
        self.mounted_root(&at.mount, &at.node).unwrap_or(at)
    }

    /// Walks `path` from `base`, calling `f` with the chain of steps.
    fn walk<'p, T>(
        &self,
        base: &Location,
        path: &'p str,
        f: impl FnOnce(&[Step<'p>]) -> Result<T>,
    ) -> Result<T> {
        let mut chain: KVec<Step<'p>> = mem::vec_with_capacity(8)?;
        mem::push(&mut chain, (base.clone(), ""))?;
        for component in path::components(path) {
            match component? {
                Component::Current => {}
                Component::Parent => {
                    if chain.len() == 1 {
                        return Err(Error::Outside);
                    }
                    chain.pop();
                }
                Component::Name(name) => {
                    let Some((here, _)) = chain.last() else { return Err(Error::Outside) };
                    let node = here.node.lookup(name)?;
                    let next = self.cross(Location { mount: here.mount.clone(), node });
                    mem::push(&mut chain, (next, name))?;
                }
            }
        }
        f(&chain)
    }

    /// Where `path` leads from `base`.
    pub fn resolve(&self, base: &Location, path: &str) -> Result<Location> {
        // Where the walk is, when it has left `base`: a name moves the
        // node on, and a mount there the mount too.
        let mut mount: Option<MountRef> = None;
        let mut node: Option<NodeRef> = None;
        for component in path::components(path) {
            match component? {
                Component::Current => {}
                // Only `..` needs the chain, to step back along.
                Component::Parent => {
                    return self.walk(base, path, |chain| {
                        chain.last().map(|(at, _)| at.clone()).ok_or(Error::Outside)
                    });
                }
                Component::Name(name) => {
                    let found = node.as_ref().unwrap_or(&base.node).lookup(name)?;
                    match self.mounted_root(mount.as_ref().unwrap_or(&base.mount), &found) {
                        Some(top) => {
                            mount = Some(top.mount);
                            node = Some(top.node);
                        }
                        None => node = Some(found),
                    }
                }
            }
        }
        Ok(Location {
            mount: mount.unwrap_or_else(|| base.mount.clone()),
            node: node.unwrap_or_else(|| base.node.clone()),
        })
    }

    /// The directory `path`'s last name is in, and that name.
    pub fn resolve_parent<'p>(
        &self,
        base: &Location,
        path: &'p str,
    ) -> Result<(Location, &'p str)> {
        let (dir, name) = path::split_last(path)?;
        let at = self.resolve(base, dir)?;
        if at.node.kind() != Kind::Directory {
            return Err(Error::NotDirectory);
        }
        Ok((at, name))
    }

    /// Opens `path` beneath `base`.
    pub fn open(&self, base: &Location, path: &str, flags: OpenFlags) -> Result<Location> {
        let at = if flags.create {
            let (dir, name) = self.resolve_parent(base, path)?;
            match dir.node.lookup(name) {
                Ok(_) if flags.exclusive => return Err(Error::Exists),
                Ok(node) => self.cross(Location { mount: dir.mount.clone(), node }),
                Err(Error::NotFound) => {
                    dir.writable()?;
                    let node = dir.node.create(name, Kind::File)?;
                    Location { mount: dir.mount.clone(), node }
                }
                Err(err) => return Err(err),
            }
        } else {
            self.resolve(base, path)?
        };
        let kind = at.node.kind();
        if flags.directory && kind != Kind::Directory {
            return Err(Error::NotDirectory);
        }
        if flags.truncate && kind == Kind::File {
            at.writable()?;
            at.node.set_size(0)?;
        }
        let node = at.node.open(&at.node)?;
        Ok(Location { mount: at.mount, node })
    }

    /// Makes the directory `path` beneath `base`.
    pub fn make_directory(&self, base: &Location, path: &str) -> Result<()> {
        let (dir, name) = self.resolve_parent(base, path)?;
        dir.writable()?;
        dir.node.create(name, Kind::Directory).map(drop)
    }

    /// Removes the file or empty directory `path` beneath `base`.
    pub fn remove(&self, base: &Location, path: &str) -> Result<()> {
        let (dir, name) = self.resolve_parent(base, path)?;
        dir.writable()?;
        let node = dir.node.lookup(name)?;
        if self.mounted_on(&dir.mount, &node).is_some() {
            return Err(Error::InUse);
        }
        dir.node.remove(name)
    }

    /// Moves `from` beneath `base` to `to` beneath `to_base`, on one
    /// filesystem.
    pub fn rename(&self, base: &Location, from: &str, to_base: &Location, to: &str) -> Result<()> {
        let (source, from_name) = self.resolve_parent(base, from)?;
        let (target, to_name) = self.resolve_parent(to_base, to)?;
        if !KArc::ptr_eq(&source.mount, &target.mount) {
            return Err(Error::CrossMount);
        }
        source.writable()?;
        let moving = source.node.lookup(from_name)?;
        if self.mounted_on(&source.mount, &moving).is_some() {
            return Err(Error::InUse);
        }
        if let Ok(replaced) = target.node.lookup(to_name)
            && self.mounted_on(&target.mount, &replaced).is_some()
        {
            return Err(Error::InUse);
        }
        source.node.rename(from_name, &*target.node, to_name)
    }

    /// What `path` beneath `base` is.
    pub fn stat(&self, base: &Location, path: &str) -> Result<Stat> {
        Ok(self.resolve(base, path)?.node.stat())
    }

    /// Lists the directory at `at` from `cursor`.
    pub fn list(&self, at: &Location, cursor: u64, f: ListFn<'_>) -> Result<()> {
        at.node.list(cursor, f)
    }

    /// Mounts `fs` on the directory `path` beneath `base`.
    pub fn mount(
        &self,
        base: &Location,
        path: &str,
        fs: FsRef,
        source: &str,
        read_only: bool,
    ) -> Result<MountRef> {
        let (at, where_) = self.walk(base, path, |chain| {
            let at = chain.last().map(|(at, _)| at.clone()).ok_or(Error::Outside)?;
            let mut where_ = mem::vec();
            prefix_of(base, &mut where_)?;
            for (_, name) in &chain[1..] {
                if !where_.is_empty() {
                    mem::push(&mut where_, b'/')?;
                }
                for &byte in name.as_bytes() {
                    mem::push(&mut where_, byte)?;
                }
            }
            Ok((at, where_))
        })?;
        if at.node.kind() != Kind::Directory {
            return Err(Error::NotDirectory);
        }
        let root = fs.root();
        let mount = mem::arc(Mount {
            id: crate::next_id(),
            fs,
            root,
            on: Some((at.mount.clone(), at.node.clone())),
            read_only,
            path: where_,
            source: mem::copy(source.as_bytes())?,
            covered: [const { AtomicU64::new(0) }; FILTER_WORDS],
        })?;
        let mut mounts = self.mounts.lock();
        // `at` is what is on top already, so a second mount on the same
        // directory stacks on the first, as on Linux; nothing is ever
        // mounted twice on one node.
        let taken = mounts.iter().any(|m| {
            m.on.as_ref().is_some_and(|(on, covered)| {
                KArc::ptr_eq(on, &at.mount) && address(covered) == address(&at.node)
            })
        });
        if taken {
            return Err(Error::InUse);
        }
        mounts.try_reserve(1).map_err(|_| Error::NoMemory)?;
        // The bit first: a walk that sees it clear has not seen the mount
        // either, and one that sees it set looks in the table.
        let (word, bit) = filter_bit(address(&at.node));
        at.mount.covered[word].fetch_or(bit, Ordering::Release);
        let _ = mounts.push_within_capacity(mount.clone());
        Ok(mount)
    }

    /// Unmounts the filesystem whose root `path` beneath `base` is, after
    /// syncing it. Fails with [`Error::InUse`] while anything is mounted
    /// on it or any handle holds a place on it.
    pub fn unmount(&self, base: &Location, path: &str) -> Result<()> {
        let at = self.resolve(base, path)?;
        if at.mount.on.is_none() || address(&at.node) != address(&at.mount.root) {
            return Err(Error::NotFound);
        }
        let mount = at.mount.clone();
        drop(at);
        let mut mounts = self.mounts.lock();
        if mounts.iter().any(|m| m.on.as_ref().is_some_and(|(on, _)| KArc::ptr_eq(on, &mount))) {
            return Err(Error::InUse);
        }
        // The table and `mount` hold it; anything more is a handle.
        if KArc::strong_count(&mount) > 2 {
            return Err(Error::InUse);
        }
        mount.fs.sync()?;
        mounts.retain(|m| !KArc::ptr_eq(m, &mount));
        if let Some((on, _)) = &mount.on {
            refilter(&mounts, on);
        }
        Ok(())
    }

    /// Unmounts the filesystem whose root `path` beneath `base` is and
    /// every filesystem mounted beneath it, after syncing each: what the
    /// kernel's fuzz test does with the tree it let programs mount things
    /// in, once they have gone. Fails with [`Error::InUse`], unmounting
    /// none, while any handle holds a place on one of them.
    pub fn unmount_tree(&self, base: &Location, path: &str) -> Result<()> {
        let at = self.resolve(base, path)?;
        if at.mount.on.is_none() || address(&at.node) != address(&at.mount.root) {
            return Err(Error::NotFound);
        }
        let top = at.mount.clone();
        drop(at);
        let mut mounts = self.mounts.lock();
        let beneath = |mount: &MountRef| {
            let mut at = mount;
            loop {
                if KArc::ptr_eq(at, &top) {
                    return true;
                }
                match &at.on {
                    Some((on, _)) => at = on,
                    None => return false,
                }
            }
        };
        let mut doomed = mem::vec_with_capacity(mounts.len())?;
        for mount in mounts.iter().filter(|m| beneath(m)) {
            let _ = doomed.push_within_capacity(mount.clone());
        }
        // Each is held by the table, by `doomed`, by every mount on it,
        // and `top` by `top` too; anything more is a handle.
        for mount in doomed.iter() {
            let on_it = doomed
                .iter()
                .filter(|m| m.on.as_ref().is_some_and(|(on, _)| KArc::ptr_eq(on, mount)))
                .count();
            let expected = 2 + on_it + usize::from(KArc::ptr_eq(mount, &top));
            if KArc::strong_count(mount) > expected {
                return Err(Error::InUse);
            }
        }
        for mount in doomed.iter() {
            mount.fs.sync()?;
        }
        mounts.retain(|m| !doomed.iter().any(|d| KArc::ptr_eq(m, d)));
        if let Some((on, _)) = &top.on {
            refilter(&mounts, on);
        }
        Ok(())
    }

    /// Calls `f` with every mount, the root first.
    pub fn mounts(&self, mut f: impl FnMut(&Mount)) {
        let mounts = self.mounts.lock();
        for mount in mounts.iter() {
            f(mount);
        }
    }

    /// Syncs every filesystem with changes; returns the first error.
    pub fn sync_all(&self) -> Result<()> {
        let mut first = Ok(());
        for fs in self.filesystems()?.iter() {
            if fs.is_dirty()
                && let Err(err) = fs.sync()
                && first.is_ok()
            {
                first = Err(err);
            }
        }
        first
    }

    /// Drops up to `max` pages that filesystems can read again; returns
    /// how many went.
    pub fn shrink(&self, max: usize) -> usize {
        let Ok(filesystems) = self.filesystems() else { return 0 };
        let mut dropped = 0;
        for fs in filesystems.iter() {
            if dropped >= max {
                break;
            }
            dropped += fs.shrink(max - dropped);
        }
        dropped
    }

    /// The mounted filesystems, copied out of the table so that syncing
    /// does not hold it.
    fn filesystems(&self) -> Result<KVec<FsRef>> {
        let mounts = self.mounts.lock();
        let mut out = mem::vec_with_capacity(mounts.len())?;
        for mount in mounts.iter() {
            let _ = out.push_within_capacity(mount.fs.clone());
        }
        Ok(out)
    }
}

/// Sets `mount`'s filter of mount points from the table, `mounts`, after
/// mounts on it have left: each word is computed whole and then stored, so
/// a walk never sees a bit clear that a mount still in the table needs.
fn refilter(mounts: &[MountRef], mount: &MountRef) {
    let mut words = [0u64; FILTER_WORDS];
    for m in mounts {
        if let Some((on, covered)) = &m.on
            && KArc::ptr_eq(on, mount)
        {
            let (word, bit) = filter_bit(address(covered));
            words[word] |= bit;
        }
    }
    for (filter, word) in mount.covered.iter().zip(words) {
        filter.store(word, Ordering::Release);
    }
}

/// Puts the path from the root to `base`, when it is known: the root, or
/// the root of a mount.
fn prefix_of(base: &Location, out: &mut KVec<u8>) -> Result<()> {
    if address(&base.node) == address(&base.mount.root) {
        for &byte in &base.mount.path {
            mem::push(out, byte)?;
        }
        Ok(())
    } else {
        for &byte in b"?" {
            mem::push(out, byte)?;
        }
        Ok(())
    }
}

impl fmt::Debug for Location {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Location(mount {}, node {:#x})", self.mount.id, address(&self.node))
    }
}
