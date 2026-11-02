// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! The Radix virtual file system (Phase 7, docs/design/storage.md).
//!
//! Every filesystem shows its files as [`Node`]s: a directory looks names
//! up, creates, removes, renames and lists them; a file reads and writes
//! bytes at offsets. A [`FileSystem`] is one tree of nodes, with a root;
//! the [`mount::Vfs`] joins filesystems into one tree by mounting each on
//! a directory of another, and resolves paths through it.
//!
//! Programs never name files by absolute path. They hold capabilities to
//! directories (radix-directory(2)), and a path is always resolved
//! *beneath* the directory it is given with: `..` never climbs above it,
//! and an absolute path is refused ([`path`]). What a program can reach
//! is what its directory capabilities lead to, as with WASI's preopened
//! directories and FreeBSD's Capsicum.
//!
//! File data lives in a page cache ([`cache::FileCache`]): reads and
//! writes copy between the caller's memory and the cache's pages, which
//! disks fill and empty by DMA, and dirty pages reach the disk when the
//! filesystem syncs. tmpfs ([`tmpfs`]) is a page cache with nothing below
//! it. [`synthetic`] makes the generated files of procfs, sysfs and devfs,
//! and [`bootfs`] serves the boot archive. [`locks`] is what a file's
//! advisory locks do (Phase 8). [`raw`] reads and writes block devices
//! with no cache between, for `/devices/sda` and its kind (Phase 10).
//! A node may have extended attributes, names with values as Linux's
//! xattrs, which [`attributes`] reads and changes with the second gate's
//! checks, and among them an access list ([`acl`], Phase 13).
//!
//! The crate is a compartment: it has no `unsafe` code. It builds for the
//! host (feature `host`), where its tests run.

#![cfg_attr(not(any(test, kani, feature = "host")), no_std)]
#![forbid(unsafe_code)]
#![cfg_attr(not(kani), feature(allocator_ext))]
// Kani's own, older nightly has the allocator API unstable still.
#![cfg_attr(kani, feature(allocator_api))]
#![feature(vec_push_within_capacity)]
#![feature(ascii_char)]

extern crate alloc;

pub mod acl;
pub mod attributes;
pub mod bootfs;
pub mod cache;
pub mod locks;
pub mod mount;
pub mod path;
pub mod raw;
pub mod synthetic;
pub mod tmpfs;
pub mod view;

#[cfg(test)]
mod tests;

use core::any::Any;
use core::fmt;
use core::sync::atomic::{AtomicU64, Ordering};

pub use radix_block::mem::{self, KArc, KVec, NoMemory};

/// The longest name in a directory, in bytes of UTF-8.
pub const NAME_MAX: usize = 255;

/// The longest path a call takes, in bytes.
pub const PATH_MAX: usize = 4096;

/// The most symbolic links one resolution follows, as Linux allows.
pub const MAX_LINKS: usize = 40;

/// What can go wrong with a file operation. Each maps to an error of the
/// `file` domain of radix-errors(2), or to a kernel error.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
    /// No file has that name.
    NotFound,
    /// A file has that name already.
    Exists,
    /// A component of the path is not a directory.
    NotDirectory,
    /// The file is a directory.
    IsDirectory,
    /// The directory is not empty.
    NotEmpty,
    /// The filesystem, or the capability, does not allow writing.
    ReadOnly,
    /// The filesystem is full.
    NoSpace,
    /// A name is longer than [`NAME_MAX`], or the path than [`PATH_MAX`].
    NameTooLong,
    /// A name is empty, `.` or `..` where a file is meant, holds a `/` or
    /// a NUL, is not UTF-8, or is one the filesystem cannot store.
    BadName,
    /// The path leaves the directory it is resolved beneath: it starts
    /// with `/`, or its `..` climbs above it.
    Outside,
    /// The operation would move a file between filesystems.
    CrossMount,
    /// The file is in use: a mount point, a filesystem with files open, or
    /// a disk that is a swap area.
    InUse,
    /// The disk failed to read or write.
    Io,
    /// The filesystem's structures on the disk are damaged.
    Corrupt,
    /// The file would grow past the largest size the filesystem allows.
    TooBig,
    /// No filesystem of that kind exists, or it does not recognise the
    /// disk.
    UnknownFilesystem,
    /// A mount needs a block device and was given something else.
    NotBlockDevice,
    /// The device the node stands for is not there.
    NoDevice,
    /// The node does not do this.
    NotSupported,
    /// Resolving the path met more than [`MAX_LINKS`] symbolic links, or
    /// a symbolic link where it was not to follow one.
    Loop,
    /// The node is not a symbolic link.
    NotLink,
    /// The file has as many names as its filesystem keeps for one.
    TooManyNames,
    /// The change would take its quota group past its limit.
    QuotaExceeded,
    /// Out of memory.
    NoMemory,
    /// A buffer of the caller's is not mapped for the access.
    BadAddress,
    /// An argument the node cannot take: an offset or length a clone
    /// needs whole blocks of, or ranges of one file that overlap.
    InvalidArgument,
    /// The second gate refuses: the node's owner, group and mode do not let
    /// the caller's name do this (ADR 0214). POSIX's EACCES.
    Denied,
    /// Only the node's owner, or a caller with the bypass right, may do
    /// this: change its mode, owner or times, or remove it from a sticky
    /// directory (ADR 0214). POSIX's EPERM.
    NotPermitted,
    /// The node has no extended attribute of that name (ADR 0256).
    /// Linux's ENODATA.
    NoAttribute,
    /// An extended attribute's name is empty or longer than
    /// [`attributes::NAME_MAX`], or a value or list does not fit where it
    /// is to go. Linux's ERANGE.
    OutOfRange,
    /// A filesystem's program stopped the call when a signal asked it
    /// to, or the thread was stopped while it waited (ADR 0226). POSIX's
    /// EINTR.
    Interrupted,
    /// The program that served the filesystem has gone, or its mount
    /// has (ADR 0226). POSIX's ENOTCONN.
    NotConnected,
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Error::NotFound => "no such file or directory",
            Error::Exists => "the file exists",
            Error::NotDirectory => "not a directory",
            Error::IsDirectory => "is a directory",
            Error::NotEmpty => "the directory is not empty",
            Error::ReadOnly => "read-only",
            Error::NoSpace => "no space left on the filesystem",
            Error::NameTooLong => "the name is too long",
            Error::BadName => "the name is not valid",
            Error::Outside => "the path leaves the directory",
            Error::CrossMount => "the files are on different filesystems",
            Error::InUse => "in use",
            Error::Io => "input/output error",
            Error::Corrupt => "the filesystem is damaged",
            Error::TooBig => "the file would be too big",
            Error::UnknownFilesystem => "unknown filesystem",
            Error::NotBlockDevice => "not a block device",
            Error::NoDevice => "no such device",
            Error::NotSupported => "not supported",
            Error::Loop => "too many levels of symbolic links",
            Error::NotLink => "not a symbolic link",
            Error::TooManyNames => "too many names",
            Error::QuotaExceeded => "disk quota exceeded",
            Error::NoMemory => "out of memory",
            Error::BadAddress => "bad address",
            Error::InvalidArgument => "invalid argument",
            Error::Denied => "permission denied",
            Error::NotPermitted => "operation not permitted",
            Error::NoAttribute => "no such attribute",
            Error::OutOfRange => "out of range",
            Error::Interrupted => "interrupted",
            Error::NotConnected => "the filesystem's program has gone",
        })
    }
}

