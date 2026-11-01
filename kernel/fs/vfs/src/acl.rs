// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! POSIX access lists (ADR 0256), as Linux has them.
//!
//! An access list adds named users and groups to a node's owner, group
//! and others, each with read, write and execute: `user:bob:rw-` lets bob
//! read and write a file whatever its mode says of the others. A node
//! keeps it as the extended attribute `system.posix_acl_access`, in
//! Linux's `posix_acl_xattr` form, which programs read and write with
//! getxattr(2) and setxattr(2) and which setfacl(1) and getfacl(1) use.
//! A directory may have a default list too, `system.posix_acl_default`,
//! which what is made in it takes ([`Acl::made`]).
//!
//! The list and the mode say the same of the owner and the others, and
//! the mode's group bits are the list's mask, the most any named entry or
//! the owning group gets: chmod(1) changes the mask, and `ls -l` shows
//! it. The kernel keeps them so: setting a list sets the mode, and
//! changing the mode changes the list's entries for the owner, the mask
//! and the others ([`Acl::with_mode`]). Where they could still differ,
//! between the two writes, the mode is what counts ([`Acl::permits`]).
//!
//! The check is Linux's `posix_acl_permission`, and it is made only as
//! Linux's `acl_permission_check` makes it: for a caller who does not own
//! the node, and only when the mode's group bits are not all clear; with
//! a mask of nothing the mode alone decides, as on Linux.

use radix_sync::Mutex;

use crate::mem::{self, KArc, KVec};
use crate::{Access, Error, Result, Who};

/// The attribute that holds a node's access list.
pub const ACCESS: &[u8] = b"system.posix_acl_access";

/// The attribute that holds a directory's default list.
pub const DEFAULT: &[u8] = b"system.posix_acl_default";

/// The version of the attribute's form, Linux's `POSIX_ACL_XATTR_VERSION`.
const VERSION: u32 = 2;

/// The bytes of the form's header (its version) and of each entry: a
/// tag and permissions of two bytes each and an id of four, little-endian.
const HEADER: usize = 4;
const ENTRY: usize = 8;

/// The id an entry without one holds: Linux's `ACL_UNDEFINED_ID`.
const NO_ID: u32 = u32::MAX;

/// The kinds of entry, Linux's numbers, in the order a list holds them.
pub mod tag {
    /// The node's owner: the mode's owner bits.
    pub const USER_OBJ: u16 = 0x01;
    /// A user named by number.
    pub const USER: u16 = 0x02;
    /// The node's group.
    pub const GROUP_OBJ: u16 = 0x04;
    /// A group named by number.
    pub const GROUP: u16 = 0x08;
    /// The most a named entry or the node's group gets: the mode's group
    /// bits.
    pub const MASK: u16 = 0x10;
    /// Everyone else: the mode's bits for the others.
    pub const OTHER: u16 = 0x20;
}

/// One entry: what it names and the permissions it gives, read (4), write
/// (2) and execute (1).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Entry {
    /// What it names ([`tag`]).
    pub tag: u16,
    /// The permissions, 7 at most.
    pub perm: u16,
    /// The user's or group's number for [`tag::USER`] and [`tag::GROUP`];
    /// none for the others.
    pub id: u32,
}

/// An access list: its entries, in the order of their tags, as Linux's
/// `posix_acl_valid` has them, with an entry for the owner, the group
/// and the others, and a mask if it names a user or group.
#[derive(Debug, PartialEq, Eq)]
pub struct Acl {
    entries: KVec<Entry>,
}

