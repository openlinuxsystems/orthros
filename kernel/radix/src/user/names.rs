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
//! Only the first two methods here give another.
//!
//! The third, `end`, takes a user's names away (ADR 0220): it ends the
//! processes that act for it, and from then on the gate turns away every
//! name of it given before ([`Identity::is_current`]). Each name counts
//! when it was given ([`MADE`]), and the kernel keeps when each user was
//! ended ([`ENDED`]): a name given before is stale.

use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use radix_abi::UserBuffer;
use radix_abi::errors::Kernel;
use radix_abi::names::{self as abi, EndArgs, EndResults, GROUPS_MAX, NameProcessArgs, StampArgs};
use radix_abi::signal::SIGKILL;
use radix_cap::{Capability, ObjectKind, Rights, SlotRef};
use radix_hal::UserMode;
use radix_mm::{Flags, HEAP, Kmalloc};
use radix_posix::signal::{Info, SI_KERNEL};
use radix_sync::Mutex;
use radix_vfs::Who;
use radix_vfs::mem::{self, KArc};

use super::methods::{Answer, answer};
use super::objects::{self, Object};
use super::posix::signal::{self, Sent};
use super::{Result, UserRef, cap_error, error};
use crate::Arch;
use crate::fs::FileObject;

const MAX: usize = GROUPS_MAX as usize;

/// How many names were given since the kernel started: each
/// [`Identity::new`] counts the next, and `end` one more.
static MADE: AtomicU64 = AtomicU64::new(1);

/// The users ended since the kernel started, each with the count of
/// [`MADE`] it was ended at: a name of it given before then is stale.
/// Made when the first is ended.
static ENDED: Mutex<Option<Vec<(u32, u64), Kmalloc>>> = Mutex::new(None);

/// Whether [`ENDED`] holds any user: until it does, the gate does not
/// take its lock.
static ANY_ENDED: AtomicBool = AtomicBool::new(false);

/// A name: a user, its group, and up to GROUPS_MAX other groups, which
/// most names have none of and the others share.
#[derive(Clone, Debug)]
pub struct Identity {
    /// The user's number.
    pub user: u32,
    /// Its group's number.
    pub group: u32,
    others: Option<KArc<Groups>>,
    /// The count of [`MADE`] it was given at, which its copies keep; 0
    /// for root's.
    made: u64,
}

#[derive(Debug)]
struct Groups {
    count: usize,
    list: [u32; MAX],
}

impl Identity {
    /// Root, user and group 0 with no other groups: what the kernel starts
    /// Spark with, and its own calls act for.
    pub const ROOT: Identity = Identity { user: 0, group: 0, others: None, made: 0 };

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
        Some(Identity { user, group, others, made: MADE.fetch_add(1, Ordering::Relaxed) })
    }

    /// Its other groups.
    pub fn groups(&self) -> &[u32] {
        self.others.as_ref().map_or(&[], |others| &others.list[..others.count])
    }

    /// It as the second gate takes it, passing it with `bypass`: whether
    /// the capability it comes through has the bypass right. None if its
    /// user was ended after it was given ([`Identity::is_current`]), which
    /// the gate turns away.
    pub fn who(&self, bypass: bool) -> Option<Who<'_>> {
        self.is_current().then(|| Who {
            user: self.user,
            group: self.group,
            groups: self.groups(),
            bypass,
            file_mask: 0,
        })
    }

    /// Whether its user was not ended since it was given (ADR 0220).
    /// Root's always is.
    pub fn is_current(&self) -> bool {
        if self.user == 0 || !ANY_ENDED.load(Ordering::Acquire) {
            return true;
        }
        let ended = ENDED.lock();
        let at = ended.as_ref().and_then(|users| users.iter().find(|e| e.0 == self.user));
        at.is_none_or(|&(_, at)| self.made > at)
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

/// A method of the names capability `cap`, called by `user`'s thread,
/// kept out of line, as [`super::methods::call`] says.
#[inline(never)]
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
        abi::END => {
            let a = EndArgs::decode(args)?;
            if a.user == 0 {
                return Err(Kernel::InvalidArgument.status(0));
            }
            mark_ended(a.user)?;
            let ended = end_processes(a.user)?;
            answer(label, EndResults::WORDS, EndResults { ended }.encode())
        }
        _ => Err(error(Kernel::NoSuchMethod)),
    }
}

fn kmalloc() -> Kmalloc {
    HEAP.with(Flags::MAY_SLEEP)
}

/// Notes that `user` was ended now: every name of it given until now is
/// stale.
fn mark_ended(user: u32) -> Result<()> {
    let mut ended = ENDED.lock();
    let users = ended.get_or_insert_with(|| Vec::new_in(kmalloc()));
    let at = MADE.fetch_add(1, Ordering::Relaxed);
    if let Some(entry) = users.iter_mut().find(|e| e.0 == user) {
        entry.1 = at;
    } else {
        users.try_reserve(1).map_err(|_| error(Kernel::NoMemory))?;
        // Room was reserved above.
        let _ = users.push_within_capacity((user, at));
    }
    ANY_ENDED.store(true, Ordering::Release);
    Ok(())
}

/// Sends SIGKILL to every process that acts for `user`, as the kernel;
/// how many it ended, of those that were not ending already.
fn end_processes(user: u32) -> Result<u32> {
    let processes = objects::processes().ok_or(error(Kernel::NoMemory))?;
    let from = Info { code: SI_KERNEL, pid: 0, status: 0 };
    let mut ended = 0u32;
    for process in processes.iter().filter(|p| p.user() == user) {
        if signal::send_process(process, SIGKILL, from) != Sent::Dropped {
            ended = ended.saturating_add(1);
        }
    }
    Ok(ended)
}