impl From<NoMemory> for Error {
    fn from(_: NoMemory) -> Error {
        Error::NoMemory
    }
}

impl From<radix_block::Error> for Error {
    fn from(err: radix_block::Error) -> Error {
        match err {
            radix_block::Error::Io | radix_block::Error::OutOfRange => Error::Io,
            radix_block::Error::ReadOnly => Error::ReadOnly,
            radix_block::Error::NoMemory => Error::NoMemory,
            radix_block::Error::Unsupported => Error::NotSupported,
        }
    }
}

/// The result of a file operation.
pub type Result<T> = core::result::Result<T, Error>;

/// What a node is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    /// A regular file.
    File,
    /// A directory.
    Directory,
    /// A block device, which filesystems are mounted from.
    BlockDevice,
    /// A character device: `null`, `zero`, terminals and the like.
    CharDevice,
    /// A pipe, which lives outside the tree (Phase 8).
    Pipe,
    /// A socket, which lives outside the tree too (Phase 9), or a
    /// socket's name in it ([`Node::make_socket`]).
    Socket,
    /// A symbolic link: a path that resolution follows in its place
    /// ([`mount`]); its size is the path's length.
    Symlink,
}

/// What a node says about itself.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Stat {
    /// What it is.
    pub kind: Kind,
    /// Its size in bytes (0 for directories that do not say).
    pub size: u64,
    /// The bytes it takes on its filesystem.
    pub allocated: u64,
    /// A number unique among the nodes of its filesystem, while it exists.
    pub id: u64,
    /// When it was made, in nanoseconds since 1970 (UTC); 0 if unknown.
    pub created: u64,
    /// When its contents last changed, likewise.
    pub modified: u64,
    /// When it was last read, likewise: when it was made, or the time a
    /// program set (`set_times`), since reading does not change it (as
    /// with Linux's `noatime`, ADR 0076).
    pub accessed: u64,
    /// When it last changed, its contents or what is said of it (its
    /// names, times or size), likewise (ADR 0135).
    pub changed: u64,
    /// How many names it has in the tree, 1 for a node with none of its
    /// own (ADR 0135).
    pub links: u32,
    /// The number of the device a device node stands for, as
    /// [`Node::make_node`] took it; 0 for any other node.
    pub device: u64,
}

/// Who a node belongs to and what its permission bits are: POSIX's
/// `st_uid`, `st_gid` and `st_mode & 0o7777` (ADR 0213). They only ever
/// narrow what a capability reaches; until users come every process is
/// user 0, so nothing checks them yet.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Access {
    /// The number of the user it belongs to.
    pub owner: u32,
    /// The number of its group.
    pub group: u32,
    /// Its permission bits, [`mode::ALL`] at most: read, write and
    /// execute for its owner, its group and the others, with
    /// set-user-ID, set-group-ID and sticky.
    pub mode: u16,
}

/// The bits of [`Access::mode`] past the nine permissions.
pub mod mode {
    /// Run as the file's owner (kept and shown, but Radix runs no program
    /// as anyone else: ADR 0213).
    pub const SET_USER: u16 = 0o4000;
    /// Run as the file's group; on a directory, files made in it take its
    /// group.
    pub const SET_GROUP: u16 = 0o2000;
    /// On a directory: only a file's owner may remove or rename it.
    pub const STICKY: u16 = 0o1000;
    /// Execute (search, for a directory) for the group.
    pub const GROUP_EXECUTE: u16 = 0o010;
    /// Every bit a mode may have.
    pub const ALL: u16 = 0o7777;
}

impl Access {
    /// What a node of `kind` shows when its filesystem keeps no owners or
    /// modes, as procfs, sysfs and devfs keep none: user 0's, with
    /// Linux's usual modes.
    pub const fn default_for(kind: Kind) -> Access {
        let mode = match kind {
            Kind::Directory => 0o755,
            Kind::File => 0o644,
            Kind::BlockDevice => 0o660,
            Kind::CharDevice => 0o666,
            Kind::Pipe => 0o600,
            Kind::Socket | Kind::Symlink => 0o777,
        };
        Access { owner: 0, group: 0, mode }
    }
}

/// What a call asks of a node, as the bits of a mode for one class: read,
/// write and execute (search, for a directory).
pub mod want {
    /// Read a file, or list a directory.
    pub const READ: u16 = 0o4;
    /// Write a file, or change a directory's entries.
    pub const WRITE: u16 = 0o2;
    /// Run a file, or look in a directory.
    pub const EXECUTE: u16 = 0o1;
}

/// Who a call acts for, which the second gate checks (ADR 0214): the name
/// of the capability it comes through (a user, its group and the other
/// groups it is in) and whether that capability has the bypass right
/// (the security design's override), which passes the gate; and for a
/// call that makes a node, the caller's file mode mask.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Who<'a> {
    /// The user's number.
    pub user: u32,
    /// Its group's number.
    pub group: u32,
    /// The other groups it is in.
    pub groups: &'a [u32],
    /// Whether it passes the gate.
    pub bypass: bool,
    /// The permission bits a node the call makes does not get, the
    /// process's umask(2), unless the directory it is made in has a
    /// default access list, which decides in its place as on Linux
    /// ([`mount::made_with`], ADR 0256).
    pub file_mask: u16,
}