impl Acl {
    /// The list an attribute's value holds, as Linux's
    /// `posix_acl_from_xattr` and `posix_acl_valid` read it: none for a
    /// header with no entries, and [`Error::InvalidArgument`] for a value
    /// that is not a whole number of entries, has a tag Linux has not or
    /// permissions past 7, or whose entries are out of order or miss
    /// one. A version other than 2 is [`Error::NotSupported`], as Linux's
    /// EOPNOTSUPP.
    pub fn parse(value: &[u8]) -> Result<Option<Acl>> {
        if value.len() < HEADER || !(value.len() - HEADER).is_multiple_of(ENTRY) {
            return Err(Error::InvalidArgument);
        }
        if u32::from_le_bytes([value[0], value[1], value[2], value[3]]) != VERSION {
            return Err(Error::NotSupported);
        }
        let count = (value.len() - HEADER) / ENTRY;
        if count == 0 {
            return Ok(None);
        }
        let mut entries = mem::vec_with_capacity(count)?;
        for raw in value[HEADER..].as_chunks::<ENTRY>().0 {
            let tag = u16::from_le_bytes([raw[0], raw[1]]);
            let perm = u16::from_le_bytes([raw[2], raw[3]]);
            let id = match tag {
                tag::USER | tag::GROUP => u32::from_le_bytes([raw[4], raw[5], raw[6], raw[7]]),
                _ => NO_ID,
            };
            mem::push(&mut entries, Entry { tag, perm, id })?;
        }
        let acl = Acl { entries };
        acl.check()?;
        Ok(Some(acl))
    }

    /// Fails with [`Error::InvalidArgument`] unless its entries are in
    /// Linux's order: the owner, the named users, the group, the named
    /// groups, the mask (needed if any is named) and the others, one each
    /// but the named ones, each with no permissions past 7.
    fn check(&self) -> Result<()> {
        // Linux's states: what may come next.
        const USERS: u8 = 1;
        const GROUPS: u8 = 2;
        const OTHERS: u8 = 3;
        const DONE: u8 = 4;
        let mut state = 0;
        let mut named = false;
        for entry in &self.entries {
            if entry.perm & !7 != 0 {
                return Err(Error::InvalidArgument);
            }
            state = match (entry.tag, state) {
                (tag::USER_OBJ, 0) => USERS,
                (tag::USER, USERS) | (tag::GROUP, GROUPS) => {
                    named = true;
                    state
                }
                (tag::GROUP_OBJ, USERS) => GROUPS,
                (tag::MASK, GROUPS) => OTHERS,
                (tag::OTHER, OTHERS) => DONE,
                (tag::OTHER, GROUPS) if !named => DONE,
                _ => return Err(Error::InvalidArgument),
            };
        }
        if state == DONE { Ok(()) } else { Err(Error::InvalidArgument) }
    }

    /// The list that says what `mode`'s permission bits say: the owner's,
    /// the group's and the others' entries alone.
    pub fn from_mode(mode: u16) -> Result<Acl> {
        let mut entries = mem::vec_with_capacity(3)?;
        for (tag, shift) in [(tag::USER_OBJ, 6), (tag::GROUP_OBJ, 3), (tag::OTHER, 0)] {
            mem::push(&mut entries, Entry { tag, perm: (mode >> shift) & 7, id: NO_ID })?;
        }
        Ok(Acl { entries })
    }

    /// Its entries.
    pub fn entries(&self) -> &[Entry] {
        &self.entries
    }

    /// It in the attribute's form.
    pub fn to_value(&self) -> Result<KVec<u8>> {
        let mut value = mem::vec_with_capacity(HEADER + ENTRY * self.entries.len())?;
        mem::extend(&mut value, &VERSION.to_le_bytes())?;
        for entry in &self.entries {
            mem::extend(&mut value, &entry.tag.to_le_bytes())?;
            mem::extend(&mut value, &entry.perm.to_le_bytes())?;
            mem::extend(&mut value, &entry.id.to_le_bytes())?;
        }
        Ok(value)
    }

    /// Whether it names a user or group or has a mask: whether a mode
    /// cannot say as much, as Linux's `posix_acl_equiv_mode` asks.
    pub fn extended(&self) -> bool {
        self.entries.iter().any(|e| matches!(e.tag, tag::USER | tag::GROUP | tag::MASK))
    }

