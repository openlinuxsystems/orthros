// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Names (Phase 13, ADR 0214): who a process or a file object acts for,
//! which the second gate checks ([`radix_vfs::Who`]), and the methods of
//! the names capability that gives them (radix-names(2)).
//!
//! A process keeps its name ([`super::process::ProcessObject::identity`]),
//! which `create`
//! and `fork` pass on and `exec` keeps; a file object keeps the name it
//! was opened for ([`crate::fs::FileObject::name`]): its directory's, or
//! for what a process makes without a path, as a pipe, the process's.
//! Only the two methods here give another.

use radix_abi::UserBuffer;
use radix_abi::errors::Kernel;
use radix_abi::names::{self as abi, GROUPS_MAX, NameProcessArgs, StampArgs};
use radix_cap::{Capability, ObjectKind, Rights, SlotRef};
use radix_hal::UserMode;
use radix_vfs::Who;
use radix_vfs::mem::{self, KArc};

use super::methods::{Answer, answer};
use super::objects::{self, Object};
use super::{Result, UserRef, cap_error, error};
use crate::Arch;
use crate::fs::FileObject;

const MAX: usize = GROUPS_MAX as usize;

/// A name: a user, its group, and up to GROUPS_MAX other groups, which
/// most names have none of and the others share.
#[derive(Clone, Debug)]
pub struct Identity {
    /// The user's number.
    pub user: u32,
    /// Its group's number.
    pub group: u32,
    others: Option<KArc<Groups>>,
}

#[derive(Debug)]
struct Groups {
    count: usize,
    list: [u32; MAX],
}

impl Identity {
    /// Root, user and group 0 with no other groups: what the kernel starts
    /// Spark with, and its own calls act for.
    pub const ROOT: Identity = Identity { user: 0, group: 0, others: None };

    /// User `user` in group `group` and in `groups`; `None` if there are
    /// more than GROUPS_MAX of them, or no memory for them.
    pub fn new(user: u32, group: u32, groups: &[u32]) -> Option<Identity> {
        if groups.len() > MAX {
            return None;
        }
        let others = if groups.is_empty() {
            None
        } else {
            let mut list = [0; MAX];
            list[..groups.len()].copy_from_slice(groups);
            Some(mem::arc(Groups { count: groups.len(), list }).ok()?)
        };
        Some(Identity { user, group, others })
    }

    /// Its other groups.
    pub fn groups(&self) -> &[u32] {
        self.others.as_ref().map_or(&[], |others| &others.list[..others.count])
    }

    /// It as the second gate takes it, passing it with `bypass`: whether
    /// the capability it comes through has the bypass right.
    pub fn who(&self, bypass: bool) -> Who<'_> {
        Who { user: self.user, group: self.group, groups: self.groups(), bypass }
    }
}

impl PartialEq for Identity {
    fn eq(&self, other: &Identity) -> bool {
        self.user == other.user && self.group == other.group && self.groups() == other.groups()
    }
}

impl Eq for Identity {}

/// The name `user`, `group` and the groups in the caller's `groups`, as
/// u32s.
fn read(user: u32, group: u32, groups: UserBuffer) -> Result<Identity> {
    let len = groups.len as usize;
    if !len.is_multiple_of(4) || len / 4 > MAX {
        return Err(Kernel::InvalidArgument.status(0));
    }
    let mut bytes = [0u8; MAX * 4];
    if !radix_ipc::range::user_bytes(groups.address, groups.len)
        || !Arch::copy_from_user(&mut bytes[..len], groups.address)
    {
        return Err(error(Kernel::BadAddress));
    }
    let mut list = [0u32; MAX];
    for (n, word) in bytes[..len].as_chunks::<4>().0.iter().enumerate() {
        list[n] = u32::from_le_bytes(*word);
    }
    Identity::new(user, group, &list[..len / 4]).ok_or(error(Kernel::NoMemory))
}

/// A method of the names capability `cap`, called by `user`'s thread.
pub fn method(user: &UserRef, cap: &Capability, label: u32, args: &[u64; 8]) -> Result<Answer> {
    cap.check(ObjectKind::Names, Rights::CONTROL).map_err(cap_error)?;
    match label {
        abi::NAME_PROCESS => {
            let a = NameProcessArgs::decode(args)?;
            let (_, Object::Process(process)) =
                objects::lookup(user.cspace, a.process, ObjectKind::Process, Rights::CONTROL)?
            else {
                return Err(error(Kernel::Gone));
            };
            let identity = read(a.user, a.group, a.groups)?;
            if !process.name_before_start(identity) {
                return Err(Kernel::InvalidArgument.status(0));
            }
            answer(label, 0, [0; 8])
        }
        abi::STAMP => {
            let a = StampArgs::decode(args)?;
            let (cap, dir) = objects::directory(user.cspace, a.directory, Rights::NONE)?;
            let identity = read(a.user, a.group, a.groups)?;
            let object = FileObject::stamped(&dir, identity).ok_or(error(Kernel::NoMemory))?;
            let to = SlotRef::new(user.cspace, a.to);
            super::files::install_file(
                user,
                to,
                ObjectKind::Directory,
                cap.rights(),
                object,
                false,
            )?;
            answer(label, 0, [0; 8])
        }
        _ => Err(error(Kernel::NoSuchMethod)),
    }
}
