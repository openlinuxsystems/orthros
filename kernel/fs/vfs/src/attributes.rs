// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Extended attributes (ADR 0256): names with values a node keeps beside
//! its contents, as Linux's xattrs, read and changed with Linux's rules.
//!
//! A name starts with its namespace, and the namespace says who may read
//! and change it, as Linux's `xattr_permission` and the handlers of each
//! namespace have it:
//!
//! - `user.`: on a file or directory, read by whoever may read the node
//!   and changed by whoever may write it; in a sticky directory only its
//!   owner changes them.
//! - `trusted.`: the bypass right's alone, which the others do not see.
//! - `security.`: read by anyone, changed with the bypass right.
//! - `system.posix_acl_access` and `system.posix_acl_default`: the access
//!   lists ([`crate::acl`]), read by anyone and changed by the owner or
//!   with the bypass right; setting a list sets the mode with it.
//!
//! Any other name is not supported. Filesystems keep the values; FAT and
//! exFAT keep none.

use crate::acl::{self, Acl};
use crate::mem::{self, KArc, KVec};
use crate::mount::Location;
use crate::{AccessChange, Error, Kind, Node, Result, Setting, Who, mode, want};

/// The longest name, in bytes: Linux's XATTR_NAME_MAX.
pub const NAME_MAX: usize = 255;

/// The longest value, in bytes: Linux's XATTR_SIZE_MAX.
pub const VALUE_MAX: usize = 65536;

/// The longest list of names, in bytes: Linux's XATTR_LIST_MAX.
pub const LIST_MAX: usize = 65536;

/// The namespace of a name.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Space {
    User,
    Trusted,
    Security,
    Access,
    Default,
}

/// The namespace of `name`: [`Error::OutOfRange`] for a name that is
/// empty or longer than [`NAME_MAX`], [`Error::InvalidArgument`] for a
/// prefix with nothing after it, and [`Error::NotSupported`] for one of
/// no namespace this knows.
fn space(name: &[u8]) -> Result<Space> {
    if name.is_empty() || name.len() > NAME_MAX {
        return Err(Error::OutOfRange);
    }
    if name.contains(&0) {
        return Err(Error::InvalidArgument);
    }
    if name == acl::ACCESS {
        return Ok(Space::Access);
    }
    if name == acl::DEFAULT {
        return Ok(Space::Default);
    }
    for (prefix, space) in [
        (&b"user."[..], Space::User),
        (b"trusted.", Space::Trusted),
        (b"security.", Space::Security),
    ] {
        if let Some(rest) = name.strip_prefix(prefix) {
            return if rest.is_empty() { Err(Error::InvalidArgument) } else { Ok(space) };
        }
    }
    Err(Error::NotSupported)
}

/// Whether `name` is shown to `who` in a list: a `trusted.` name only
/// with the bypass right, as Linux shows them only to CAP_SYS_ADMIN.
fn shown(who: &Who, name: &[u8]) -> bool {
    who.bypass || !name.starts_with(b"trusted.")
}

/// The value of `node`'s attribute `name`, as getxattr(2) reads it for
/// `who`. An access list is given with the mode's entries in it
/// ([`acl::as_read`]).
pub fn get(who: &Who, node: &dyn Node, name: &[u8]) -> Result<KVec<u8>> {
    let kind = node.kind();
    match space(name)? {
        Space::User => {
            if !matches!(kind, Kind::File | Kind::Directory) {
                return Err(Error::NoAttribute);
            }
            who.check(node, want::READ)?;
        }
        Space::Trusted if !who.bypass => return Err(Error::NoAttribute),
        Space::Access | Space::Default if kind == Kind::Symlink => {
            return Err(Error::NotSupported);
        }
        _ => {}
    }
    let value = node.get_attribute(name)?;
    if name == acl::ACCESS { acl::as_read(value, node.access().mode) } else { Ok(value) }
}

/// The names of `node`'s attributes that `who` is shown, each with a NUL
/// after it, as listxattr(2) gives them.
pub fn list(who: &Who, node: &dyn Node) -> Result<KVec<u8>> {
    let names = node.list_attributes()?;
    if who.bypass || !names.windows(8).any(|w| w == b"trusted.") {
        return Ok(names);
    }
    let mut shown_names = mem::vec_with_capacity(names.len())?;
    for name in names.split_inclusive(|&b| b == 0) {
        if shown(who, name) {
            mem::extend(&mut shown_names, name)?;
        }
    }
    Ok(shown_names)
}

/// Checks that `who` may change `at`'s attribute `name`, which is in
/// `space`.
fn may_change(who: &Who, at: &Location, space: Space) -> Result<()> {
    at.writable()?;
    let node = &*at.node;
    let kind = node.kind();
    match space {
        Space::User => {
            if !matches!(kind, Kind::File | Kind::Directory) {
                return Err(Error::NotPermitted);
            }
            let access = node.access();
            if kind == Kind::Directory && access.mode & mode::STICKY != 0 && !who.owns(access) {
                return Err(Error::NotPermitted);
            }
            who.check(node, want::WRITE)
        }
        Space::Trusted | Space::Security if !who.bypass => Err(Error::NotPermitted),
        Space::Trusted | Space::Security => Ok(()),
        Space::Access | Space::Default => {
            if kind == Kind::Symlink {
                return Err(Error::NotSupported);
            }
            if who.owns(node.access()) { Ok(()) } else { Err(Error::NotPermitted) }
        }
    }
}