    /// The permission bits of the mode that goes with it: the owner's
    /// entry, the mask (the group's entry if it has none) and the
    /// others'.
    pub fn mode_bits(&self) -> u16 {
        let perm = |want: u16| self.entries.iter().find(|e| e.tag == want).map_or(0, |e| e.perm);
        let group = if self.has(tag::MASK) { perm(tag::MASK) } else { perm(tag::GROUP_OBJ) };
        (perm(tag::USER_OBJ) << 6) | (group << 3) | perm(tag::OTHER)
    }

    /// Whether it has an entry of `tag`.
    fn has(&self, tag: u16) -> bool {
        self.entries.iter().any(|e| e.tag == tag)
    }

    /// Sets the entries the mode says, from `mode`'s permission bits: the
    /// owner's, the mask (the group's if it has none) and the others', as
    /// Linux's `__posix_acl_chmod` does for chmod(2).
    pub fn with_mode(&mut self, mode: u16) {
        let masked = self.has(tag::MASK);
        for entry in &mut self.entries {
            match entry.tag {
                tag::USER_OBJ => entry.perm = (mode >> 6) & 7,
                tag::MASK => entry.perm = (mode >> 3) & 7,
                tag::GROUP_OBJ if !masked => entry.perm = (mode >> 3) & 7,
                tag::OTHER => entry.perm = mode & 7,
                _ => {}
            }
        }
    }

    /// Whether it lets `who` do `want` ([`crate::want`]'s bits) to a node
    /// with `access`, as Linux's `posix_acl_permission`: the owner's
    /// entry for the owner; a named user's entry, within the mask; the
    /// group's and named groups' entries that `who` is in, the first that
    /// gives all of `want`, within the mask, and none if some matched but
    /// none gave it; else the others'. The owner's entry, the mask and the
    /// others' are read from `access`'s mode, which they match but for a
    /// moment while both change.
    pub fn permits(&self, who: &Who, access: Access, want: u16) -> bool {
        let want = want & 7;
        let mode = access.mode;
        let mask = if self.has(tag::MASK) { (mode >> 3) & 7 } else { 7 };
        let mut found = false;
        for entry in &self.entries {
            match entry.tag {
                tag::USER_OBJ if access.owner == who.user => return (mode >> 6) & want == want,
                tag::USER if entry.id == who.user => return entry.perm & mask & want == want,
                tag::GROUP_OBJ if who.in_group(access.group) => {
                    found = true;
                    // Without a mask the group's entry is the mode's.
                    let perm = if self.has(tag::MASK) { entry.perm } else { (mode >> 3) & 7 };
                    if perm & want == want {
                        return perm & mask & want == want;
                    }
                }
                tag::GROUP if who.in_group(entry.id) => {
                    found = true;
                    if entry.perm & want == want {
                        return entry.perm & mask & want == want;
                    }
                }
                tag::OTHER => return !found && mode & want == want,
                _ => {}
            }
        }
        false
    }

    /// What a node made with `mode` gets of this list, a directory's
    /// default list, as Linux's `posix_acl_create_masq`: the list less
    /// what `mode` does not give, and the mode less what the list does
    /// not; the caller's file mode mask plays no part. Returns the new
    /// mode, whose bits past the permissions stay, and the node's access
    /// list, none if the mode says all it does.
    pub fn made(&self, mode: u16) -> Result<(u16, Option<Acl>)> {
        let mut entries = mem::vec_with_capacity(self.entries.len())?;
        let mut perms = mode & 0o777;
        let masked = self.has(tag::MASK);
        for entry in &self.entries {
            let mut entry = *entry;
            match entry.tag {
                tag::USER_OBJ => {
                    entry.perm &= (perms >> 6) & 7;
                    perms = (perms & !0o700) | (entry.perm << 6);
                }
                tag::OTHER => {
                    entry.perm &= perms & 7;
                    perms = (perms & !0o007) | entry.perm;
                }
                tag::MASK => {
                    entry.perm &= (perms >> 3) & 7;
                    perms = (perms & !0o070) | (entry.perm << 3);
                }
                tag::GROUP_OBJ if !masked => {
                    entry.perm &= (perms >> 3) & 7;
                    perms = (perms & !0o070) | (entry.perm << 3);
                }
                _ => {}
            }
            mem::push(&mut entries, entry)?;
        }
        let acl = Acl { entries };
        let mode = (mode & !0o777) | perms;
        Ok((mode, acl.extended().then_some(acl)))
    }

