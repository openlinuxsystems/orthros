// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Views (ADR 0215): directories made of attached places, as the security
//! design's view object.
//!
//! A view is a small tree of directories the kernel makes, on a mount of
//! its own that is in no tree, with places of the tree (directories and
//! files that capabilities hold) attached to some of its names and
//! symbolic links at others, as a private mount namespace of bind mounts
//! is on Linux. A walk crosses onto an attached place as onto a mount's
//! root ([`Vfs::walk`]), so a program whose root is a view sees real paths
//! (`/tmp`, `/system/libraries`) and nothing past them: `..` goes back
//! along the walk, `..` at the view's root stays there, a link's target
//! that starts with `/` starts again at the view's root, and `realpath`
//! gives the names in the view.
//!
//! A view acts for one name, the one of the capability it was made from,
//! and takes only places reached through a capability acting for the same
//! name, so that nothing in it is reached as another user; made with the
//! bypass right, it takes only places reached with it too. A place
//! attached without the write right is read-only in the view, and every
//! place beneath it. Nothing is ever detached: a view goes when the last
//! capability to it does.
//!
//! [`Vfs::walk`]: crate::mount::Vfs

use radix_sync::Mutex;

use crate::mem::{self, KVec};
use crate::mount::{Location, Mount};
use crate::path::{self, Component};
use crate::tmpfs::Tmpfs;
use crate::{Access, Error, FsRef, Kind, NodeRef, Result, Who, address};

/// The bytes a view's links' targets may take together.
const VIEW_BYTES: u64 = 64 << 10;
/// The directories, attached places and links a view may hold, its root
/// included.
const VIEW_FILES: u64 = 512;

/// What a view attaches, and the name it acts for.
pub struct View {
    /// The user, group and other groups of the name it acts for.
    user: u32,
    group: u32,
    groups: KVec<u32>,
    /// Whether it was made with the bypass right.
    bypass: bool,
    /// The places attached, each with the view's node it is on: kept so
    /// that the node, whose address finds it, stays while the view does.
    attached: Mutex<KVec<(NodeRef, Location)>>,
}

impl View {
    /// The place attached on the view's node at `address`, if any.
    pub(crate) fn attached(&self, address_of: usize) -> Option<Location> {
        let attached = self.attached.lock();
        attached.iter().find(|(on, _)| address(on) == address_of).map(|(_, at)| at.clone())
    }

    /// Whether `who` acts for the name the view does.
    fn same_name(&self, who: &Who) -> bool {
        who.user == self.user && who.group == self.group && who.groups == &self.groups[..]
    }
}

impl crate::mount::Vfs {
    /// Makes an empty view acting for `who`'s name, and with the bypass
    /// right if `who` has it; returns its root.
    pub fn make_view(&self, who: &Who) -> Result<Location> {
        let fs: FsRef = Tmpfs::new(VIEW_BYTES, VIEW_FILES, 0o555)?;
        let mut groups = mem::vec_with_capacity(who.groups.len())?;
        for &group in who.groups {
            let _ = groups.push_within_capacity(group);
        }
        let view = View {
            user: who.user,
            group: who.group,
            groups,
            bypass: who.bypass,
            attached: Mutex::new(mem::vec()),
        };
        let mount = Mount::of_view(fs, view)?;
        let root = mount.fs.root();
        Ok(Location::new(mount, root))
    }