impl Who<'_> {
    /// The kernel's own calls, and those of a capability with the bypass
    /// right acting for root.
    pub const KERNEL: Who<'static> =
        Who { user: 0, group: 0, groups: &[], bypass: true, file_mask: 0 };

    /// Whether it is in `group`.
    pub fn in_group(&self, group: u32) -> bool {
        self.group == group || self.groups.contains(&group)
    }

    /// Whether the gate lets it do `want` ([`want`]'s bits) to a node of
    /// `kind` with `access`, as Linux's generic permission check without
    /// access lists: the owner's bits if it owns the node, else the
    /// group's if it is in the node's group, else the others'. The bypass
    /// passes, but for running a file no one may run, as for Linux's root.
    /// [`Who::check`] reads the node's access list too.
    pub fn may(&self, access: Access, kind: Kind, want: u16) -> bool {
        if self.bypass {
            return want & want::EXECUTE == 0
                || kind == Kind::Directory
                || access.mode & 0o111 != 0;
        }
        let shift = if access.owner == self.user {
            6
        } else if self.in_group(access.group) {
            3
        } else {
            0
        };
        (access.mode >> shift) & want == want
    }

    /// [`Who::may`] for `node`, failing with [`Error::Denied`], with its
    /// access list (ADR 0256) as Linux's `acl_permission_check` reads it:
    /// for a name that does not own the node, when the mode's group bits,
    /// the list's mask, are not all clear ([`acl::Acl::permits`]). With
    /// the bypass it does not read the node's access unless it must.
    pub fn check(&self, node: &dyn Node, want: u16) -> Result<()> {
        let kind = node.kind();
        if self.bypass && (want & want::EXECUTE == 0 || kind == Kind::Directory) {
            return Ok(());
        }
        let access = node.access();
        if !self.bypass
            && access.owner != self.user
            && access.mode & 0o070 != 0
            && let Some(list) = node.access_list()?
        {
            return if list.permits(self, access, want) { Ok(()) } else { Err(Error::Denied) };
        }
        if self.may(access, kind, want) { Ok(()) } else { Err(Error::Denied) }
    }

    /// Whether it owns a node with `access`, or passes the gate: who may
    /// change its mode and times, as Linux's `inode_owner_or_capable`.
    pub fn owns(&self, access: Access) -> bool {
        self.bypass || access.owner == self.user
    }

    /// The part of `change` to a node with `access` that it may make, as
    /// Linux's chown(2) and chmod(2) let a process, or
    /// [`Error::NotPermitted`]. Without the bypass right: the owner stays,
    /// but that the node's owner may name itself; the group changes only
    /// by the owner, to one of its groups; the mode changes only by the
    /// owner, and loses its set-group-ID bit unless the node's group (the
    /// new one, if the change has one) is one of its. A chown that changes
    /// neither, as `chown -1 -1`, is the owner's too when it would take a
    /// set-ID bit away.
    pub fn may_change(&self, access: Access, change: AccessChange) -> Result<AccessChange> {
        if self.bypass {
            return Ok(change);
        }
        let owner = access.owner == self.user;
        let refused = change.owner.is_some_and(|new| !owner || new != access.owner)
            || change
                .group
                .is_some_and(|new| !owner || (new != access.group && !self.in_group(new)))
            || (change.mode.is_some() && !owner)
            || (!owner && change.apply(access).mode != access.mode);
        if refused {
            return Err(Error::NotPermitted);
        }
        let mut allowed = change;
        if let Some(mode) = &mut allowed.mode
            && !self.in_group(change.group.unwrap_or(access.group))
        {
            *mode &= !mode::SET_GROUP;
        }
        Ok(allowed)
    }

    /// Whether it may set the times of `node`: to times it gives
    /// (`given`) as their owner, and to now as their owner or one who may
    /// write it, as utimensat(2) has it; [`Error::NotPermitted`] or
    /// [`Error::Denied`] otherwise.
    pub fn may_set_times(&self, node: &dyn Node, given: bool) -> Result<()> {
        if self.owns(node.access()) {
            Ok(())
        } else if given {
            Err(Error::NotPermitted)
        } else {
            self.check(node, want::WRITE)
        }
    }
}

/// A change of a node's owner, group or mode ([`Node::set_access`]): each
/// that is `None` stays as it is.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct AccessChange {
    /// The new owner's number.
    pub owner: Option<u32>,
    /// The new group's number.
    pub group: Option<u32>,
    /// The new permission bits, [`mode::ALL`] at most.
    pub mode: Option<u16>,
    /// Take the set-user-ID bit away, and the set-group-ID bit if the
    /// group may execute it, before `mode` is set: what chown(2) does to
    /// a node that is not a directory, so that a program never keeps them
    /// for a new owner.
    pub drop_set_ids: bool,
}

impl AccessChange {
    /// What chown(2) changes on a node of `kind`: `owner` and `group`, and
    /// as Linux has it, for one that is not a directory, its set-ID bits
    /// ([`AccessChange::drop_set_ids`]), even when both stay.
    pub fn chown(kind: Kind, owner: Option<u32>, group: Option<u32>) -> Self {
        AccessChange { owner, group, mode: None, drop_set_ids: kind != Kind::Directory }
    }

    /// Whether a filesystem that keeps no owners or modes, of a node that
    /// shows `shown`, takes the change, as Linux's vfat and exfat do: one
    /// that names the owner and group shown, if any, and permission bits
    /// alone is taken and changes nothing, so that `cp -p` and `tar` can
    /// copy to a USB stick; any other is refused.
    pub fn fits_none_kept(&self, shown: Access) -> bool {
        self.owner.is_none_or(|owner| owner == shown.owner)
            && self.group.is_none_or(|group| group == shown.group)
            && self.mode.is_none_or(|mode| mode & !0o777 == 0)
    }

    /// `access` with the change made. Filesystems call it under the lock
    /// that keeps the node's access, so that no other change slips in
    /// between.
    pub fn apply(&self, access: Access) -> Access {
        let mut kept = access.mode;
        if self.drop_set_ids {
            kept &= !mode::SET_USER;
            if kept & mode::GROUP_EXECUTE != 0 {
                kept &= !mode::SET_GROUP;
            }
        }
        Access {
            owner: self.owner.unwrap_or(access.owner),
            group: self.group.unwrap_or(access.group),
            mode: self.mode.map_or(kept, |mode| mode & mode::ALL),
        }
    }
}

/// The device number of `major` and `minor`, as [`Stat::device`] holds
/// it: the major number in its high half.
pub const fn device_number(major: u32, minor: u32) -> u64 {
    ((major as u64) << 32) | minor as u64
}

/// The major and minor numbers of device number `device`
/// ([`device_number`]).
pub const fn device_parts(device: u64) -> (u32, u32) {
    ((device >> 32) as u32, device as u32)
}