    /// A copy of it.
    pub fn copy(&self) -> Result<Acl> {
        let mut entries = mem::vec_with_capacity(self.entries.len())?;
        for entry in &self.entries {
            mem::push(&mut entries, *entry)?;
        }
        Ok(Acl { entries })
    }
}

/// The value of `system.posix_acl_access` that `stored` holds, with the
/// owner's entry, the mask and the others' read from `mode`, so that a
/// program reads what decides ([`Acl::permits`]). A value that does not
/// parse is given as it is.
pub fn as_read(stored: KVec<u8>, mode: u16) -> Result<KVec<u8>> {
    match Acl::parse(&stored) {
        Ok(Some(mut acl)) => {
            acl.with_mode(mode);
            acl.to_value()
        }
        _ => Ok(stored),
    }
}

/// What a node knows of its access list ([`crate::Node::access_list`]),
/// for a filesystem that reads it from a disk to keep it in memory:
/// whether it read it, and what it read. A change of the attribute
/// forgets it ([`AclCell::forget`]), and the next check reads it again.
pub struct AclCell {
    state: Mutex<Known>,
}

/// What an [`AclCell`] knows.
enum Known {
    /// Nothing yet, or forgotten since: the attribute is read again. The
    /// number counts the times it was forgotten, so that a read that a
    /// change overtook is not kept.
    Unread(u64),
    /// The node has no access list.
    None(u64),
    /// Its list.
    Read(u64, KArc<Acl>),
}

impl Known {
    fn era(&self) -> u64 {
        match self {
            Known::Unread(era) | Known::None(era) | Known::Read(era, _) => *era,
        }
    }
}

impl Default for AclCell {
    fn default() -> Self {
        AclCell::unread()
    }
}

impl AclCell {
    /// A node's whose list is to be read.
    pub fn unread() -> AclCell {
        AclCell { state: Mutex::new(Known::Unread(0)) }
    }

    /// A node's known to have none, as one just made.
    pub fn none() -> AclCell {
        AclCell { state: Mutex::new(Known::None(0)) }
    }

    /// The node's list: kept, or read with `read`, which gives the
    /// attribute's value or [`Error::NoAttribute`], and kept unless the
    /// attribute changed meanwhile. `read` runs without the cell's lock.
    pub fn get(&self, read: impl FnOnce() -> Result<KVec<u8>>) -> Result<Option<KArc<Acl>>> {
        let era = match &*self.state.lock() {
            Known::None(_) => return Ok(None),
            Known::Read(_, acl) => return Ok(Some(acl.clone())),
            Known::Unread(era) => *era,
        };
        let acl = match read() {
            Ok(value) => Acl::parse(&value).ok().flatten(),
            Err(Error::NoAttribute | Error::NotSupported) => None,
            Err(err) => return Err(err),
        };
        let acl = match acl {
            Some(acl) => Some(mem::arc(acl)?),
            None => None,
        };
        let mut state = self.state.lock();
        if state.era() == era && matches!(*state, Known::Unread(_)) {
            *state = match &acl {
                Some(acl) => Known::Read(era, acl.clone()),
                None => Known::None(era),
            };
        }
        Ok(acl)
    }

    /// Forgets what it knows, after the attribute changed.
    pub fn forget(&self) {
        let mut state = self.state.lock();
        *state = Known::Unread(state.era() + 1);
    }
}