/// Sets `at`'s attribute `name` to `value` for `who`, as setxattr(2)
/// does. An access list is checked, and sets the mode's permission bits
/// with it, as Linux's `posix_acl_update_mode`: one the mode says all of
/// is not kept, and the attribute goes; the set-group-ID bit goes too
/// unless `who` is in the node's group or has the bypass right. A list
/// with no entries takes the attribute away. A default list on a node
/// that is not a directory is refused ([`Error::Denied`]).
pub fn set(who: &Who, at: &Location, name: &[u8], value: &[u8], how: Setting) -> Result<()> {
    let space = space(name)?;
    if value.len() > VALUE_MAX {
        return Err(Error::OutOfRange);
    }
    may_change(who, at, space)?;
    let node = &*at.node;
    match space {
        Space::Access => {
            let Some(list) = Acl::parse(value)? else { return gone(node, name) };
            let access = node.access();
            let mode = (access.mode & !0o777) | list.mode_bits();
            let change = who
                .may_change(access, AccessChange { mode: Some(mode), ..AccessChange::default() })?;
            if list.extended() {
                node.set_attribute(name, &list.to_value()?, Setting::Either)?;
            } else {
                gone(node, name)?;
            }
            node.set_access(&change)
        }
        Space::Default => {
            let list = Acl::parse(value)?;
            if node.kind() != Kind::Directory {
                return if list.is_some() { Err(Error::Denied) } else { Ok(()) };
            }
            match list {
                Some(list) => node.set_attribute(name, &list.to_value()?, Setting::Either),
                None => gone(node, name),
            }
        }
        _ => node.set_attribute(name, value, how),
    }
}

/// Takes `node`'s attribute `name` away if it has it.
fn gone(node: &dyn Node, name: &[u8]) -> Result<()> {
    match node.remove_attribute(name) {
        Ok(()) | Err(Error::NoAttribute) => Ok(()),
        Err(err) => Err(err),
    }
}

/// Removes `at`'s attribute `name` for `who`, as removexattr(2) does. An
/// access list goes and leaves the mode as it is, as on Linux.
pub fn remove(who: &Who, at: &Location, name: &[u8]) -> Result<()> {
    let space = space(name)?;
    may_change(who, at, space)?;
    at.node.remove_attribute(name)
}

/// After a chmod(2) of `node` to `mode`, sets its access list's entries
/// for the owner, the mask and the others to the mode's, as Linux's
/// `posix_acl_chmod` does, so that the attribute on the disk says what
/// decides.
pub fn mode_changed(node: &dyn Node, mode: u16) -> Result<()> {
    if node.kind() == Kind::Symlink {
        return Ok(());
    }
    let value = match node.get_attribute(acl::ACCESS) {
        Ok(value) => value,
        Err(Error::NoAttribute | Error::NotSupported) => return Ok(()),
        Err(err) => return Err(err),
    };
    let Some(mut list) = Acl::parse(&value)? else { return Ok(()) };
    list.with_mode(mode);
    node.set_attribute(acl::ACCESS, &list.to_value()?, Setting::Either)
}

/// What a node made in a directory with a default access list takes of
/// it ([`crate::mount::made_with`]): an access list, and for a directory
/// the default list too.
#[derive(Default)]
pub struct Inherited {
    access: Option<KVec<u8>>,
    default: Option<KVec<u8>>,
}

impl Inherited {
    /// Gives what `dir`, whose default list is `default`, hands a node of
    /// `kind` made with `mode`: the mode the list leaves ([`Acl::made`]),
    /// and what to set on the node once it is made.
    pub(crate) fn from(default: &Acl, kind: Kind, mode: u16) -> Result<(u16, Inherited)> {
        let (mode, access) = default.made(mode)?;
        let access = match access {
            Some(list) => Some(list.to_value()?),
            None => None,
        };
        let default = match kind {
            Kind::Directory => Some(default.to_value()?),
            _ => None,
        };
        Ok((mode, Inherited { access, default }))
    }

    /// Sets on `node`, just made, what it takes.
    pub(crate) fn apply(&self, node: &dyn Node) -> Result<()> {
        if let Some(value) = &self.access {
            node.set_attribute(acl::ACCESS, value, Setting::Either)?;
        }
        if let Some(value) = &self.default {
            node.set_attribute(acl::DEFAULT, value, Setting::Either)?;
        }
        Ok(())
    }

    /// Whether there is anything to set.
    pub(crate) fn is_empty(&self) -> bool {
        self.access.is_none() && self.default.is_none()
    }
}

/// The default access list of the directory `dir`, if it has one.
pub(crate) fn default_list(dir: &dyn Node) -> Result<Option<KArc<Acl>>> {
    dir.default_list()
}