/// A node, shared.
pub type NodeRef = KArc<dyn Node>;

/// What a socket's name holds ([`Node::make_socket`]): the kernel's, which
/// the filesystem only keeps.
pub type SocketBinding = KArc<dyn Any + Send + Sync>;

/// What [`Out::lend`] and [`In::lend`] call with the memory they lend: the
/// physical address and length of each part of a page of it, in order.
pub type Borrower<'a> = &'a mut dyn FnMut(&[(u64, u32)]) -> Result<()>;

/// Where a read puts what it reads: the caller's buffer.
pub trait Out {
    /// Its length in bytes.
    fn len(&self) -> usize;
    /// Whether it is empty.
    fn is_empty(&self) -> bool {
        self.len() == 0
    }
    /// Copies `bytes` to `at` bytes from its start.
    fn put(&mut self, at: usize, bytes: &[u8]) -> Result<()>;
    /// Zeroes `len` bytes from `at`.
    fn zero(&mut self, at: usize, len: usize) -> Result<()> {
        let zeros = [0u8; 256];
        let mut done = 0;
        while done < len {
            let n = (len - done).min(zeros.len());
            self.put(at + done, &zeros[..n])?;
            done += n;
        }
        Ok(())
    }
    /// Copies as [`Out::put`] does, but without waiting for a page of a
    /// program's memory to be made: `Ok(false)` if one is not there yet,
    /// when some of the bytes may have been copied. For a caller holding
    /// a lock that making a page can take (a file's cache: a fault on a
    /// mapped file reads it), which lets go of the lock and calls
    /// [`Out::fault_in`] before it tries again. Kernel memory is always
    /// there.
    fn put_now(&mut self, at: usize, bytes: &[u8]) -> Result<bool> {
        self.put(at, bytes).map(|()| true)
    }
    /// Makes the memory for `len` bytes from `at` ready to be written, as
    /// faults on it would, so that [`Out::put_now`] there succeeds; called
    /// with no lock held.
    fn fault_in(&mut self, _at: usize, _len: usize) -> Result<()> {
        Ok(())
    }
    /// Lends its memory to a device to write into, if it can, as
    /// [`radix_block::BlockDevice::read_frames`] takes it: as much of the
    /// `len` bytes from `at` as [`radix_block::MAX_FRAMES`] parts of pages
    /// hold. Calls `with` with them (physical address, length), which
    /// stay in place and the program's until it returns, and returns how
    /// many bytes it lent, or what `with` failed with. `None` if it
    /// cannot lend them (memory that is not the program's own, or `at`'s
    /// address not a multiple of `align`): the caller copies instead,
    /// which the default has it do.
    fn lend(
        &mut self,
        _at: usize,
        _len: usize,
        _align: usize,
        _with: Borrower<'_>,
    ) -> Option<Result<usize>> {
        None
    }
}

/// Where a write takes what it writes from: the caller's buffer.
pub trait In {
    /// Its length in bytes.
    fn len(&self) -> usize;
    /// Whether it is empty.
    fn is_empty(&self) -> bool {
        self.len() == 0
    }
    /// Copies `out.len()` bytes from `at` bytes from its start.
    fn get(&self, at: usize, out: &mut [u8]) -> Result<()>;
    /// Copies as [`In::get`] does, without waiting for a page, as
    /// [`Out::put_now`] does.
    fn get_now(&self, at: usize, out: &mut [u8]) -> Result<bool> {
        self.get(at, out).map(|()| true)
    }
    /// Makes the memory for `len` bytes from `at` ready to be read, as
    /// [`Out::fault_in`] does.
    fn fault_in(&self, _at: usize, _len: usize) -> Result<()> {
        Ok(())
    }
    /// Lends its memory to a device to read from, as [`Out::lend`] does.
    fn lend(
        &self,
        _at: usize,
        _len: usize,
        _align: usize,
        _with: Borrower<'_>,
    ) -> Option<Result<usize>> {
        None
    }
}

/// A buffer in kernel memory to read into.
pub struct SliceMut<'a>(pub &'a mut [u8]);

/// A buffer in kernel memory to write from.
pub struct Slice<'a>(pub &'a [u8]);

impl Out for SliceMut<'_> {
    fn len(&self) -> usize {
        self.0.len()
    }

    fn put(&mut self, at: usize, bytes: &[u8]) -> Result<()> {
        let end = at.checked_add(bytes.len()).ok_or(Error::BadAddress)?;
        self.0.get_mut(at..end).ok_or(Error::BadAddress)?.copy_from_slice(bytes);
        Ok(())
    }
}

impl In for Slice<'_> {
    fn len(&self) -> usize {
        self.0.len()
    }

    fn get(&self, at: usize, out: &mut [u8]) -> Result<()> {
        let end = at.checked_add(out.len()).ok_or(Error::BadAddress)?;
        out.copy_from_slice(self.0.get(at..end).ok_or(Error::BadAddress)?);
        Ok(())
    }
}

/// Called with each entry a directory lists: the cursor that continues
/// after it, its name and its kind. Returns false to stop.
pub type ListFn<'a> = &'a mut dyn FnMut(u64, &str, Kind) -> bool;

/// A file, directory or device of a filesystem.
///
/// Directory methods fail with [`Error::NotDirectory`] on other nodes,
/// and file methods with [`Error::IsDirectory`] on directories, unless a
/// node says otherwise. A filesystem hands out one node per file while
/// any reference to it lives, so that nodes can be compared by address
/// (the mount table does).
pub trait Node: Send + Sync + Any {
    /// What it is.
    fn stat(&self) -> Stat;

    /// What kind of node it is: what [`Node::stat`] says, without the
    /// rest, which a filesystem may need locks for.
    fn kind(&self) -> Kind {
        self.stat().kind
    }

    /// Whether nothing in it may change though its filesystem is mounted
    /// to write: a file or directory of a read-only Btrfs subvolume.
    fn read_only(&self) -> bool {
        false
    }

    /// The node a new handle to it uses: `None` for itself (the caller
    /// keeps the reference it has), or for a generated file a snapshot of
    /// its contents taken now, which reads see unchanged.
    fn open(&self) -> Result<Option<NodeRef>> {
        Ok(None)
    }

    /// The entry `name` of a directory.
    fn lookup(&self, _name: &str) -> Result<NodeRef> {
        Err(Error::NotDirectory)
    }

