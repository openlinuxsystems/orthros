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
//! is [`Error::Outside`]); `.` stays. Mount points are told by the node's
//! address: every filesystem keeps one node per file while it is
//! referenced, and the mount keeps its mount point referenced.

use core::fmt;
use core::sync::atomic::{AtomicUsize, Ordering};

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
}

/// A mount, shared.
pub type MountRef = KArc<Mount>;

impl Mount {
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
    /// How many mounts there are: while only the root, resolution skips
    /// the table.
    count: AtomicUsize,
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
        })?;
        let mut mounts = mem::vec();
        mem::push(&mut mounts, mount.clone())?;
        Ok(Vfs {
            root: Location { mount, node: root },
            mounts: Mutex::new(mounts),
            count: AtomicUsize::new(1),
        })
    }

    /// The root of the tree.
    pub fn root(&self) -> Location {
        self.root.clone()
    }

    /// What is mounted on `at`, if anything.
    fn mounted_on(&self, at: &Location) -> Option<MountRef> {
        if self.count.load(Ordering::Acquire) <= 1 {
            return None;
        }
        let mounts = self.mounts.lock();
        let node = address(&at.node);
        mounts
            .iter()
            .find(|m| {
                m.on.as_ref().is_some_and(|(on, covered)| {
                    KArc::ptr_eq(on, &at.mount) && address(covered) == node
                })
            })
            .cloned()
    }

    /// `at`, or the root of what is mounted there, and so on up a stack of
    /// mounts.
    fn cross(&self, mut at: Location) -> Location {
        while let Some(mount) = self.mounted_on(&at) {
            at = Location { node: mount.root.clone(), mount };
        }
        at
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
        self.walk(base, path, |chain| chain.last().map(|(at, _)| at.clone()).ok_or(Error::Outside))
    }

    /// The directory `path`'s last name is in, and that name.
    pub fn resolve_parent<'p>(
        &self,
        base: &Location,
        path: &'p str,
    ) -> Result<(Location, &'p str)> {
        let (dir, name) = path::split_last(path)?;
        let at = self.resolve(base, dir)?;
        if at.node.stat().kind != Kind::Directory {
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
        let kind = at.node.stat().kind;
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
        if self.mounted_on(&Location { mount: dir.mount.clone(), node }).is_some() {
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
        if self.mounted_on(&Location { mount: source.mount.clone(), node: moving }).is_some() {
            return Err(Error::InUse);
        }
        if let Ok(replaced) = target.node.lookup(to_name)
            && self.mounted_on(&Location { mount: target.mount.clone(), node: replaced }).is_some()
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
        if at.node.stat().kind != Kind::Directory {
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
        mem::push(&mut mounts, mount.clone())?;
        self.count.store(mounts.len(), Ordering::Release);
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
        self.count.store(mounts.len(), Ordering::Release);
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
        self.count.store(mounts.len(), Ordering::Release);
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
