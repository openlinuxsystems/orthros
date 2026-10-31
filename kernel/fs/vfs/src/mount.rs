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
//!
//! A view ([`crate::view`], ADR 0215) is a mount in no tree, of
//! directories the kernel makes, with places of the tree attached to some
//! of their names: a walk crosses onto what is attached as onto a mount's
//! root, through the same filter.

use core::fmt;
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use radix_sync::Mutex;

use crate::attributes::{self, Inherited};
use crate::mem::{self, KArc, KVec};
use crate::path::{self, Component};
use crate::{
    Access, Error, FsRef, Kind, ListFn, MAX_LINKS, NodeRef, PATH_MAX, Result, Stat, Who, address,
    mode, want,
};

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
    /// Where it is in the tree, from the root, for `/processes/mounts`.
    path: KVec<u8>,
    /// Whether it is in no tree ([`Mount::detached`]).
    detached: bool,
    /// What it was mounted from: a device's name, or the filesystem's.
    source: KVec<u8>,
    /// Whether the disk it was mounted from is gone ([`Vfs::lose`]).
    lost: AtomicBool,
    /// The filter of its nodes that something is mounted on: bit
    /// [`filter_bit`] of each is set. Changed only under the table's lock;
    /// a bit is set before its mount is in the table, and cleared after it
    /// has left. A view's are those of its attached places, set under its
    /// own lock.
    covered: [AtomicU64; FILTER_WORDS],
    /// What it attaches, if it is a view's.
    pub(crate) view: Option<crate::view::View>,
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
            detached: true,
            source: mem::copy(source.as_bytes())?,
            lost: AtomicBool::new(false),
            covered: [const { AtomicU64::new(0) }; FILTER_WORDS],
            view: None,
        })?)
    }

    /// A view's mount of `fs`, in no tree and read-only to everything but
    /// the view's own calls ([`crate::view`]).
    pub(crate) fn of_view(fs: FsRef, view: crate::view::View) -> Result<MountRef> {
        let root = fs.root();
        Ok(mem::arc(Mount {
            id: crate::next_id(),
            fs,
            root,
            on: None,
            read_only: true,
            path: mem::vec(),
            detached: true,
            source: mem::copy(b"view")?,
            lost: AtomicBool::new(false),
            covered: [const { AtomicU64::new(0) }; FILTER_WORDS],
            view: Some(view),
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

    /// Whether it is in no tree ([`Mount::detached`]).
    pub(crate) fn is_detached(&self) -> bool {
        self.detached
    }

    /// Marks its node at `address` as one something may be on.
    pub(crate) fn cover(&self, address: usize) {
        let (word, bit) = filter_bit(address);
        self.covered[word].fetch_or(bit, Ordering::Release);
    }
}

/// A place in the tree.
#[derive(Clone)]
pub struct Location {
    /// The mount it is on.
    pub mount: MountRef,
    /// The node.
    pub node: NodeRef,
    /// Whether it was reached through a view's read-only entry, which
    /// makes it, and every place beneath it, read-only too (ADR 0215).
    pub(crate) read_only: bool,
}

impl Location {
    /// `node` of `mount`.
    pub fn new(mount: MountRef, node: NodeRef) -> Location {
        Location { mount, node, read_only: false }
    }

    /// The node `node` beneath it, on its mount: read-only if it is.
    fn beneath(&self, node: NodeRef) -> Location {
        Location { mount: self.mount.clone(), node, read_only: self.read_only }
    }

    /// Whether nothing may be written here: its mount is read-only, or it
    /// was reached through a view's read-only entry.
    pub fn read_only(&self) -> bool {
        self.read_only || self.mount.read_only()
    }

    /// Whether it is a view's own directory or file, not a place one
    /// attaches (ADR 0215).
    pub fn in_view(&self) -> bool {
        self.mount.view.is_some()
    }

    /// Whether it is a view's root.
    fn is_view_root(&self) -> bool {
        self.in_view() && address(&self.node) == address(&self.mount.root)
    }

    /// Whether it is the same place as `other`.
    pub fn same(&self, other: &Location) -> bool {
        KArc::ptr_eq(&self.mount, &other.mount) && address(&self.node) == address(&other.node)
    }

    /// Its path from the tree's root if it is the root of a mount in the
    /// tree, whose path the tree keeps: `""` for the tree's root.
    pub fn mount_path(&self) -> Option<&[u8]> {
        (address(&self.node) == address(&self.mount.root) && !self.mount.detached)
            .then_some(&self.mount.path[..])
    }

    /// Fails with [`Error::ReadOnly`] if its mount, or the node itself, is
    /// read-only.
    pub fn writable(&self) -> Result<()> {
        if self.read_only() || self.node.read_only() { Err(Error::ReadOnly) } else { Ok(()) }
    }
}

/// What a node of `kind` that `who` makes in directory `dir` with
/// `access` gets, and what it takes of the directory's default access
/// list, to set once it is made ([`Made::finish`]). Its mode loses the
/// bits of `who`'s file mode mask, unless the directory has a default
/// list, which the mode is masked with in its place, as Linux's
/// `posix_acl_create` has it (ADR 0256); a symbolic link's mode stays
/// 0777. In a directory with the set-group-ID bit, as POSIX lets Linux
/// have it, it takes the directory's group, and a directory that bit
/// too, so that a shared folder's files all take its group.
pub fn made_with(who: &Who, dir: &Location, access: Access, kind: Kind) -> Result<Made> {
    let parent = dir.node.access();
    let mut access = access;
    let mut inherited = Inherited::default();
    if kind != Kind::Symlink {
        match attributes::default_list(&*dir.node)? {
            Some(default) => {
                let (mode, takes) = Inherited::from(&default, kind, access.mode)?;
                access.mode = mode;
                inherited = takes;
            }
            None => access.mode &= !(who.file_mask & 0o777),
        }
    }
    if parent.mode & mode::SET_GROUP != 0 {
        let extra = if kind == Kind::Directory { mode::SET_GROUP } else { 0 };
        access = Access { group: parent.group, mode: access.mode | extra, ..access };
    }
    Ok(Made { access, inherited })
}

/// What [`made_with`] gives a node about to be made: its access, and what
/// it takes of its directory's default access list.
pub struct Made {
    /// Its owner, group and mode.
    pub access: Access,
    inherited: Inherited,
}

impl Made {
    /// Sets on `node`, just made as `name` in `dir`, the access lists it
    /// takes. If that fails the node goes again, as Linux makes none it
    /// cannot give its lists: without them its group bits, which are the
    /// list's mask, would be its group's.
    pub fn finish(&self, dir: &dyn crate::Node, name: &str, node: &NodeRef) -> Result<()> {
        if self.inherited.is_empty() {
            return Ok(());
        }
        self.inherited.apply(&**node).inspect_err(|_| {
            let _ = dir.remove(name);
        })
    }
}

/// Whether `who` may look names up in `node`: search it, if it is a
/// directory. In anything else the lookup fails with not-a-directory, as
/// on Linux, whatever its mode.
#[inline]
fn may_search(who: &Who, node: &dyn crate::Node) -> Result<()> {
    if who.bypass || node.kind() != Kind::Directory {
        return Ok(());
    }
    who.check(node, want::EXECUTE)
}

/// Whether `who` may change the entries of the directory at `dir`: write
/// and search it.
fn may_change(who: &Who, dir: &Location) -> Result<()> {
    who.check(&*dir.node, want::WRITE | want::EXECUTE)
}

/// Whether `who` may remove `node` from the directory at `dir`: change
/// the directory, and in a sticky one, own the node or the directory, as
/// POSIX has it (ADR 0214).
fn may_remove(who: &Who, dir: &Location, node: &dyn crate::Node) -> Result<()> {
    may_change(who, dir)?;
    if who.bypass {
        return Ok(());
    }
    let parent = dir.node.access();
    if parent.mode & mode::STICKY != 0 && !who.owns(parent) && !who.owns(node.access()) {
        return Err(Error::NotPermitted);
    }
    Ok(())
}

/// How `open` opens.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct OpenFlags {
    /// Make the file if it is not there, with this owner, group and mode
    /// ([`made_with`]); a directory there fails with
    /// [`Error::IsDirectory`].
    pub create: Option<Access>,
    /// With `create`: fail if it is there.
    pub exclusive: bool,
    /// Cut the file to nothing; a directory fails with
    /// [`Error::IsDirectory`].
    pub truncate: bool,
    /// Fail unless it is a directory.
    pub directory: bool,
    /// Fail with [`Error::Loop`] if the last name is a symbolic link,
    /// rather than follow it.
    pub no_follow: bool,
    /// What the opener will do with a file that is there, as [`want`]'s
    /// bits, which the second gate checks (ADR 0214).
    pub want: u16,
}

/// The tree of mounted filesystems.
pub struct Vfs {
    root: Location,
    mounts: Mutex<KVec<MountRef>>,
}

/// One step of a resolution: where it is, and where in the walk's names
/// is the name that led there.
type Step = (Location, core::ops::Range<usize>);

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
            detached: false,
            source: mem::copy(source.as_bytes())?,
            lost: AtomicBool::new(false),
            covered: [const { AtomicU64::new(0) }; FILTER_WORDS],
            view: None,
        })?;
        let mut mounts = mem::vec();
        mem::push(&mut mounts, mount.clone())?;
        Ok(Vfs { root: Location::new(mount, root), mounts: Mutex::new(mounts) })
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
        Some(Location::new(top.clone(), top.root.clone()))
    }

    /// What is on `node` of `mount`: the root of what is mounted there,
    /// and so on up a stack of mounts; or, on a view's, the place it
    /// attached there, read-only if attached so, or what is mounted on
    /// that place since. None if nothing is.
    fn over(&self, mount: &Mount, node: &NodeRef) -> Option<Location> {
        let Some(view) = &mount.view else {
            return self.mounted_root(mount, node);
        };
        if !mount.may_cover(address(node)) {
            return None;
        }
        let attached = view.attached(address(node))?;
        Some(match self.mounted_root(&attached.mount, &attached.node) {
            Some(top) => Location { read_only: attached.read_only, ..top },
            None => attached,
        })
    }

    /// `at`, or what is on it ([`Vfs::over`]), read-only if `at` is.
    fn cross(&self, at: Location) -> Location {
        match self.over(&at.mount, &at.node) {
            Some(top) => Location { read_only: top.read_only || at.read_only, ..top },
            None => at,
        }
    }

    /// Walks `path` from `base`, calling `f` with the chain of steps: the
    /// places it passed, each with the name that led there. A symbolic
    /// link on the way is followed, as is one that is the last name when
    /// `follow` is set or a `/` follows it: the path goes on from the
    /// link's directory with the link's target in its place, or, for a
    /// target that starts with `/`, from `base`, as if `base` were the
    /// root. At most [`MAX_LINKS`] are followed. `..` steps back along the
    /// chain; at `base` it stays there when `base` is the tree's root or a
    /// view's, as `/..` is `/`, and is refused anywhere else. Each
    /// directory it looks in must let `who` search it (ADR 0214).
    fn walk<T>(
        &self,
        who: &Who,
        base: &Location,
        path: &str,
        follow: bool,
        f: impl FnOnce(&[Step], &[u8]) -> Result<T>,
    ) -> Result<T> {
        let mut chain: KVec<Step> = mem::vec_with_capacity(8)?;
        let mut names: KVec<u8> = mem::vec();
        mem::push(&mut chain, (base.clone(), 0..0))?;
        let at_root = base.same(&self.root) || base.is_view_root();
        // The path left to walk, once a link has replaced what was walked.
        let mut owned: Option<KVec<u8>> = None;
        let mut links = 0;
        'walk: loop {
            let text = match &owned {
                // Built from the path and links' targets, which are text.
                Some(bytes) => core::str::from_utf8(bytes).map_err(|_| Error::BadName)?,
                None => path,
            };
            let mut components = path::components(text);
            while let Some(component) = components.next() {
                let component = component?;
                if let Some((here, _)) = chain.last() {
                    may_search(who, &*here.node)?;
                }
                match component {
                    Component::Current => {}
                    Component::Parent if chain.len() == 1 && at_root => {}
                    Component::Parent if chain.len() == 1 => return Err(Error::Outside),
                    Component::Parent => {
                        if let Some((_, name)) = chain.pop() {
                            names.truncate(name.start);
                        }
                    }
                    Component::Name(name) => {
                        let Some((here, _)) = chain.last() else { return Err(Error::Outside) };
                        let node = here.node.lookup(name)?;
                        let last = components.at_end();
                        if node.kind() == Kind::Symlink
                            && (!last || follow || components.trailing_slash())
                        {
                            links += 1;
                            if links > MAX_LINKS {
                                return Err(Error::Loop);
                            }
                            let target = crate::link_target(&*node)?;
                            let rest = components.rest().as_bytes();
                            if target.len() + rest.len() > PATH_MAX {
                                return Err(Error::NameTooLong);
                            }
                            let mut next = mem::vec_with_capacity(target.len() + rest.len())?;
                            mem::extend(&mut next, &target)?;
                            mem::extend(&mut next, rest)?;
                            if target.first() == Some(&b'/') {
                                chain.truncate(1);
                                names.clear();
                            }
                            owned = Some(next);
                            continue 'walk;
                        }
                        let next = self.cross(here.beneath(node));
                        let start = names.len();
                        mem::extend(&mut names, name.as_bytes())?;
                        mem::push(&mut chain, (next, start..names.len()))?;
                    }
                }
            }
            return f(&chain, &names);
        }
    }

    /// Where `path` leads from `base`, following symbolic links ([`walk`]),
    /// for `who`.
    ///
    /// [`walk`]: Vfs::walk
    pub fn resolve(&self, who: &Who, base: &Location, path: &str) -> Result<Location> {
        self.resolve_as(who, base, path, true)
    }

    /// Where `path` leads from `base`, following symbolic links but for
    /// one that is its last name (unless a `/` follows it): what `lstat`
    /// and `readlink` look at.
    pub fn resolve_link(&self, who: &Who, base: &Location, path: &str) -> Result<Location> {
        self.resolve_as(who, base, path, false)
    }

    fn resolve_as(&self, who: &Who, base: &Location, path: &str, follow: bool) -> Result<Location> {
        // `.` alone is `base` itself, as a descriptor's own directory is
        // to fstat(2): it needs no search of it.
        if path == "." {
            return Ok(base.clone());
        }
        // Where the walk is, when it has left `base`: a name moves the
        // node on, and a mount there the mount too; a view's read-only
        // entry makes the rest read-only.
        let mut mount: Option<MountRef> = None;
        let mut node: Option<NodeRef> = None;
        let mut read_only = base.read_only;
        let mut components = path::components(path);
        while let Some(component) = components.next() {
            let component = component?;
            let here = node.as_ref().unwrap_or(&base.node);
            may_search(who, &**here)?;
            match component {
                Component::Current => {}
                // Only `..` and symbolic links need the chain: `..` to
                // step back along, and a link to start again from.
                Component::Parent => return self.walk_to_end(who, base, path, follow),
                Component::Name(name) => {
                    let found = here.lookup(name)?;
                    if found.kind() == Kind::Symlink
                        && (!components.at_end() || follow || components.trailing_slash())
                    {
                        return self.walk_to_end(who, base, path, follow);
                    }
                    match self.over(mount.as_ref().unwrap_or(&base.mount), &found) {
                        Some(top) => {
                            read_only |= top.read_only;
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
            read_only,
        })
    }

    /// Where the walk of `path` from `base` ends.
    fn walk_to_end(
        &self,
        who: &Who,
        base: &Location,
        path: &str,
        follow: bool,
    ) -> Result<Location> {
        self.walk(who, base, path, follow, |chain, _| {
            chain.last().map(|(at, _)| at.clone()).ok_or(Error::Outside)
        })
    }

    /// The directory `path`'s last name is in, and that name.
    pub fn resolve_parent<'p>(
        &self,
        who: &Who,
        base: &Location,
        path: &'p str,
    ) -> Result<(Location, &'p str)> {
        let (dir, name) = path::split_last(path)?;
        let at = self.resolve(who, base, dir)?;
        if at.node.kind() != Kind::Directory {
            return Err(Error::NotDirectory);
        }
        Ok((at, name))
    }

    /// Opens `path` beneath `base` for `who`, following symbolic links
    /// unless `flags.no_follow` says not to follow one that is the last
    /// name. A file made through a symbolic link is made where the link
    /// points, as POSIX has it. One that is there must let `who` do what
    /// `flags.want` asks, and write it to truncate it; one the open makes
    /// is `who`'s whatever its mode, and its directory must let `who`
    /// change it.
    pub fn open(
        &self,
        who: &Who,
        base: &Location,
        path: &str,
        flags: OpenFlags,
    ) -> Result<Location> {
        let mut made = false;
        let at = if let Some(access) = flags.create {
            // The path again with the link's target for its last name,
            // once a link stood there.
            let mut owned: Option<KVec<u8>> = None;
            let mut links = 0;
            loop {
                let text = match &owned {
                    Some(bytes) => core::str::from_utf8(bytes).map_err(|_| Error::BadName)?,
                    None => path,
                };
                let (dir, name) = self.resolve_parent(who, base, text)?;
                match dir.node.lookup(name) {
                    Ok(_) if flags.exclusive => return Err(Error::Exists),
                    Ok(node) if node.kind() == Kind::Symlink => {
                        links += 1;
                        if flags.no_follow || links > MAX_LINKS {
                            return Err(Error::Loop);
                        }
                        let target = crate::link_target(&*node)?;
                        owned = Some(replace_last(text, &target)?);
                    }
                    Ok(node) => break self.cross(dir.beneath(node)),
                    Err(Error::NotFound) => {
                        dir.writable()?;
                        who.check(&*dir.node, want::WRITE | want::EXECUTE)?;
                        let made_as = made_with(who, &dir, access, Kind::File)?;
                        let node = dir.node.create(name, Kind::File, made_as.access)?;
                        made_as.finish(&*dir.node, name, &node)?;
                        made = true;
                        break dir.beneath(node);
                    }
                    Err(err) => return Err(err),
                }
            }
        } else {
            self.resolve_as(who, base, path, !flags.no_follow)?
        };
        let kind = at.node.kind();
        if kind == Kind::Symlink {
            return Err(Error::Loop);
        }
        // Open neither makes nor cuts a directory, as POSIX has it.
        if kind == Kind::Directory && (flags.create.is_some() || flags.truncate) {
            return Err(Error::IsDirectory);
        }
        if flags.directory && kind != Kind::Directory {
            return Err(Error::NotDirectory);
        }
        // A directory is listed and changed through, each checked then.
        if !made && kind != Kind::Directory {
            let asked = flags.want | if flags.truncate { want::WRITE } else { 0 };
            if asked != 0 {
                who.check(&*at.node, asked)?;
            }
        }
        if flags.truncate && kind == Kind::File {
            at.writable()?;
            at.node.set_size(0)?;
        }
        let node = at.node.open()?.unwrap_or(at.node);
        Ok(Location { mount: at.mount, node, read_only: at.read_only })
    }

    /// Makes a symbolic link `path` beneath `base` that points to
    /// `target`: any text of at most [`PATH_MAX`] bytes but the empty one,
    /// which is not looked at until the link is followed. It belongs to
    /// `access`'s owner and group, and its mode is 0777.
    pub fn make_link(
        &self,
        who: &Who,
        base: &Location,
        path: &str,
        target: &str,
        access: Access,
    ) -> Result<()> {
        if target.is_empty() || target.contains('\0') {
            return Err(Error::BadName);
        }
        if target.len() > PATH_MAX {
            return Err(Error::NameTooLong);
        }
        let (dir, name) = self.resolve_parent(who, base, path)?;
        // In Linux's order of errors, so `mkdir -p` passes what is there.
        if dir.node.lookup(name).is_ok() {
            return Err(Error::Exists);
        }
        dir.writable()?;
        may_change(who, &dir)?;
        let made = made_with(who, &dir, Access { mode: 0o777, ..access }, Kind::Symlink)?;
        dir.node.make_link(name, target, made.access).map(drop)
    }

    /// Makes `to` beneath `to_base` another name of the file `path`
    /// beneath `base` ([`crate::Node::link`]): of the link itself when it
    /// is a symbolic link, unless `follow`, as `linkat` has it. Each side
    /// is resolved for its own name, `who` and `to_who`, as each directory
    /// capability acts for its own; `who` must own the file or may read
    /// and write it, as Linux's `fs.protected_hardlinks` has it, and
    /// `to_who` change the directory the new name goes in.
    #[allow(clippy::too_many_arguments)]
    pub fn link(
        &self,
        who: &Who,
        base: &Location,
        path: &str,
        to_who: &Who,
        to_base: &Location,
        to: &str,
        follow: bool,
    ) -> Result<()> {
        let from = self.resolve_as(who, base, path, follow)?;
        let (dir, name) = self.resolve_parent(to_who, to_base, to)?;
        // In Linux's order of errors.
        if dir.node.lookup(name).is_ok() {
            return Err(Error::Exists);
        }
        dir.writable()?;
        if !KArc::ptr_eq(&from.mount, &dir.mount) {
            return Err(Error::CrossMount);
        }
        may_change(to_who, &dir)?;
        if from.node.kind() == Kind::Directory {
            return Err(Error::IsDirectory);
        }
        if !who.bypass {
            let access = from.node.access();
            let usable = from.node.kind() == Kind::File
                && who.check(&*from.node, want::READ | want::WRITE).is_ok();
            if !who.owns(access) && !usable {
                return Err(Error::NotPermitted);
            }
        }
        dir.node.link(name, &from.node)
    }

    /// Makes `path` beneath `base` a node of `kind` that stands for
    /// device number `device` ([`crate::Node::make_node`]), with `access`.
    pub fn make_node(
        &self,
        who: &Who,
        base: &Location,
        path: &str,
        kind: Kind,
        device: u64,
        access: Access,
    ) -> Result<()> {
        let (dir, name) = self.resolve_parent(who, base, path)?;
        // In Linux's order of errors, so `mkdir -p` passes what is there.
        if dir.node.lookup(name).is_ok() {
            return Err(Error::Exists);
        }
        dir.writable()?;
        may_change(who, &dir)?;
        let made = made_with(who, &dir, access, kind)?;
        let node = dir.node.make_node(name, kind, device, made.access)?;
        made.finish(&*dir.node, name, &node)
    }

    /// Makes a socket's name `path` beneath `base`, which holds `binding`
    /// ([`crate::Node::make_socket`]), with `access`.
    pub fn make_socket(
        &self,
        who: &Who,
        base: &Location,
        path: &str,
        binding: crate::SocketBinding,
        access: Access,
    ) -> Result<()> {
        let (dir, name) = self.resolve_parent(who, base, path)?;
        // In Linux's order of errors, so `mkdir -p` passes what is there.
        if dir.node.lookup(name).is_ok() {
            return Err(Error::Exists);
        }
        dir.writable()?;
        may_change(who, &dir)?;
        let made = made_with(who, &dir, access, Kind::Socket)?;
        let node = dir.node.make_socket(name, binding, made.access)?;
        made.finish(&*dir.node, name, &node)
    }

    /// Reads where the symbolic link `path` beneath `base` points into
    /// `out`; returns the target's length, more than `out` holds when it
    /// was cut short.
    pub fn read_link(
        &self,
        who: &Who,
        base: &Location,
        path: &str,
        out: &mut dyn crate::Out,
    ) -> Result<usize> {
        self.resolve_link(who, base, path)?.node.read_link(out)
    }

    /// Writes where `path` beneath `base` leads into `out`: the names
    /// from `base` that the walk took, every symbolic link followed and
    /// `.` and `..` worked out, joined by `/` (`.` for `base` itself), as
    /// realpath(3) wants. Returns its length, which is more than `out`
    /// holds if it was cut short.
    pub fn real_path(
        &self,
        who: &Who,
        base: &Location,
        path: &str,
        out: &mut dyn crate::Out,
    ) -> Result<usize> {
        self.walk(who, base, path, true, |chain, names| {
            let steps = &chain[1..];
            if steps.is_empty() {
                put_within(out, 0, b".")?;
                return Ok(1);
            }
            let mut len = 0;
            for (i, (_, name)) in steps.iter().enumerate() {
                if i > 0 {
                    put_within(out, len, b"/")?;
                    len += 1;
                }
                put_within(out, len, &names[name.clone()])?;
                len += name.len();
            }
            Ok(len)
        })
    }

    /// Makes the directory `path` beneath `base`, with `access`.
    pub fn make_directory(
        &self,
        who: &Who,
        base: &Location,
        path: &str,
        access: Access,
    ) -> Result<()> {
        let (dir, name) = self.resolve_parent(who, base, path)?;
        // In Linux's order of errors, so `mkdir -p` passes what is there,
        // a place attached to a view among them.
        if dir.node.lookup(name).is_ok() {
            return Err(Error::Exists);
        }
        dir.writable()?;
        may_change(who, &dir)?;
        let made = made_with(who, &dir, access, Kind::Directory)?;
        let node = dir.node.create(name, Kind::Directory, made.access)?;
        made.finish(&*dir.node, name, &node)
    }

    /// Removes the file or empty directory `path` beneath `base`.
    pub fn remove(&self, who: &Who, base: &Location, path: &str) -> Result<()> {
        let (dir, name) = self.resolve_parent(who, base, path)?;
        dir.writable()?;
        let node = dir.node.lookup(name)?;
        may_remove(who, &dir, &*node)?;
        let mounted = self.mounted_on(&dir.mount, &node).is_some();
        // Let go of it first: held here, a file no one has open would look
        // open to its filesystem, which keeps an open file past its last
        // name rather than freeing it.
        drop(node);
        if mounted {
            return Err(Error::InUse);
        }
        dir.node.remove(name)
    }

    /// Makes `path` beneath `base` a new, empty subvolume
    /// ([`crate::Node::make_subvolume`]).
    pub fn make_subvolume(&self, who: &Who, base: &Location, path: &str) -> Result<()> {
        let (dir, name) = self.resolve_parent(who, base, path)?;
        dir.writable()?;
        if dir.node.lookup(name).is_ok() {
            return Err(Error::Exists);
        }
        may_change(who, &dir)?;
        dir.node.make_subvolume(name)
    }

    /// Makes `to` beneath `to_base` a snapshot of the subvolume that holds
    /// `source` beneath `base`, read-only if `read_only`
    /// ([`crate::Node::snapshot`]). `who` must own `source`, as on Linux,
    /// and `to_who` change the directory the snapshot goes in.
    #[allow(clippy::too_many_arguments)]
    pub fn snapshot(
        &self,
        who: &Who,
        base: &Location,
        source: &str,
        to_who: &Who,
        to_base: &Location,
        to: &str,
        read_only: bool,
    ) -> Result<()> {
        let from = self.resolve(who, base, source)?;
        let (dir, name) = self.resolve_parent(to_who, to_base, to)?;
        if dir.node.lookup(name).is_ok() {
            return Err(Error::Exists);
        }
        dir.writable()?;
        may_change(to_who, &dir)?;
        if !who.owns(from.node.access()) {
            return Err(Error::NotPermitted);
        }
        // On one filesystem, through whichever mounts of it, as Linux has
        // it: the filesystem checks.
        dir.node.snapshot(name, &from.node, read_only)
    }

    /// Removes the subvolume `path` beneath `base`, with everything in it
    /// ([`crate::Node::remove_subvolume`]). `who` must be able to remove
    /// it from its directory, and to write and search it, as Linux's
    /// `user_subvol_rm_allowed` has it.
    pub fn remove_subvolume(&self, who: &Who, base: &Location, path: &str) -> Result<()> {
        let (dir, name) = self.resolve_parent(who, base, path)?;
        dir.writable()?;
        let node = dir.node.lookup(name)?;
        may_remove(who, &dir, &*node)?;
        who.check(&*node, want::WRITE | want::EXECUTE)?;
        let mounted = self.mounted_on(&dir.mount, &node).is_some();
        // Let go of it first: held here, the subvolume would be in use.
        drop(node);
        if mounted {
            return Err(Error::InUse);
        }
        dir.node.remove_subvolume(name)
    }

    /// Moves `from` beneath `base` to `to` beneath `to_base`, on one
    /// filesystem, each side resolved for its own name. `who` must be able
    /// to remove it from its directory, and to write a directory it moves
    /// to another, for its `..`, and `to_who` to remove what it replaces
    /// from the other, or to change it, as on Linux.
    pub fn rename(
        &self,
        who: &Who,
        base: &Location,
        from: &str,
        to_who: &Who,
        to_base: &Location,
        to: &str,
    ) -> Result<()> {
        let (source, from_name) = self.resolve_parent(who, base, from)?;
        let (target, to_name) = self.resolve_parent(to_who, to_base, to)?;
        if !KArc::ptr_eq(&source.mount, &target.mount) {
            return Err(Error::CrossMount);
        }
        source.writable()?;
        let moving = source.node.lookup(from_name)?;
        if self.mounted_on(&source.mount, &moving).is_some() {
            return Err(Error::InUse);
        }
        let replaced = target.node.lookup(to_name).ok();
        if let Some(replaced) = &replaced
            && self.mounted_on(&target.mount, replaced).is_some()
        {
            return Err(Error::InUse);
        }
        if !(who.bypass && to_who.bypass) {
            may_remove(who, &source, &*moving)?;
            match &replaced {
                Some(replaced) => may_remove(to_who, &target, &**replaced)?,
                None => may_change(to_who, &target)?,
            }
            if moving.kind() == Kind::Directory && address(&source.node) != address(&target.node) {
                who.check(&*moving, want::WRITE)?;
            }
        }
        drop((moving, replaced));
        source.node.rename(from_name, &*target.node, to_name)
    }

    /// What `path` beneath `base` is.
    pub fn stat(&self, who: &Who, base: &Location, path: &str) -> Result<Stat> {
        Ok(self.resolve(who, base, path)?.node.stat())
    }

    /// What `path` beneath `base` is, the link itself if it is a symbolic
    /// link.
    pub fn stat_link(&self, who: &Who, base: &Location, path: &str) -> Result<Stat> {
        Ok(self.resolve_link(who, base, path)?.node.stat())
    }

    /// Lists the directory at `at` from `cursor`, which `who` must be able
    /// to read.
    pub fn list(&self, who: &Who, at: &Location, cursor: u64, f: ListFn<'_>) -> Result<()> {
        who.check(&*at.node, want::READ)?;
        at.node.list(cursor, f)
    }

    /// Mounts `fs` on the directory `path` beneath `base`.
    pub fn mount(
        &self,
        who: &Who,
        base: &Location,
        path: &str,
        fs: FsRef,
        source: &str,
        read_only: bool,
    ) -> Result<MountRef> {
        let root = fs.root();
        self.mount_at(who, base, path, fs, root, source, read_only)
    }

    /// Mounts the directory `from` on the directory `path` beneath `base`,
    /// as Linux's bind mounts do: the same files, from another place. The
    /// mount is read-only if `read_only`, or if `from`'s is.
    pub fn bind(
        &self,
        who: &Who,
        base: &Location,
        path: &str,
        from: &Location,
        read_only: bool,
    ) -> Result<MountRef> {
        if from.node.kind() != Kind::Directory {
            return Err(Error::NotDirectory);
        }
        if from.in_view() {
            return Err(Error::NotSupported);
        }
        let read_only = read_only || from.read_only();
        let (fs, root) = (from.mount.fs.clone(), from.node.clone());
        let mount = self.mount_at(who, base, path, fs, root, from.mount.source(), read_only)?;
        if from.mount.lost.load(Ordering::Acquire) {
            mount.lost.store(true, Ordering::Release);
        }
        Ok(mount)
    }

    /// Mounts `fs`, its directory `root` at the top, on the directory
    /// `path` beneath `base`.
    #[allow(clippy::too_many_arguments)]
    fn mount_at(
        &self,
        who: &Who,
        base: &Location,
        path: &str,
        fs: FsRef,
        root: NodeRef,
        source: &str,
        read_only: bool,
    ) -> Result<MountRef> {
        // A view's places are the tree's, which a view does not change.
        if base.in_view() {
            return Err(Error::NotSupported);
        }
        let (at, where_) = self.walk(who, base, path, true, |chain, names| {
            let at = chain.last().map(|(at, _)| at.clone()).ok_or(Error::Outside)?;
            let mut where_ = mem::vec();
            prefix_of(base, &mut where_)?;
            for (_, name) in &chain[1..] {
                if !where_.is_empty() {
                    mem::push(&mut where_, b'/')?;
                }
                mem::extend(&mut where_, &names[name.clone()])?;
            }
            Ok((at, where_))
        })?;
        if at.node.kind() != Kind::Directory {
            return Err(Error::NotDirectory);
        }
        let mount = mem::arc(Mount {
            id: crate::next_id(),
            fs,
            root,
            on: Some((at.mount.clone(), at.node.clone())),
            read_only,
            path: where_,
            detached: false,
            source: mem::copy(source.as_bytes())?,
            lost: AtomicBool::new(false),
            covered: [const { AtomicU64::new(0) }; FILTER_WORDS],
            view: None,
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

    /// Marks each mount from a device `gone` says is gone as lost: its
    /// disk is, and nothing it holds can be written any more, so a sync
    /// that fails no longer keeps it mounted ([`Vfs::unmount`]), as
    /// Linux's umount does not keep a filesystem whose disk was pulled
    /// out. Returns how many there were.
    pub fn lose(&self, gone: impl Fn(&str) -> bool) -> usize {
        let mounts = self.mounts.lock();
        let mut lost = 0;
        for mount in mounts.iter().filter(|m| gone(m.source())) {
            mount.lost.store(true, Ordering::Release);
            lost += 1;
        }
        lost
    }

    /// Syncs `mount` before it is unmounted: a lost one ([`Vfs::lose`])
    /// is unmounted whether or not what it held could be written.
    fn sync_before_unmount(mount: &Mount) -> Result<()> {
        match mount.fs.sync() {
            Err(_) if mount.lost.load(Ordering::Acquire) => Ok(()),
            done => done,
        }
    }

    /// Unmounts the filesystem whose root `path` beneath `base` is, after
    /// syncing it. Fails with [`Error::InUse`] while anything is mounted
    /// on it or any handle holds a place on it, and with the sync's error
    /// if it fails, unless the mount's disk is gone.
    pub fn unmount(&self, who: &Who, base: &Location, path: &str) -> Result<()> {
        if base.in_view() {
            return Err(Error::NotSupported);
        }
        let at = self.resolve(who, base, path)?;
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
        Self::sync_before_unmount(&mount)?;
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
    pub fn unmount_tree(&self, who: &Who, base: &Location, path: &str) -> Result<()> {
        if base.in_view() {
            return Err(Error::NotSupported);
        }
        let at = self.resolve(who, base, path)?;
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
        for mount in &doomed {
            let on_it = doomed
                .iter()
                .filter(|m| m.on.as_ref().is_some_and(|(on, _)| KArc::ptr_eq(on, mount)))
                .count();
            let expected = 2 + on_it + usize::from(KArc::ptr_eq(mount, &top));
            if KArc::strong_count(mount) > expected {
                return Err(Error::InUse);
            }
        }
        for mount in &doomed {
            Self::sync_before_unmount(mount)?;
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
        for fs in &self.filesystems()? {
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
        for fs in &filesystems {
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

/// Puts what of `bytes` fits in `out` at `at`.
fn put_within(out: &mut dyn crate::Out, at: usize, bytes: &[u8]) -> Result<()> {
    let room = out.len().saturating_sub(at).min(bytes.len());
    if room > 0 { out.put(at, &bytes[..room]) } else { Ok(()) }
}

/// `path` with its last name replaced by `target`, the target of the
/// symbolic link that name is: the directory's path and the target, or
/// the target alone when it starts with `/`.
fn replace_last(path: &str, target: &[u8]) -> Result<KVec<u8>> {
    let dir = match path.trim_end_matches('/').rfind('/') {
        Some(at) if target.first() != Some(&b'/') => &path[..=at],
        _ => "",
    };
    if dir.len() + target.len() > PATH_MAX {
        return Err(Error::NameTooLong);
    }
    let mut out = mem::vec_with_capacity(dir.len() + target.len())?;
    mem::extend(&mut out, dir.as_bytes())?;
    mem::extend(&mut out, target)?;
    Ok(out)
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