    /// Who it belongs to and its permission bits. A filesystem that keeps
    /// none makes them up: [`Access::default_for`] its kind, unless it
    /// says otherwise.
    fn access(&self) -> Access {
        Access::default_for(self.kind())
    }

    /// Changes its owner, group or mode as `change` says, and its change
    /// time. A symbolic link's mode cannot change. Nodes whose filesystem
    /// keeps no owners or modes fail with [`Error::NotSupported`], or take
    /// a change that [`AccessChange::fits_none_kept`] and change nothing.
    fn set_access(&self, _change: &AccessChange) -> Result<()> {
        Err(Error::NotSupported)
    }

    /// Makes an empty file or directory `name` in a directory, which
    /// belongs to `access`'s owner and group and has its mode, if its
    /// filesystem keeps them.
    fn create(&self, _name: &str, _kind: Kind, _access: Access) -> Result<NodeRef> {
        Err(Error::NotDirectory)
    }

    /// Makes a symbolic link `name` to `target` in a directory, which
    /// belongs to `access`'s owner and group: a directory of a filesystem
    /// that has none fails with [`Error::NotSupported`].
    fn make_link(&self, _name: &str, _target: &str, _access: Access) -> Result<NodeRef> {
        if self.kind() == Kind::Directory {
            Err(Error::NotSupported)
        } else {
            Err(Error::NotDirectory)
        }
    }

    /// Makes `name` in a directory another name of `node`, a file (not a
    /// directory) of the same filesystem: a hard link (ADR 0135). A
    /// directory of a filesystem that has none fails with
    /// [`Error::NotSupported`].
    fn link(&self, _name: &str, _node: &NodeRef) -> Result<()> {
        if self.kind() == Kind::Directory {
            Err(Error::NotSupported)
        } else {
            Err(Error::NotDirectory)
        }
    }

    /// Makes `name` in a directory a node of `kind`: a named pipe
    /// ([`Kind::Pipe`]), a device node ([`Kind::CharDevice`] or
    /// [`Kind::BlockDevice`]) that stands for device number `device`, an
    /// empty file, or a socket's name that holds no socket. A directory
    /// of a filesystem that has none fails with [`Error::NotSupported`].
    /// It belongs to `access`'s owner and group and has its mode.
    fn make_node(
        &self,
        _name: &str,
        _kind: Kind,
        _device: u64,
        _access: Access,
    ) -> Result<NodeRef> {
        if self.kind() == Kind::Directory {
            Err(Error::NotSupported)
        } else {
            Err(Error::NotDirectory)
        }
    }

    /// Makes a socket's name `name` in a directory: a file of kind
    /// [`Kind::Socket`] that holds `binding`, which
    /// [`Node::socket_binding`] gives back, and which cannot be opened,
    /// with `access`. A directory of a filesystem that has none fails
    /// with [`Error::NotSupported`].
    fn make_socket(
        &self,
        _name: &str,
        _binding: SocketBinding,
        _access: Access,
    ) -> Result<NodeRef> {
        if self.kind() == Kind::Directory {
            Err(Error::NotSupported)
        } else {
            Err(Error::NotDirectory)
        }
    }

    /// What a socket's name holds ([`Node::make_socket`]); none for any
    /// other node.
    fn socket_binding(&self) -> Option<SocketBinding> {
        None
    }

    /// Reads the path a symbolic link points to into `out`, from its
    /// start; returns its length, which is more than `out` holds if it
    /// was cut short.
    fn read_link(&self, _out: &mut dyn Out) -> Result<usize> {
        Err(Error::NotLink)
    }

    /// Removes the file or empty directory `name` from a directory.
    fn remove(&self, _name: &str) -> Result<()> {
        Err(Error::NotDirectory)
    }

    /// Makes `name` in this directory a new, empty subvolume, as Btrfs
    /// has them; a filesystem with none refuses.
    fn make_subvolume(&self, _name: &str) -> Result<()> {
        Err(Error::NotSupported)
    }

    /// Makes `name` in this directory a snapshot of the subvolume whose
    /// top directory is `source`, read-only if `read_only`.
    fn snapshot(&self, _name: &str, _source: &NodeRef, _read_only: bool) -> Result<()> {
        Err(Error::NotSupported)
    }

    /// Removes the subvolume `name` in this directory, with everything in
    /// it.
    fn remove_subvolume(&self, _name: &str) -> Result<()> {
        Err(Error::NotSupported)
    }

    /// Moves entry `from` of this directory to `to` in `to_dir`, on the
    /// same filesystem, replacing a file (or empty directory, for a
    /// directory) of that name.
    fn rename(&self, _from: &str, _to_dir: &dyn Node, _to: &str) -> Result<()> {
        Err(Error::NotDirectory)
    }

    /// Lists a directory's entries from `cursor` (0 for the first), other
    /// than `.` and `..`.
    fn list(&self, _cursor: u64, _f: ListFn<'_>) -> Result<()> {
        Err(Error::NotDirectory)
    }

    /// Reads from byte `offset` into `out`; returns how many bytes it
    /// read, fewer than asked at the end of the file.
    fn read(&self, _offset: u64, _out: &mut dyn Out) -> Result<usize> {
        Err(Error::IsDirectory)
    }

    /// Writes `data` at byte `offset`, growing the file if need be;
    /// returns how many bytes it wrote. `offset` [`APPEND`] writes at the
    /// end of the file, wherever that is when the write happens.
    fn write(&self, _offset: u64, _data: &dyn In) -> Result<usize> {
        Err(Error::IsDirectory)
    }

    /// Makes a file `size` bytes long: cut, or grown with zeros.
    fn set_size(&self, _size: u64) -> Result<()> {
        Err(Error::IsDirectory)
    }

    /// Makes this file's `len` bytes from `to` on share the disk's blocks
    /// that file `source` holds from `from` on, as Linux's `FICLONERANGE`
    /// does (`len` 0: to `source`'s end), growing it to hold them: a copy
    /// that takes no room until one of the two changes. The offsets and
    /// `len` are whole blocks, but for a range that ends at `source`'s end.
    /// A filesystem that cannot fails with [`Error::NotSupported`], and
    /// two filesystems with [`Error::CrossMount`].
    fn clone_range(&self, _source: &NodeRef, _from: u64, _len: u64, _to: u64) -> Result<()> {
        Err(Error::NotSupported)
    }