    /// Attaches `what`, a directory, a file or a device node, at `path`
    /// beneath `dir`, a directory of a view, making the view's directories
    /// on the way: read-only if `read_only`. `who` is the name `what` was
    /// reached for, which must be the view's, with the bypass right if the
    /// view has it ([`Error::NotPermitted`]). Fails with
    /// [`Error::NotSupported`] if `dir` is not a view's or `what` cannot
    /// be attached (a view's own, a pipe's or a socket's, a link),
    /// [`Error::Exists`] if the name is taken, and [`Error::InUse`] if the
    /// path goes through a place attached already.
    pub fn attach(
        &self,
        dir: &Location,
        path: &str,
        what: &Location,
        who: &Who,
        read_only: bool,
    ) -> Result<()> {
        let view = dir.mount.view.as_ref().ok_or(Error::NotSupported)?;
        let kind @ (Kind::Directory | Kind::File | Kind::CharDevice | Kind::BlockDevice) =
            what.node.kind()
        else {
            return Err(Error::NotSupported);
        };
        if what.in_view() || what.mount.is_detached() {
            return Err(Error::NotSupported);
        }
        if !view.same_name(who) || (view.bypass && !who.bypass) {
            return Err(Error::NotPermitted);
        }
        let mode = if kind == Kind::Directory { 0o555 } else { 0o444 };
        let mut attached = view.attached.lock();
        attached.try_reserve(1).map_err(|_| Error::NoMemory)?;
        let on = place(&attached, dir, path, |at, name| {
            let access = Access { owner: 0, group: 0, mode };
            match kind {
                // A device's place is a node of its kind with no device:
                // were it ever opened rather than crossed, no_device.
                Kind::CharDevice | Kind::BlockDevice => at.make_node(name, kind, 0, access),
                _ => at.create(name, kind, access),
            }
        })?;
        let mut at = what.clone();
        at.read_only = at.read_only || read_only;
        // The bit first, as for a mount: a walk that finds it clear has
        // not seen the place either.
        dir.mount.cover(address(&on));
        let _ = attached.push_within_capacity((on, at));
        Ok(())
    }

    /// Syncs the filesystem of each place attached to the view `dir` is
    /// in, as sync(2) on a program whose root is a view means them;
    /// returns the first error.
    pub fn sync_view(&self, dir: &Location) -> Result<()> {
        let view = dir.mount.view.as_ref().ok_or(Error::NotSupported)?;
        // Copied out: a sync takes long, and the lock is the view's.
        let places = {
            let attached = view.attached.lock();
            let mut places = mem::vec_with_capacity(attached.len())?;
            for (_, at) in attached.iter() {
                let _ = places.push_within_capacity(at.mount.clone());
            }
            places
        };
        let mut first = Ok(());
        for (i, mount) in places.iter().enumerate() {
            // Each filesystem once, however many places are on it.
            if places[..i].iter().any(|m| crate::KArc::ptr_eq(&m.fs, &mount.fs)) {
                continue;
            }
            let synced = mount.fs.sync();
            if first.is_ok() {
                first = synced;
            }
        }
        first
    }

    /// Makes a symbolic link at `path` beneath `dir`, a directory of a
    /// view, to `target`, making the view's directories on the way, as
    /// [`Vfs::attach`] does.
    ///
    /// [`Vfs::attach`]: crate::mount::Vfs::attach
    pub fn attach_link(&self, dir: &Location, path: &str, target: &str) -> Result<()> {
        let view = dir.mount.view.as_ref().ok_or(Error::NotSupported)?;
        if target.is_empty() || target.contains('\0') {
            return Err(Error::BadName);
        }
        if target.len() > crate::PATH_MAX {
            return Err(Error::NameTooLong);
        }
        let attached = view.attached.lock();
        place(&attached, dir, path, |at, name| {
            at.make_link(name, target, Access { owner: 0, group: 0, mode: 0o777 })
        })
        .map(drop)
    }
}

/// Makes the last name of `path` beneath `dir` with `make`, and the
/// view's directories before it, under the view's lock, whose list of
/// places `attached` is. Returns the node made.
fn place(
    attached: &[(NodeRef, Location)],
    dir: &Location,
    path: &str,
    make: impl FnOnce(&NodeRef, &str) -> Result<NodeRef>,
) -> Result<NodeRef> {
    let is_place = |node: &NodeRef| attached.iter().any(|(on, _)| address(on) == address(node));
    let mut here = dir.node.clone();
    if is_place(&here) {
        return Err(Error::InUse);
    }
    let mut components = path::components(path).peekable();
    let mut last = None;
    while let Some(component) = components.next() {
        match component? {
            Component::Current => {}
            Component::Parent => return Err(Error::BadName),
            Component::Name(name) if components.peek().is_none() => last = Some(name),
            Component::Name(name) => {
                here = match here.lookup(name) {
                    Ok(node) if is_place(&node) => return Err(Error::InUse),
                    Ok(node) if node.kind() == Kind::Directory => node,
                    Ok(_) => return Err(Error::NotDirectory),
                    Err(Error::NotFound) => here.create(
                        name,
                        Kind::Directory,
                        Access { owner: 0, group: 0, mode: 0o555 },
                    )?,
                    Err(err) => return Err(err),
                };
            }
        }
    }
    let name = last.ok_or(Error::BadName)?;
    if here.lookup(name).is_ok() {
        return Err(Error::Exists);
    }
    make(&here, name)
}