    /// Sets when it was last read and when its contents last changed, in
    /// nanoseconds since 1970 (none: as they are); a filesystem keeps them
    /// as finely as it can (FAT: two seconds, and the day for reads).
    /// Nodes that keep no times (devices, pipes, the synthetic
    /// filesystems' files) fail with [`Error::NotSupported`].
    fn set_times(&self, _accessed: Option<u64>, _modified: Option<u64>) -> Result<()> {
        Err(Error::NotSupported)
    }

    /// The value of its extended attribute `name` (ADR 0256): a name of
    /// one of Linux's namespaces, such as `user.origin` or
    /// `system.posix_acl_access`, whose value the filesystem keeps as
    /// Linux does. [`Error::NoAttribute`] if it has none of that name, and
    /// [`Error::NotSupported`] if its filesystem keeps none. Who may read
    /// which is [`attributes`]' to check.
    fn get_attribute(&self, _name: &[u8]) -> Result<KVec<u8>> {
        Err(Error::NotSupported)
    }

    /// Sets its extended attribute `name` to `value` as `how` says, and
    /// its change time: [`Error::Exists`] if it has one already and `how`
    /// is [`Setting::Create`], [`Error::NoAttribute`] if it has none and
    /// `how` is [`Setting::Replace`], and [`Error::NoSpace`] if the place
    /// the filesystem keeps them has no room for it.
    fn set_attribute(&self, _name: &[u8], _value: &[u8], _how: Setting) -> Result<()> {
        Err(Error::NotSupported)
    }

    /// The names of its extended attributes, each with a NUL after it, as
    /// listxattr(2) gives them.
    fn list_attributes(&self) -> Result<KVec<u8>> {
        Err(Error::NotSupported)
    }

    /// Removes its extended attribute `name`, and sets its change time:
    /// [`Error::NoAttribute`] if it has none of that name.
    fn remove_attribute(&self, _name: &[u8]) -> Result<()> {
        Err(Error::NotSupported)
    }

    /// Its access list ([`acl`]): what its attribute
    /// `system.posix_acl_access` holds, which a filesystem that reads it
    /// from a disk keeps in memory ([`acl::AclCell`]); none if it has none
    /// or its filesystem keeps no attributes. The gate asks for it often.
    fn access_list(&self) -> Result<Option<KArc<acl::Acl>>> {
        Ok(None)
    }

    /// A directory's default access list: what its attribute
    /// `system.posix_acl_default` holds, which what is made in it takes
    /// ([`mount::made_with`]); none if it has none. Making any node asks
    /// for it, so a filesystem that reads it from a disk keeps it in
    /// memory ([`acl::AclCell`]), as Linux keeps `i_default_acl`.
    fn default_list(&self) -> Result<Option<KArc<acl::Acl>>> {
        acl::AclCell::unread().get(|| self.get_attribute(acl::DEFAULT))
    }

    /// Writes what the node holds in memory to the disk.
    fn sync(&self) -> Result<()> {
        Ok(())
    }

    /// The block device a block device node stands for.
    fn block_device(&self) -> Option<radix_block::DeviceRef> {
        None
    }

    /// The node itself, for a filesystem to find its own type behind a
    /// [`NodeRef`] (`rename` does).
    fn as_any(&self) -> &dyn Any;
}

/// The offset of a write that appends.
pub const APPEND: u64 = u64::MAX;

/// How [`Node::set_attribute`] sets an attribute, as setxattr(2)'s flags
/// say.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Setting {
    /// Makes it, or replaces the value it has.
    Either,
    /// Makes it, failing with [`Error::Exists`] if it is there
    /// (XATTR_CREATE).
    Create,
    /// Replaces it, failing with [`Error::NoAttribute`] if it is not
    /// there (XATTR_REPLACE).
    Replace,
}

/// The path the symbolic link `node` points to: UTF-8, not empty, at most
/// [`PATH_MAX`] bytes, without NULs (what a link that holds anything else
/// leads to is [`Error::NotFound`]).
pub fn link_target(node: &dyn Node) -> Result<KVec<u8>> {
    let size = usize::try_from(node.stat().size).unwrap_or(usize::MAX).min(PATH_MAX);
    let mut target = mem::vec_with_capacity(size)?;
    while target.len() < size {
        // Room was made above.
        let _ = target.push_within_capacity(0);
    }
    let len = node.read_link(&mut SliceMut(&mut target))?;
    if len > size || len == 0 {
        return Err(Error::NotFound);
    }
    target.truncate(len);
    if target.contains(&0) || core::str::from_utf8(&target).is_err() {
        return Err(Error::NotFound);
    }
    Ok(target)
}

/// What kind of filesystem one is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FsKind {
    /// Memory only.
    Tmpfs,
    /// FAT12, FAT16 or FAT32 on a disk.
    Fat,
    /// Btrfs on a disk.
    Btrfs,
    /// exFAT on a disk.
    Exfat,
    /// ext4 (or ext2 or ext3) on a disk.
    Ext4,
    /// NTFS on a disk.
    Ntfs,
    /// ISO 9660 on a CD, DVD or disc image, with Joliet and Rock Ridge,
    /// read-only.
    Iso9660,
    /// UDF on a DVD, Blu-ray disc, disc image or disk, read-only.
    Udf,
    /// The processes (`/processes`).
    Proc,
    /// The kernel's devices and settings (`/kernel`).
    Sys,
    /// The devices (`/devices`).
    Dev,
    /// The boot archive (`/system`), read-only.
    Boot,
    /// Pipes and pseudo-terminals, which live outside the tree (Phase 8):
    /// their files are on a mount of their own ([`mount::Mount::detached`]).
    Pipe,
    /// Served by a program through `/devices/fuse`, in Linux's FUSE
    /// protocol (ADR 0226): a network share, for one.
    Fuse,
}

impl FsKind {
    /// Its name, as `mount` takes it.
    pub fn name(self) -> &'static str {
        match self {
            FsKind::Tmpfs => "tmpfs",
            FsKind::Fat => "fat",
            FsKind::Btrfs => "btrfs",
            FsKind::Exfat => "exfat",
            FsKind::Ext4 => "ext4",
            FsKind::Ntfs => "ntfs",
            FsKind::Iso9660 => "iso9660",
            FsKind::Udf => "udf",
            FsKind::Proc => "proc",
            FsKind::Sys => "sys",
            FsKind::Dev => "dev",
            FsKind::Boot => "bootfs",
            FsKind::Pipe => "pipefs",
            FsKind::Fuse => "fuse",
        }
    }
}

/// How full a filesystem is.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct FsInfo {
    /// The size of its blocks (clusters, pages), in bytes.
    pub block_size: u32,
    /// How many blocks it has.
    pub blocks: u64,
    /// How many are free.
    pub free: u64,
    /// How many files and directories it can hold; zero when it keeps no
    /// count (FAT, the synthetic filesystems), as Linux's vfat and procfs
    /// say.
    pub files: u64,
    /// How many more it can take.
    pub free_files: u64,
}

/// What a scrub of a filesystem read and found, counted as Linux's
/// `struct btrfs_scrub_progress` counts it: each copy of a block is
/// counted, and each of its errors.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Scrub {
    /// The data extents read, a copy each.
    pub data_extents: u64,
    /// The tree blocks read, a copy each.
    pub tree_extents: u64,
    /// The bytes of data read.
    pub data_bytes: u64,
    /// The bytes of tree blocks read.
    pub tree_bytes: u64,
    /// The sectors or blocks the disk failed to read.
    pub read_errors: u64,
    /// The sectors or blocks whose checksum differs.
    pub csum_errors: u64,
    /// The tree blocks whose checksum matches but whose header does not
    /// (its address, filesystem, level or generation).
    pub verify_errors: u64,
    /// The data sectors with no checksum (`nodatasum` files).
    pub no_csum: u64,
    /// The checksums of data no extent holds.
    pub csum_discards: u64,
    /// The copies of the superblock that are bad or old.
    pub super_errors: u64,
    /// The times memory ran out.
    pub malloc_errors: u64,
    /// The sectors or blocks with no good copy.
    pub uncorrectable_errors: u64,
    /// The bad copies written again from a good one.
    pub corrected_errors: u64,
    /// Where on the device the scrub got to.
    pub last_physical: u64,
    /// The errors a second read did not show.
    pub unverified_errors: u64,
    /// The filesystem's UUID.
    pub fsid: [u8; 16],
}

/// Linux's mark for a balance's convert to one copy of each block,
/// `BTRFS_AVAIL_ALLOC_BIT_SINGLE`; the other profiles are their block
/// group flags.
pub const BALANCE_SINGLE: u64 = 1 << 48;

/// What a balance does with one kind of chunk, as btrfs-balance(8)'s
/// `-d`, `-m` and `-s` say (ADR 0159).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct BalanceArgs {
    /// Whether chunks of this kind move.
    pub on: bool,
    /// The profile they are converted to: [`BALANCE_SINGLE`] or a block
    /// group profile flag; 0 keeps each chunk's.
    pub convert: u64,
    /// Leaves alone the chunks already of the profile `convert` names, as
    /// the `soft` filter does.
    pub soft: bool,
}

/// The chunks a balance moves: of data, of metadata, and of the chunk
/// tree.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Balance {
    /// Data chunks.
    pub data: BalanceArgs,
    /// Metadata chunks.
    pub metadata: BalanceArgs,
    /// Chunks of the chunk tree.
    pub system: BalanceArgs,
}

/// What a balance did, as Linux's `btrfs_balance_progress` counts it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Balanced {
    /// The chunks it looked at.
    pub considered: u64,
    /// The chunks it moved.
    pub moved: u64,
}

/// A quota group of a Btrfs filesystem with simple quotas, as `btrfs
/// qgroup show` lists it (ADR 0170): its subvolume's, with its limits.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Qgroup {
    /// Its ID: the level in the high 16 bits, 0 for a subvolume's, and the
    /// subvolume's ID below.
    pub id: u64,
    /// The bytes of the extents its subvolume wrote.
    pub referenced: u64,
    /// The same bytes, as simple quotas count each extent once.
    pub exclusive: u64,
    /// The most `referenced` may grow to; `u64::MAX` for no limit.
    pub max_referenced: u64,
    /// The most `exclusive` may grow to; `u64::MAX` for no limit.
    pub max_exclusive: u64,
}

/// A filesystem.
pub trait FileSystem: Send + Sync {
    /// What kind it is.
    fn kind(&self) -> FsKind;
    /// Its root directory.
    fn root(&self) -> NodeRef;
    /// How full it is.
    fn info(&self) -> FsInfo;
    /// Whether it refuses every change.
    fn read_only(&self) -> bool {
        false
    }
    /// Its label, if it has one.
    fn label(&self, _out: &mut dyn fmt::Write) -> fmt::Result {
        Ok(())
    }
    /// Writes everything it holds in memory to the disk.
    fn sync(&self) -> Result<()> {
        Ok(())
    }
    /// Whether it holds changes the disk has not seen.
    fn is_dirty(&self) -> bool {
        false
    }
    /// Reads every block it holds, every copy, and checks each against
    /// its checksum, as Linux's `btrfs scrub` does; writes a good copy
    /// over a bad one if `repair`. A filesystem that keeps no checksums
    /// fails with [`Error::NotSupported`].
    fn scrub(&self, _repair: bool) -> Result<Scrub> {
        Err(Error::NotSupported)
    }
    /// Puts `device` in the place of its disk of device ID `devid`, there
    /// or missing, and rebuilds onto it from the copies on its other
    /// disks, as Linux's `btrfs replace start -B` does; returns what the
    /// rebuild read and found, as [`FileSystem::scrub`] counts it. A
    /// filesystem that keeps no copies on other disks fails with
    /// [`Error::NotSupported`].
    fn replace_disk(&self, _devid: u64, _device: radix_block::DeviceRef) -> Result<Scrub> {
        Err(Error::NotSupported)
    }
    /// Discards on its disks the bytes it does not use, in ranges of at
    /// least `minimum` bytes, as Linux's FITRIM does; returns how many
    /// bytes it discarded. A filesystem that cannot fails with
    /// [`Error::NotSupported`], as does one on disks that discard nothing.
    fn trim(&self, _minimum: u64) -> Result<u64> {
        Err(Error::NotSupported)
    }
    /// Adds `device` to its disks, as Linux's `btrfs device add` does;
    /// returns the device ID it takes. It holds nothing until a balance
    /// moves chunks onto it, or new ones go there. A filesystem that
    /// cannot fails with [`Error::NotSupported`].
    fn add_disk(&self, _device: radix_block::DeviceRef) -> Result<u64> {
        Err(Error::NotSupported)
    }
    /// Moves every chunk off its disk of device ID `devid`, there or
    /// missing, then takes the disk out, as Linux's `btrfs device remove`
    /// does. A filesystem that cannot fails with [`Error::NotSupported`].
    fn remove_disk(&self, _devid: u64) -> Result<()> {
        Err(Error::NotSupported)
    }
    /// Moves the chunks `plan` names to new ones, converting them to
    /// another profile if it says so, as Linux's `btrfs balance start`
    /// does. A filesystem that cannot fails with [`Error::NotSupported`].
    fn balance(&self, _plan: &Balance) -> Result<Balanced> {
        Err(Error::NotSupported)
    }
    /// Turns simple quotas on, with `enable`, or off, as Linux's `btrfs
    /// quota enable --simple` and `btrfs quota disable` do. A filesystem
    /// that cannot fails with [`Error::NotSupported`].
    fn quota(&self, _enable: bool) -> Result<()> {
        Err(Error::NotSupported)
    }
    /// Its quota groups, in order of their IDs. A filesystem that keeps
    /// none fails with [`Error::NotSupported`].
    fn qgroups(&self) -> Result<KVec<Qgroup>> {
        Err(Error::NotSupported)
    }
    /// Sets the limits of quota group `id`; `u64::MAX` is none. A
    /// filesystem that keeps no quota groups fails with
    /// [`Error::NotSupported`].
    fn limit_qgroup(&self, _id: u64, _max_referenced: u64, _max_exclusive: u64) -> Result<()> {
        Err(Error::NotSupported)
    }
    /// Drops up to `max` pages it can read again from the disk; returns
    /// how many it dropped.
    fn shrink(&self, _max: usize) -> usize {
        0
    }
    /// Whether letting go of the last reference to one of its nodes only
    /// frees memory, so that it may happen with any lock held. A FAT file
    /// removed while open frees its clusters on the disk then, so FAT
    /// says no, as does any filesystem that does not say.
    fn light_drops(&self) -> bool {
        false
    }
}

/// A filesystem, shared.
pub type FsRef = KArc<dyn FileSystem>;

/// The node behind `node`, as type `T`, if it is one.
pub fn downcast<T: Node>(node: &dyn Node) -> Option<&T> {
    node.as_any().downcast_ref::<T>()
}

/// The address of a node: equal for two references to the same node.
pub fn address(node: &NodeRef) -> usize {
    KArc::as_ptr(node).cast::<u8>() as usize
}

static CLOCK: radix_sync::Once<fn() -> u64> = radix_sync::Once::new();

/// Registers the wall clock: nanoseconds since 1970 (UTC), for files'
/// times.
pub fn set_clock(now: fn() -> u64) {
    let _ = CLOCK.set(now);
}

/// The time now, in nanoseconds since 1970, or 0 before a clock is set.
pub fn now() -> u64 {
    CLOCK.get().map_or(0, |now| now())
}

static RANDOM: radix_sync::Once<fn(&mut [u8])> = radix_sync::Once::new();

/// Registers the kernel's random bytes, for what a filesystem makes
/// unique, such as a new subvolume's UUID.
pub fn set_random(fill: fn(&mut [u8])) {
    let _ = RANDOM.set(fill);
}

/// Fills `out` with random bytes: the kernel's once they are set, and
/// before that, as in host tests, bytes from the clock and a count,
/// unlike each other but not unpredictable.
pub fn random(out: &mut [u8]) {
    if let Some(fill) = RANDOM.get() {
        return fill(out);
    }
    static COUNT: AtomicU64 = AtomicU64::new(0);
    for chunk in out.chunks_mut(8) {
        // SplitMix64's step.
        let n = COUNT.fetch_add(1, Ordering::Relaxed);
        let mut x = now().wrapping_add(n.wrapping_mul(0x9e37_79b9_7f4a_7c15));
        x = (x ^ (x >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        x = (x ^ (x >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        x ^= x >> 31;
        chunk.copy_from_slice(&x.to_le_bytes()[..chunk.len()]);
    }
}

/// A kernel's [`join`]: runs its first argument on the caller's CPU and
/// its second on another, and returns when both are done.
pub type Join = fn(&mut dyn FnMut(), &mut (dyn FnMut() + Send));

static JOIN: radix_sync::Once<Join> = radix_sync::Once::new();

/// Registers the kernel's [`join`], which it gives only with a second CPU
/// (ADR 0133).
pub fn set_join(join: Join) {
    let _ = JOIN.set(join);
}

/// Whether [`join`] may run its halves on two CPUs.
pub fn joins() -> bool {
    JOIN.get().is_some()
}

/// Runs `a` here and `b` on another CPU when one is free, and returns when
/// both are done; without a kernel's [`set_join`], `a`, then `b`. It may
/// sleep, so the caller holds no spin lock.
pub fn join(a: impl FnOnce(), b: impl FnOnce() + Send) {
    let (mut a, mut b) = (Some(a), Some(b));
    let mut a = || {
        if let Some(a) = a.take() {
            a();
        }
    };
    let mut b = || {
        if let Some(b) = b.take() {
            b();
        }
    };
    match JOIN.get() {
        Some(join) => join(&mut a, &mut b),
        None => {
            a();
            b();
        }
    }
}

static NEXT_ID: AtomicU64 = AtomicU64::new(1);

/// A number for a new filesystem, unique since boot.
pub fn next_id() -> u64 {
    NEXT_ID.fetch_add(1, Ordering::Relaxed)
}

/// Text built in memory, for generated files: a [`fmt::Write`] that grows
/// on the heap and stops at [`Text::LIMIT`].
pub struct Text {
    bytes: KVec<u8>,
    failed: bool,
}

impl Default for Text {
    fn default() -> Self {
        Text::new()
    }
}

impl Text {
    /// The most bytes a generated file holds.
    pub const LIMIT: usize = 1 << 20;

    /// Empty text.
    pub fn new() -> Text {
        Text { bytes: mem::vec(), failed: false }
    }

    /// What was written.
    pub fn bytes(&self) -> &[u8] {
        &self.bytes
    }

    /// Whether memory ran out, or the limit was reached.
    pub fn failed(&self) -> bool {
        self.failed
    }

    /// The bytes, unless writing them failed.
    pub fn finish(self) -> Result<KVec<u8>> {
        if self.failed { Err(Error::NoMemory) } else { Ok(self.bytes) }
    }

    /// Adds `bytes`, which need not be UTF-8 (a program's arguments).
    pub fn write_bytes(&mut self, bytes: &[u8]) -> fmt::Result {
        if self.bytes.len() + bytes.len() > Text::LIMIT
            || mem::extend(&mut self.bytes, bytes).is_err()
        {
            self.failed = true;
            return Err(fmt::Error);
        }
        Ok(())
    }
}

impl fmt::Write for Text {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        self.write_bytes(s.as_bytes())
    }
}
