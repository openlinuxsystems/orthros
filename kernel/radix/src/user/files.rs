// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! The methods of directories and files (radix-directory(2),
//! radix-file(2)), and the ring's read, write and sync operations.
//!
//! A directory capability names a place in the tree (`fs::FileObject`);
//! every path a method takes is copied in from the caller, checked, and
//! resolved beneath that place (`radix_vfs::mount::Vfs`), so a program
//! reaches only what its directory capabilities lead to. Data moves
//! between the caller's memory and the page cache's pages, a piece at a
//! time, without a buffer of its own in between. The capability `open`
//! makes has the rights the caller asked for and its directory has, never
//! more, and the object it names is charged to the caller's process.
//!
//! Since Phase 8 a file object is an open file description: reads and
//! writes at CURRENT use its offset, and those of a pipe's end, a
//! socket or a terminal's side go to the pipe, the socket or the terminal
//! (`fs::pipe`, `fs::socket`, `fs::tty`), which may wait. Opening a
//! terminal makes it the controlling terminal of a session leader that
//! has none, unless asked not to. Since Phase 9 directories name sockets
//! and connect them, and sockets listen, accept, and send files.

use core::mem::size_of;

use radix_abi::capability::SlotFlags;
use radix_abi::directory::{
    self, FileKind, FilesystemKind, LinkFlags, MountFlags, OpenFlags, PathFlags,
};
use radix_abi::errors::Kernel;
use radix_abi::file::{DescriptionFlags, LockKind, LockOwner, SocketFlags, Whence};
use radix_abi::process::{LimitKind, MAX_SLOTS};
use radix_abi::startup::SLOT_FD_BASE;
use radix_abi::{UserBuffer, file, terminal};
use radix_cap::{CapError, ObjectKind, Rights, SlotRef};
use radix_hal::UserMode;
use radix_klog::info;
use radix_mm::{Flags, HEAP};
use radix_vfs::mount::{Location, OpenFlags as VfsOpen};
use radix_vfs::{FsKind, In, Kind, NodeRef, Out, PATH_MAX};

use super::account::Account;
use super::memory::cap_rights;
use super::methods::{Answer, answer};
use super::objects::{self, Found, Object};
use super::{Result, UserRef, UserThread, cap_error, error};
use crate::fs::{self, Description, FileObject, FileRef, locks, pipe, socket, tty};
use crate::{Arch, caps};

/// The most bytes one read or write moves, as on Linux.
pub const IO_MAX: u64 = 0x7fff_f000;

/// A path copied in from a program: on the stack when it is short, on the
/// heap when it is long. A method copies it into a place of its own
/// ([`Path::Empty`], then [`Path::copy`]), where it stays: returning a
/// short one would move its 136 bytes twice, each a string move.
enum Path {
    /// No path yet, for [`Path::copy`] to fill: making one stores its tag
    /// alone.
    Empty,
    Short([u8; 128], usize),
    Long(alloc::vec::Vec<u8, radix_mm::Kmalloc>),
}

impl Path {
    /// Copies the path in `buffer` from the caller into this one, and
    /// gives it back.
    #[inline(always)]
    fn copy(&mut self, buffer: UserBuffer) -> Result<&Path> {
        if buffer.len > PATH_MAX as u64 {
            return Err(fs::status(radix_vfs::Error::NameTooLong));
        }
        if !radix_ipc::range::user_bytes(buffer.address, buffer.len) {
            return Err(error(Kernel::BadAddress));
        }
        let len = buffer.len as usize;
        if len > 128 {
            let mut bytes = alloc::vec::Vec::new_in(HEAP.with(Flags::MAY_SLEEP));
            bytes.try_reserve_exact(len).map_err(|_| error(Kernel::NoMemory))?;
            while bytes.len() < len {
                // Room was reserved above.
                let _ = bytes.push_within_capacity(0);
            }
            *self = Path::Long(bytes);
        } else {
            *self = Path::Short([0; 128], len);
        }
        let bytes = match self {
            Path::Empty => &mut [],
            Path::Short(bytes, len) => &mut bytes[..*len],
            Path::Long(bytes) => &mut bytes[..],
        };
        if !Arch::copy_from_user(bytes, buffer.address) {
            return Err(error(Kernel::BadAddress));
        }
        Ok(self)
    }

    fn bytes(&self) -> &[u8] {
        match self {
            Path::Empty => &[],
            Path::Short(bytes, len) => &bytes[..*len],
            Path::Long(bytes) => &bytes[..],
        }
    }

    /// The path, checked (radix_vfs::path::parse).
    fn as_str(&self) -> Result<&str> {
        radix_vfs::path::parse(self.bytes()).map_err(fs::status)
    }

    /// The bytes as a symbolic link's target: UTF-8, which may start with
    /// `/` (the VFS checks the rest).
    fn as_target(&self) -> Result<&str> {
        core::str::from_utf8(self.bytes()).map_err(|_| fs::status(radix_vfs::Error::BadName))
    }
}

/// A caller's buffer to read into, checked to be in user memory.
struct UserOut {
    address: u64,
    len: usize,
}

impl Out for UserOut {
    fn len(&self) -> usize {
        self.len
    }

    fn put(&mut self, at: usize, bytes: &[u8]) -> radix_vfs::Result<()> {
        if at.checked_add(bytes.len()).is_none_or(|end| end > self.len)
            || !Arch::copy_to_user(self.address + at as u64, bytes)
        {
            return Err(radix_vfs::Error::BadAddress);
        }
        Ok(())
    }

    fn put_now(&mut self, at: usize, bytes: &[u8]) -> radix_vfs::Result<bool> {
        if at.checked_add(bytes.len()).is_none_or(|end| end > self.len) {
            return Err(radix_vfs::Error::BadAddress);
        }
        Ok(super::copy_to_user_now(self.address + at as u64, bytes))
    }

    fn fault_in(&mut self, at: usize, len: usize) -> radix_vfs::Result<()> {
        if at.checked_add(len).is_none_or(|end| end > self.len)
            || !super::fault_in(self.address + at as u64, len, true)
        {
            return Err(radix_vfs::Error::BadAddress);
        }
        Ok(())
    }

    fn lend(
        &mut self,
        at: usize,
        len: usize,
        align: usize,
        with: radix_vfs::Borrower<'_>,
    ) -> Option<radix_vfs::Result<usize>> {
        lend(self.address, self.len, (at, len, align), true, with)
    }
}

/// A caller's buffer to write from.
struct UserIn {
    address: u64,
    len: usize,
}

impl In for UserIn {
    fn len(&self) -> usize {
        self.len
    }

    fn get(&self, at: usize, out: &mut [u8]) -> radix_vfs::Result<()> {
        if at.checked_add(out.len()).is_none_or(|end| end > self.len)
            || !Arch::copy_from_user(out, self.address + at as u64)
        {
            return Err(radix_vfs::Error::BadAddress);
        }
        Ok(())
    }

    fn get_now(&self, at: usize, out: &mut [u8]) -> radix_vfs::Result<bool> {
        if at.checked_add(out.len()).is_none_or(|end| end > self.len) {
            return Err(radix_vfs::Error::BadAddress);
        }
        Ok(super::copy_from_user_now(out, self.address + at as u64))
    }

    fn fault_in(&self, at: usize, len: usize) -> radix_vfs::Result<()> {
        if at.checked_add(len).is_none_or(|end| end > self.len)
            || !super::fault_in(self.address + at as u64, len, false)
        {
            return Err(radix_vfs::Error::BadAddress);
        }
        Ok(())
    }

    fn lend(
        &self,
        at: usize,
        len: usize,
        align: usize,
        with: radix_vfs::Borrower<'_>,
    ) -> Option<radix_vfs::Result<usize>> {
        lend(self.address, self.len, (at, len, align), false, with)
    }
}

/// [`Out::lend`] and [`In::lend`] for a caller's buffer of `size` bytes
/// at `address`: `len` bytes from `at`, as far as
/// [`radix_block::MAX_FRAMES`] parts of pages reach, if their address is
/// a multiple of `align`, to a device that writes them if `write`. A
/// range outside the buffer, or memory that cannot be lent, is copied
/// instead, which reports the bad address if there is one.
fn lend(
    address: u64,
    size: usize,
    (at, len, align): (usize, usize, usize),
    write: bool,
    with: radix_vfs::Borrower<'_>,
) -> Option<radix_vfs::Result<usize>> {
    let start = address + at as u64;
    if len == 0
        || at.checked_add(len).is_none_or(|end| end > size)
        || align == 0
        || !start.is_multiple_of(align as u64)
    {
        return None;
    }
    let page = radix_abi::address_space::PAGE_SIZE;
    let head = (page - start % page) as usize;
    let n = len.min(head + (radix_block::MAX_FRAMES - 1) * page as usize);
    super::lend(start, n, write, |frames| with(frames).map(|()| n))
}

/// Checks a caller's buffer and caps it at [`IO_MAX`].
fn user_range(buffer: UserBuffer) -> Result<(u64, usize)> {
    let len = buffer.len.min(IO_MAX);
    if !radix_ipc::range::user_bytes(buffer.address, len) {
        return Err(error(Kernel::BadAddress));
    }
    Ok((buffer.address, len as usize))
}

fn vfs() -> Result<&'static radix_vfs::mount::Vfs> {
    fs::vfs().ok_or(error(Kernel::Gone))
}

fn file_kind(kind: Kind) -> FileKind {
    match kind {
        Kind::File => FileKind::File,
        Kind::Directory => FileKind::Directory,
        Kind::BlockDevice => FileKind::BlockDevice,
        Kind::CharDevice => FileKind::CharDevice,
        Kind::Pipe => FileKind::Pipe,
        Kind::Symlink => FileKind::Symlink,
        Kind::Socket => FileKind::Socket,
    }
}

fn filesystem_kind(kind: FsKind) -> FilesystemKind {
    match kind {
        FsKind::Tmpfs => FilesystemKind::Tmpfs,
        FsKind::Fat => FilesystemKind::Fat,
        FsKind::Btrfs => FilesystemKind::Btrfs,
        FsKind::Exfat => FilesystemKind::Exfat,
        FsKind::Ext4 => FilesystemKind::Ext4,
        FsKind::Proc => FilesystemKind::Proc,
        FsKind::Sys => FilesystemKind::Sys,
        FsKind::Dev => FilesystemKind::Dev,
        FsKind::Boot => FilesystemKind::Bootfs,
        FsKind::Pipe => FilesystemKind::Pipe,
    }
}

/// The rights a directory capability's `open` hands on, besides copy and
/// transfer.
const OPEN_RIGHTS: Rights = Rights::READ.union(Rights::WRITE).union(Rights::CONTROL);

/// The directory `found` designates, if its capability has the rights
/// `needs`: `found` is what the system call found in the slot the caller
/// named, so the slot is not looked at again.
fn directory_of(found: &Found, needs: Rights) -> Result<&FileRef> {
    found.cap.check(ObjectKind::Directory, needs).map_err(cap_error)?;
    match &found.object {
        Some(Object::Directory(dir)) => Ok(dir),
        _ => Err(error(Kernel::Gone)),
    }
}

/// The file `found` designates, if its capability has the rights `needs`,
/// as [`directory_of`].
fn file_of(found: &Found, needs: Rights) -> Result<&FileRef> {
    found.cap.check(ObjectKind::File, needs).map_err(cap_error)?;
    match &found.object {
        Some(Object::File(file)) => Ok(file),
        _ => Err(error(Kernel::Gone)),
    }
}

/// `control`'s CLONE and CLONE_RANGE (ADR 0139): the file `file`, which
/// `found` holds open for writing, takes the data of the one the request
/// names, open for reading, as Linux's FICLONE and FICLONERANGE.
fn clone_file(user: &UserRef, found: &Found, file: &FileRef, a: &file::ControlArgs) -> Result<()> {
    found.cap.check(ObjectKind::File, Rights::WRITE).map_err(cap_error)?;
    // Linux's struct file_clone_range: the source, its offset, the length
    // and the offset in the file.
    let mut range = [0u64; 4];
    if a.request == file::CLONE {
        range[0] = a.argument;
    } else {
        let mut bytes = [0u8; 32];
        if a.size < 32 || !Arch::copy_from_user(&mut bytes, a.buffer) {
            return Err(error(Kernel::BadAddress));
        }
        for (word, chunk) in range.iter_mut().zip(bytes.as_chunks::<8>().0) {
            *word = u64::from_le_bytes(*chunk);
        }
    }
    let slot = u32::try_from(range[0]).map_err(|_| error(Kernel::NoSuchSlot))?;
    let (_, source) = objects::file(user.cspace, slot, Rights::READ)?;
    file.at.writable().map_err(fs::status)?;
    // A filesystem that cannot clone is EOPNOTSUPP, as on Linux.
    file.at.node.clone_range(&source.at.node, range[1], range[2], range[3]).map_err(|err| match err
    {
        radix_vfs::Error::NotSupported => Kernel::NotSupported.status(0),
        err => fs::status(err),
    })
}

/// The directory interface, on the directory `found` in a slot.
pub fn directory_method(
    user: &UserRef,
    found: &Found,
    label: u32,
    args: &[u64; 8],
) -> Result<Answer> {
    let cspace = user.cspace;
    match label {
        directory::OPEN => {
            let a = directory::OpenArgs::decode(args)?;
            // Linux refuses to make a directory by open since 6.4.
            if a.flags.contains(OpenFlags::CREATE) && a.flags.contains(OpenFlags::DIRECTORY) {
                return Err(error(Kernel::InvalidArgument));
            }
            let asked = cap_rights(a.rights).intersection(OPEN_RIGHTS);
            let changes =
                a.flags.contains(OpenFlags::CREATE) || a.flags.contains(OpenFlags::TRUNCATE);
            let mut needs =
                Rights::READ.union(asked.intersection(Rights::READ.union(Rights::WRITE)));
            if changes {
                needs = needs.union(Rights::WRITE);
            }
            let dir = directory_of(found, needs)?;
            let to = SlotRef::new(cspace, a.to);
            // The slot first when the open changes something, so that a
            // file is not made or cut for nothing; otherwise putting the
            // capability there finds out (`install`).
            if changes {
                match crate::caps::get(to) {
                    Err(CapError::Empty) => {}
                    Ok(_) => return Err(error(Kernel::Occupied)),
                    Err(err) => return Err(cap_error(err)),
                }
            }
            let mut path = Path::Empty;
            let path = path.copy(a.path)?;
            let flags = VfsOpen {
                create: a.flags.contains(OpenFlags::CREATE),
                exclusive: a.flags.contains(OpenFlags::EXCLUSIVE),
                truncate: a.flags.contains(OpenFlags::TRUNCATE),
                directory: a.flags.contains(OpenFlags::DIRECTORY),
                no_follow: a.flags.contains(OpenFlags::NO_FOLLOW),
            };
            let mut at = vfs()?.open(&dir.at, path.as_str()?, flags).map_err(fs::status)?;
            let description = Description {
                append: a.flags.contains(OpenFlags::APPEND),
                non_blocking: a.flags.contains(OpenFlags::NON_BLOCKING),
            };
            let close_on_exec = a.flags.contains(OpenFlags::CLOSE_ON_EXEC);
            let kind = at.node.kind();
            let object_kind =
                if kind == Kind::Directory { ObjectKind::Directory } else { ObjectKind::File };
            let wanted = asked.intersection(object_kind.rights());
            let missing = wanted.difference(found.cap.rights());
            if !missing.is_empty() {
                return Err(cap_error(CapError::MissingRights(missing)));
            }
            // A filesystem's named pipe, device node or socket's name opens
            // what it stands for, on a read-only mount too (ADR 0135).
            if let Some(node) = stands_for(&at, wanted, description.non_blocking)? {
                at.node = node;
            } else if object_kind == ObjectKind::File && wanted.contains(Rights::WRITE) {
                at.writable().map_err(fs::status)?;
            }
            let rights = wanted.union(Rights::COPY).union(Rights::TRANSFER);
            // The bytes the VFS took as a path above.
            let from = Some((&dir.opened, path.bytes()));
            let object =
                FileObject::opened(at, description, from).ok_or(error(Kernel::NoMemory))?;
            let terminal = object.tty_handle().is_some().then(|| object.at.node.clone());
            install_file(user, to, object_kind, rights, object, close_on_exec)?;
            if let Some(node) = terminal
                && let Some(handle) = tty::handle_of(&node)
                && !handle.master
                && !a.flags.contains(OpenFlags::NO_TERMINAL)
            {
                tty::Tty::opened_by(handle.tty(), &user.process);
            }
            let results = directory::OpenResults { kind: file_kind(kind) };
            answer(label, directory::OpenResults::WORDS, results.encode())
        }
        directory::MAKE_DIRECTORY => {
            let a = directory::MakeDirectoryArgs::decode(args)?;
            let dir = directory_of(found, Rights::WRITE)?;
            let mut path = Path::Empty;
            let path = path.copy(a.path)?;
            vfs()?.make_directory(&dir.at, path.as_str()?).map_err(fs::status)?;
            answer(label, 0, [0; 8])
        }
        directory::REMOVE => {
            let a = directory::RemoveArgs::decode(args)?;
            let dir = directory_of(found, Rights::WRITE)?;
            let mut path = Path::Empty;
            let path = path.copy(a.path)?;
            vfs()?.remove(&dir.at, path.as_str()?).map_err(fs::status)?;
            answer(label, 0, [0; 8])
        }
        directory::RENAME => {
            let a = directory::RenameArgs::decode(args)?;
            let dir = directory_of(found, Rights::WRITE)?;
            let (_, to_dir) = objects::directory(cspace, a.to_dir, Rights::WRITE)?;
            let (mut from, mut to) = (Path::Empty, Path::Empty);
            let (from, to) = (from.copy(a.from)?, to.copy(a.to)?);
            vfs()?.rename(&dir.at, from.as_str()?, &to_dir.at, to.as_str()?).map_err(fs::status)?;
            answer(label, 0, [0; 8])
        }
        directory::LIST => {
            let a = directory::ListArgs::decode(args)?;
            let dir = directory_of(found, Rights::READ)?;
            let (address, len) = user_range(a.buffer)?;
            let results = list(&dir.at, a.cursor, address, len)?;
            answer(label, directory::ListResults::WORDS, results.encode())
        }
        directory::STAT => {
            let a = directory::StatArgs::decode(args)?;
            let dir = directory_of(found, Rights::READ)?;
            let mut path = Path::Empty;
            let path = path.copy(a.path)?;
            let at = resolve(&dir.at, path.as_str()?, a.flags)?;
            put_status(&at.node.stat(), at.mount.id, a.buffer)?;
            answer(label, 0, [0; 8])
        }
        directory::SET_TIMES => {
            let a = directory::SetTimesArgs::decode(args)?;
            let dir = directory_of(found, Rights::WRITE)?;
            let mut path = Path::Empty;
            let path = path.copy(a.path)?;
            let at = resolve(&dir.at, path.as_str()?, a.flags)?;
            set_times(&at, a.accessed, a.modified)?;
            answer(label, 0, [0; 8])
        }
        directory::MAKE_LINK => {
            let a = directory::MakeLinkArgs::decode(args)?;
            let dir = directory_of(found, Rights::WRITE)?;
            let (mut path, mut target) = (Path::Empty, Path::Empty);
            let (path, target) = (path.copy(a.path)?, target.copy(a.target)?);
            vfs()?
                .make_link(&dir.at, path.as_str()?, target.as_target()?)
                .map_err(refused_as_not_permitted)?;
            answer(label, 0, [0; 8])
        }
        directory::LINK => {
            let a = directory::LinkArgs::decode(args)?;
            let dir = directory_of(found, Rights::READ)?;
            let (_, to_dir) = objects::directory(cspace, a.to_dir, Rights::WRITE)?;
            let (mut from, mut to) = (Path::Empty, Path::Empty);
            let (from, to) = (from.copy(a.path)?, to.copy(a.to)?);
            let follow = a.flags.contains(LinkFlags::FOLLOW);
            vfs()?
                .link(&dir.at, from.as_str()?, &to_dir.at, to.as_str()?, follow)
                .map_err(refused_as_not_permitted)?;
            answer(label, 0, [0; 8])
        }
        directory::MAKE_NODE => {
            let a = directory::MakeNodeArgs::decode(args)?;
            let dir = directory_of(found, Rights::WRITE)?;
            let kind = match a.kind {
                FileKind::File => Kind::File,
                FileKind::Pipe => Kind::Pipe,
                FileKind::CharDevice => Kind::CharDevice,
                FileKind::BlockDevice => Kind::BlockDevice,
                FileKind::Socket => Kind::Socket,
                _ => return Err(error(Kernel::InvalidArgument)),
            };
            let mut path = Path::Empty;
            let path = path.copy(a.path)?;
            vfs()?
                .make_node(&dir.at, path.as_str()?, kind, a.device)
                .map_err(refused_as_not_permitted)?;
            answer(label, 0, [0; 8])
        }
        directory::MAKE_SUBVOLUME => {
            let a = directory::MakeSubvolumeArgs::decode(args)?;
            let dir = directory_of(found, Rights::WRITE)?;
            let mut path = Path::Empty;
            let path = path.copy(a.path)?;
            vfs()?.make_subvolume(&dir.at, path.as_str()?).map_err(fs::status)?;
            answer(label, 0, [0; 8])
        }
        directory::SNAPSHOT => {
            let a = directory::SnapshotArgs::decode(args)?;
            let dir = directory_of(found, Rights::READ)?;
            let (_, to_dir) = objects::directory(cspace, a.to_dir, Rights::WRITE)?;
            let (mut from, mut to) = (Path::Empty, Path::Empty);
            let (from, to) = (from.copy(a.path)?, to.copy(a.to)?);
            vfs()?
                .snapshot(&dir.at, from.as_str()?, &to_dir.at, to.as_str()?, a.read_only)
                .map_err(fs::status)?;
            answer(label, 0, [0; 8])
        }
        directory::REMOVE_SUBVOLUME => {
            let a = directory::RemoveSubvolumeArgs::decode(args)?;
            let dir = directory_of(found, Rights::WRITE)?;
            let mut path = Path::Empty;
            let path = path.copy(a.path)?;
            vfs()?.remove_subvolume(&dir.at, path.as_str()?).map_err(fs::status)?;
            answer(label, 0, [0; 8])
        }
        directory::SCRUB => {
            let a = directory::ScrubArgs::decode(args)?;
            // Mending writes to the disk.
            let dir = directory_of(found, if a.repair { Rights::WRITE } else { Rights::READ })?;
            let (address, len) = user_range(a.buffer)?;
            if len < directory::ScrubProgress::SIZE {
                return Err(error(Kernel::InvalidArgument));
            }
            let mount = &dir.at.mount;
            if a.repair && mount.read_only() {
                return Err(fs::status(radix_vfs::Error::ReadOnly));
            }
            let counts = mount.fs.scrub(a.repair).map_err(|err| match err {
                radix_vfs::Error::NotSupported => Kernel::NotSupported.status(0),
                err => fs::status(err),
            })?;
            put_scrub(&counts, UserOut { address, len })?;
            answer(label, 0, [0; 8])
        }
        directory::REPLACE_DISK => {
            let a = directory::ReplaceDiskArgs::decode(args)?;
            let dir = directory_of(found, Rights::CONTROL)?;
            let (address, len) = user_range(a.buffer)?;
            if len < directory::ScrubProgress::SIZE {
                return Err(error(Kernel::InvalidArgument));
            }
            let (_, file) = objects::file(cspace, a.device, Rights::READ.union(Rights::WRITE))?;
            let device = file
                .at
                .node
                .block_device()
                .ok_or_else(|| fs::status(radix_vfs::Error::NotBlockDevice))?;
            let mount = &dir.at.mount;
            if mount.read_only() {
                return Err(fs::status(radix_vfs::Error::ReadOnly));
            }
            let counts = fs::replace_disk(&mount.fs, a.devid, device).map_err(|err| match err {
                radix_vfs::Error::NotSupported => Kernel::NotSupported.status(0),
                err => fs::status(err),
            })?;
            put_scrub(&counts, UserOut { address, len })?;
            answer(label, 0, [0; 8])
        }
        directory::READ_LINK => {
            let a = directory::ReadLinkArgs::decode(args)?;
            let dir = directory_of(found, Rights::READ)?;
            let mut path = Path::Empty;
            let path = path.copy(a.path)?;
            let (address, len) = user_range(a.buffer)?;
            let mut out = UserOut { address, len };
            let len = vfs()?.read_link(&dir.at, path.as_str()?, &mut out).map_err(fs::status)?;
            let results = directory::ReadLinkResults { len: len as u64 };
            answer(label, directory::ReadLinkResults::WORDS, results.encode())
        }
        directory::REAL_PATH => {
            let a = directory::RealPathArgs::decode(args)?;
            let dir = directory_of(found, Rights::READ)?;
            let mut path = Path::Empty;
            let path = path.copy(a.path)?;
            let (address, len) = user_range(a.buffer)?;
            let mut out = UserOut { address, len };
            let len = vfs()?.real_path(&dir.at, path.as_str()?, &mut out).map_err(fs::status)?;
            let results = directory::RealPathResults { len: len as u64 };
            answer(label, directory::RealPathResults::WORDS, results.encode())
        }
        directory::MOUNT => {
            let a = directory::MountArgs::decode(args)?;
            let dir = directory_of(found, Rights::CONTROL)?;
            let read_only = a.flags.contains(MountFlags::READ_ONLY);
            let compress = a.flags.contains(MountFlags::COMPRESS);
            let mut path = Path::Empty;
            let path = path.copy(a.path)?;
            let mut kind = Path::Empty;
            let kind = kind.copy(a.filesystem)?;
            let kind = kind.as_str()?;
            let rw = if read_only { Rights::READ } else { Rights::READ.union(Rights::WRITE) };
            // A bind mount: the directory in slot `source`, mounted again.
            if kind == "bind" {
                if compress {
                    return Err(Kernel::InvalidArgument.status(3));
                }
                let (_, from) = objects::directory(cspace, a.source, rw)?;
                let mount = vfs()?
                    .bind(&dir.at, path.as_str()?, &from.at, read_only)
                    .map_err(fs::status)?;
                info!(
                    "Files: mounted a directory of {} from {} on /{}{}",
                    mount.fs.kind().name(),
                    mount.source(),
                    mount.path(),
                    if mount.read_only() { ", read-only" } else { "" },
                );
                return answer(label, 0, [0; 8]);
            }
            let source = if matches!(kind, "fat" | "exfat" | "btrfs" | "ext4") {
                Some(objects::file(cspace, a.source, rw)?.1)
            } else if a.source != 0 {
                return Err(Kernel::InvalidArgument.status(2));
            } else {
                None
            };
            if compress && kind != "btrfs" {
                return Err(Kernel::InvalidArgument.status(3));
            }
            let flags = fs::Flags { read_only, compress };
            let made =
                fs::make(kind, source.as_ref().map(|s| &s.at.node), flags).map_err(fs::status)?;
            let mount = vfs()?
                .mount(&dir.at, path.as_str()?, made.fs, made.source.as_str(), read_only)
                .map_err(fs::status)?;
            let mut labelled = radix_klog::TextBuf::<40>::new();
            {
                use core::fmt::Write;
                let mut name = radix_klog::TextBuf::<32>::new();
                let _ = mount.fs.label(&mut name);
                if !name.as_str().is_empty() {
                    let _ = write!(labelled, ", labelled {}", name.as_str());
                }
            }
            info!(
                "Files: mounted {} from {} on /{}{}{}",
                mount.fs.kind().name(),
                mount.source(),
                mount.path(),
                if mount.read_only() { ", read-only" } else { "" },
                labelled.as_str()
            );
            answer(label, 0, [0; 8])
        }
        directory::UNMOUNT => {
            let a = directory::UnmountArgs::decode(args)?;
            let dir = directory_of(found, Rights::CONTROL)?;
            let mut path = Path::Empty;
            let path = path.copy(a.path)?;
            vfs()?.unmount(&dir.at, path.as_str()?).map_err(fs::status)?;
            answer(label, 0, [0; 8])
        }
        directory::SYNC => {
            directory::SyncArgs::decode(args)?;
            let dir = directory_of(found, Rights::NONE)?;
            dir.at.mount.fs.sync().map_err(fs::status)?;
            answer(label, 0, [0; 8])
        }
        directory::INFO => {
            directory::InfoArgs::decode(args)?;
            let dir = directory_of(found, Rights::READ)?;
            let mount = &dir.at.mount;
            let info = mount.fs.info();
            let results = directory::InfoResults {
                filesystem: filesystem_kind(mount.fs.kind()),
                flags: if mount.read_only() { MountFlags::READ_ONLY } else { MountFlags::NONE },
                block_size: info.block_size,
                blocks: info.blocks,
                free: info.free,
                files: info.files,
                free_files: info.free_files,
            };
            answer(label, directory::InfoResults::WORDS, results.encode())
        }
        directory::BIND_SOCKET => {
            let a = directory::BindSocketArgs::decode(args)?;
            let dir = directory_of(found, Rights::WRITE)?;
            let (_, file) = objects::file(cspace, a.socket, Rights::WRITE)?;
            let socket = file.socket().ok_or(error(Kernel::NotSocket))?;
            let mut path = Path::Empty;
            let path = path.copy(a.path)?;
            let mut name = Path::Empty;
            let name = name.copy(a.name)?;
            let name = socket::Name::of(name.bytes()).ok_or(Kernel::InvalidArgument.status(2))?;
            socket::bind(socket, vfs()?, &dir.at, path.as_str()?, name)?;
            answer(label, 0, [0; 8])
        }
        directory::CONNECT_SOCKET => {
            let a = directory::ConnectSocketArgs::decode(args)?;
            let dir = directory_of(found, Rights::WRITE)?;
            let (_, file) = objects::file(cspace, a.socket, Rights::WRITE)?;
            let socket = file.socket().ok_or(error(Kernel::NotSocket))?;
            let mut path = Path::Empty;
            let path = path.copy(a.path)?;
            let named = vfs()?.resolve(&dir.at, path.as_str()?).map_err(fs::status)?;
            let non_blocking = file.description().non_blocking;
            socket::connect(socket, &named.node, user.process.number(), non_blocking)?;
            answer(label, 0, [0; 8])
        }
        file::LOCK | file::LOCK_HOLDER => {
            lock_method(user, found, ObjectKind::Directory, label, args)
        }
        _ => Err(error(Kernel::NoSuchMethod)),
    }
}

/// Puts a capability with `rights` to `object`, a file or directory
/// object of `kind`, in `to`, charged to the caller's process; with
/// `close_on_exec`, the slot gets that flag in the same step, so that no
/// `fork` copies it without.
pub fn install_file(
    user: &UserThread,
    to: SlotRef,
    kind: ObjectKind,
    rights: Rights,
    object: FileRef,
    close_on_exec: bool,
) -> Result<()> {
    let charge = Account::charge(user.process.account(), size_of::<FileObject>() as u64)?;
    let mut object = Some(match kind {
        ObjectKind::Directory => Object::Directory(object),
        _ => Object::File(object),
    });
    let made = caps::with(|store| -> Result<()> {
        objects::create_with_rights(store, to, kind, rights, &mut object, Some(charge))?;
        if close_on_exec {
            store.set_slot_flags(to, SlotFlags::CLOSE_ON_EXEC.0 as u8).map_err(cap_error)?;
        }
        Ok(())
    })
    .map_err(cap_error)
    .and_then(|made| made);
    // Not made: it goes now, with the store's lock gone.
    drop(object);
    made
}

/// Writes the directory's entries from `cursor` into the caller's buffer
/// as DirEntry records.
fn list(at: &Location, cursor: u64, address: u64, len: usize) -> Result<directory::ListResults> {
    let mut used = 0usize;
    let mut count = 0u32;
    let mut next = cursor;
    let mut failed = None;
    let mut first_too_big = false;
    vfs()?
        .list(at, cursor, &mut |after, name, kind| {
            let size = (directory::DirEntry::SIZE + name.len()).next_multiple_of(8);
            if used + size > len {
                first_too_big = count == 0;
                return false;
            }
            let mut header = [0u8; directory::DirEntry::SIZE];
            header[..8].copy_from_slice(&after.to_le_bytes());
            header[directory::DirEntry::OFFSET_KIND] = file_kind(kind).raw();
            header[directory::DirEntry::OFFSET_NAME_LEN..directory::DirEntry::OFFSET_NAME_LEN + 2]
                .copy_from_slice(&(name.len() as u16).to_le_bytes());
            let base = address + used as u64;
            let pad = [0u8; 8];
            let tail = size - directory::DirEntry::SIZE - name.len();
            let copied = Arch::copy_to_user(base, &header)
                && Arch::copy_to_user(base + directory::DirEntry::SIZE as u64, name.as_bytes())
                && Arch::copy_to_user(
                    base + (directory::DirEntry::SIZE + name.len()) as u64,
                    &pad[..tail],
                );
            if !copied {
                failed = Some(error(Kernel::BadAddress));
                return false;
            }
            used += size;
            count += 1;
            next = after;
            true
        })
        .map_err(fs::status)?;
    if let Some(err) = failed {
        return Err(err);
    }
    if first_too_big {
        // Not even one entry fits: an empty answer would read as the end.
        return Err(Kernel::InvalidArgument.status(1));
    }
    Ok(directory::ListResults { count, used: used as u64, next })
}

/// The file interface, on the file `found` in a slot, for `user`'s
/// thread.
pub fn file_method(user: &UserRef, found: &Found, label: u32, args: &[u64; 8]) -> Result<Answer> {
    match label {
        file::READ => {
            let a = file::ReadArgs::decode(args)?;
            let file = file_of(found, Rights::READ)?;
            let (address, len) = user_range(a.buffer)?;
            let count = read(user, file, a.offset, address, len, false)?;
            let results = file::ReadResults { count };
            answer(label, file::ReadResults::WORDS, results.encode())
        }
        file::WRITE => {
            let a = file::WriteArgs::decode(args)?;
            let file = file_of(found, Rights::WRITE)?;
            let (address, len) = user_range(a.data)?;
            let count = write(user, file, a.offset, address, len, false)?;
            let results = file::WriteResults { count };
            answer(label, file::WriteResults::WORDS, results.encode())
        }
        file::STAT => {
            let a = file::StatArgs::decode(args)?;
            let file = file_of(found, Rights::NONE)?;
            put_status(&file.at.node.stat(), file.at.mount.id, a.buffer)?;
            answer(label, 0, [0; 8])
        }
        file::SET_TIMES => {
            let a = file::SetTimesArgs::decode(args)?;
            let file = file_of(found, Rights::WRITE)?;
            set_times(&file.at, a.accessed, a.modified)?;
            answer(label, 0, [0; 8])
        }
        file::SET_SIZE => {
            let a = file::SetSizeArgs::decode(args)?;
            let file = file_of(found, Rights::WRITE)?;
            file.at.writable().map_err(fs::status)?;
            file.at.node.set_size(a.size).map_err(fs::status)?;
            answer(label, 0, [0; 8])
        }
        file::SYNC => {
            file::SyncArgs::decode(args)?;
            // As POSIX's fsync, whatever the file was opened for.
            let file = file_of(found, Rights::NONE)?;
            sync(file)?;
            answer(label, 0, [0; 8])
        }
        file::SEEK => {
            let a = file::SeekArgs::decode(args)?;
            let file = file_of(found, Rights::NONE)?;
            let offset = seek(file, a.offset, a.whence)?;
            answer(label, file::SeekResults::WORDS, file::SeekResults { offset }.encode())
        }
        file::STATUS_FLAGS => {
            let a = file::StatusFlagsArgs::decode(args)?;
            let file = file_of(found, Rights::NONE)?;
            let old = file.description();
            if a.set {
                file.set_description(Description {
                    append: a.flags.contains(DescriptionFlags::APPEND),
                    non_blocking: a.flags.contains(DescriptionFlags::NON_BLOCKING),
                });
            }
            let mut flags = DescriptionFlags::NONE;
            if old.append {
                flags = flags.union(DescriptionFlags::APPEND);
            }
            if old.non_blocking {
                flags = flags.union(DescriptionFlags::NON_BLOCKING);
            }
            answer(
                label,
                file::StatusFlagsResults::WORDS,
                file::StatusFlagsResults { flags }.encode(),
            )
        }
        file::CONTROL => {
            let a = file::ControlArgs::decode(args)?;
            let file = file_of(found, Rights::NONE)?;
            let result = match target(file) {
                Target::Tty(handle) => {
                    handle.control(user, a.request, a.argument, a.buffer, a.size)?
                }
                Target::Pipe(_) | Target::Socket(_) if a.request == terminal::READABLE => {
                    let len = match target(file) {
                        Target::Pipe(end) => end.pipe().len(),
                        Target::Socket(socket) => socket.pending(),
                        _ => 0,
                    } as u32;
                    if a.size < 4 || !Arch::copy_to_user(a.buffer, &len.to_le_bytes()) {
                        return Err(error(Kernel::BadAddress));
                    }
                    0
                }
                Target::Kmsg if a.request == radix_abi::kernel_log::CLEAR => {
                    found.cap.check(ObjectKind::File, Rights::WRITE).map_err(cap_error)?;
                    fs::kmsg::clear();
                    0
                }
                Target::Keyboard if a.request == radix_abi::input::SET_LEDS => {
                    found.cap.check(ObjectKind::File, Rights::WRITE).map_err(cap_error)?;
                    fs::keyboard::set_lights(a.argument as u32);
                    0
                }
                Target::Framebuffer if a.request == radix_abi::framebuffer::GET_INFO => {
                    let bytes = fs::framebuffer::info_bytes()?;
                    if a.size < bytes.len() as u64 || !Arch::copy_to_user(a.buffer, &bytes) {
                        return Err(error(Kernel::BadAddress));
                    }
                    0
                }
                Target::Framebuffer => {
                    let may_write = found.cap.check(ObjectKind::File, Rights::WRITE).is_ok();
                    fs::framebuffer::control(file, a.request, may_write)?;
                    0
                }
                Target::Node if matches!(a.request, file::CLONE | file::CLONE_RANGE) => {
                    clone_file(user, found, file, &a)?;
                    0
                }
                Target::Node => {
                    let Some(device) = file.at.node.block_device() else {
                        return Err(error(Kernel::NotTerminal));
                    };
                    let may_write = found.cap.check(ObjectKind::File, Rights::WRITE).is_ok();
                    fs::disk::control(&device, a.request, a.argument, may_write)?;
                    0
                }
                _ => return Err(error(Kernel::NotTerminal)),
            };
            answer(label, file::ControlResults::WORDS, file::ControlResults { result }.encode())
        }
        file::LOCK | file::LOCK_HOLDER => lock_method(user, found, ObjectKind::File, label, args),
        file::SEND => {
            let a = file::SendArgs::decode(args)?;
            let (file, socket) = socket_of(found, Rights::WRITE)?;
            let (address, len) = user_range(a.data)?;
            let data = UserIn { address, len };
            let non_blocking =
                a.flags.contains(SocketFlags::DONT_WAIT) || file.description().non_blocking;
            let signal = !a.flags.contains(SocketFlags::NO_SIGNAL);
            let count = socket.send(&data, non_blocking, signal)? as u64;
            user.process.note_written(count);
            answer(label, file::SendResults::WORDS, file::SendResults { count }.encode())
        }
        file::RECEIVE => {
            let a = file::ReceiveArgs::decode(args)?;
            let (file, socket) = socket_of(found, Rights::READ)?;
            let (address, len) = user_range(a.buffer)?;
            let mut out = UserOut { address, len };
            let non_blocking =
                a.flags.contains(SocketFlags::DONT_WAIT) || file.description().non_blocking;
            let count = socket.receive(&mut out, non_blocking)? as u64;
            user.process.note_read(count);
            answer(label, file::ReceiveResults::WORDS, file::ReceiveResults { count }.encode())
        }
        file::SHUTDOWN => {
            let a = file::ShutdownArgs::decode(args)?;
            let (_, socket) = socket_of(found, Rights::NONE)?;
            socket.connected()?.shutdown(a.read, a.write);
            answer(label, 0, [0; 8])
        }
        file::LISTEN => {
            let a = file::ListenArgs::decode(args)?;
            let (_, socket) = socket_of(found, Rights::WRITE)?;
            socket::listen(socket, a.backlog, user.process.number())?;
            answer(label, 0, [0; 8])
        }
        file::ACCEPT => {
            let a = file::AcceptArgs::decode(args)?;
            let (file, socket) = socket_of(found, Rights::READ)?;
            // The slot first, so that no connection is taken for nothing.
            match caps::get(SlotRef::new(user.cspace, a.to)) {
                Err(CapError::Empty) => {}
                Ok(_) => return Err(error(Kernel::Occupied)),
                Err(err) => return Err(cap_error(err)),
            }
            let non_blocking = file.description().non_blocking;
            socket::accept(user, socket, a.to, a.flags, non_blocking)?;
            answer(label, 0, [0; 8])
        }
        file::SOCKET_NAME => {
            let a = file::SocketNameArgs::decode(args)?;
            let (_, socket) = socket_of(found, Rights::NONE)?;
            let name = socket.name(a.peer)?;
            let (address, len) = user_range(a.buffer)?;
            let bytes = name.bytes();
            let fits = &bytes[..bytes.len().min(len)];
            if !fits.is_empty() && !Arch::copy_to_user(address, fits) {
                return Err(error(Kernel::BadAddress));
            }
            let results = file::SocketNameResults { len: bytes.len() as u64 };
            answer(label, file::SocketNameResults::WORDS, results.encode())
        }
        file::SOCKET_STATE => {
            file::SocketStateArgs::decode(args)?;
            let (_, socket) = socket_of(found, Rights::NONE)?;
            let results = file::SocketStateResults {
                listening: socket.listens(),
                connected: socket.end().is_some(),
                peer: socket.end().map_or(0, socket::End::peer_process),
            };
            answer(label, file::SocketStateResults::WORDS, results.encode())
        }
        file::SEND_FILES => {
            let a = file::SendFilesArgs::decode(args)?;
            let (file, socket) = socket_of(found, Rights::WRITE)?;
            let end = socket.connected()?;
            let (address, len) = user_range(a.data)?;
            if len == 0 {
                return Err(Kernel::InvalidArgument.status(0));
            }
            let files = files_to_send(user, a.slots)?;
            let data = UserIn { address, len };
            let non_blocking =
                a.flags.contains(SocketFlags::DONT_WAIT) || file.description().non_blocking;
            let signal = !a.flags.contains(SocketFlags::NO_SIGNAL);
            let count = end.send(&data, files, non_blocking, signal)? as u64;
            user.process.note_written(count);
            answer(label, file::SendFilesResults::WORDS, file::SendFilesResults { count }.encode())
        }
        file::RECEIVE_FILES => {
            let a = file::ReceiveFilesArgs::decode(args)?;
            let (file, socket) = socket_of(found, Rights::READ)?;
            let end = socket.connected()?;
            let (address, len) = user_range(a.buffer)?;
            if !a.slots.len.is_multiple_of(4)
                || !radix_ipc::range::user_bytes(a.slots.address, a.slots.len)
            {
                return Err(error(Kernel::BadAddress));
            }
            let mut out = UserOut { address, len };
            let non_blocking =
                a.flags.contains(SocketFlags::DONT_WAIT) || file.description().non_blocking;
            let (count, files) = end.receive(&mut out, non_blocking)?;
            let close_on_exec = a.flags.contains(SocketFlags::CLOSE_ON_EXEC);
            let (files, lost) = match files {
                Some(files) => install_received(user, files, a.slots, close_on_exec)?,
                None => (0, 0),
            };
            user.process.note_read(count as u64);
            let results = file::ReceiveFilesResults { count: count as u64, files, lost };
            answer(label, file::ReceiveFilesResults::WORDS, results.encode())
        }
        _ => Err(error(Kernel::NoSuchMethod)),
    }
}

/// The file `found` designates and the socket it is of, if its
/// capability has the rights `needs`; not_socket for any other file.
fn socket_of(found: &Found, needs: Rights) -> Result<(&FileRef, &socket::Socket)> {
    let file = file_of(found, Rights::NONE)?;
    let socket = file.socket().ok_or(error(Kernel::NotSocket))?;
    file_of(found, needs)?;
    Ok((file, socket))
}

/// The files `send_files` sends: those whose slots the caller's `slots`
/// holds, u32 each, at most SEND_FILES_MAX; none if it holds none. Each
/// must be a file or directory capability with the copy right, and no
/// socket's.
fn files_to_send(user: &UserRef, slots: UserBuffer) -> Result<Option<pipe::Files>> {
    let count = slots.len / 4;
    if !slots.len.is_multiple_of(4) || count > file::SEND_FILES_MAX {
        return Err(Kernel::InvalidArgument.status(1));
    }
    if count == 0 {
        return Ok(None);
    }
    let mut files = alloc::vec::Vec::new_in(HEAP.with(Flags::MAY_SLEEP));
    files.try_reserve_exact(count as usize).map_err(|_| error(Kernel::NoMemory))?;
    for i in 0..count {
        let mut number = [0; 4];
        if !Arch::copy_from_user(&mut number, slots.address + i * 4) {
            return Err(error(Kernel::BadAddress));
        }
        let is_file = |kind| matches!(kind, ObjectKind::File | ObjectKind::Directory);
        let mut place = None;
        let found = objects::find(user.cspace, u32::from_le_bytes(number), is_file, &mut place)?;
        let kind = found.cap.kind();
        if !is_file(kind) {
            return Err(error(Kernel::WrongKind));
        }
        found.cap.check(kind, Rights::COPY).map_err(cap_error)?;
        let Some(Object::File(file) | Object::Directory(file)) = found.object.take() else {
            return Err(error(Kernel::Gone));
        };
        // A socket sent could hold itself open (`fs::socket`).
        if file.socket().is_some() {
            return Err(error(Kernel::NotSupported));
        }
        // Room was reserved above.
        let _ = files.push_within_capacity(pipe::Passed { kind, rights: found.cap.rights(), file });
    }
    Ok(Some(files))
}

/// Puts each of `files`, received through a socket, in the caller's
/// lowest free slot from SLOT_FD_BASE below its limit of files, closed by
/// exec if `close_on_exec`, and writes the slots' numbers into its
/// `slots`, u32 each, while there is room there. Returns how many it put,
/// and how many it closed for want of room in `slots` or of a free slot.
fn install_received(
    user: &UserRef,
    files: pipe::Files,
    slots: UserBuffer,
    close_on_exec: bool,
) -> Result<(u32, u32)> {
    let room = slots.len / 4;
    let limit = user.process.posix.lock().limits.get(LimitKind::Files).current;
    let end = (u64::from(SLOT_FD_BASE) + limit).min(u64::from(MAX_SLOTS)) as u32;
    let (mut put, mut lost) = (0u32, 0u32);
    let mut from = SLOT_FD_BASE;
    for passed in files {
        if u64::from(put) == room {
            lost += 1;
            continue;
        }
        let installed = loop {
            let Some(slot) = free_slot(user, from, end) else { break None };
            let to = SlotRef::new(user.cspace, slot);
            match install_file(
                user,
                to,
                passed.kind,
                passed.rights,
                passed.file.clone(),
                close_on_exec,
            ) {
                Ok(()) => break Some(slot),
                // Another thread took it first.
                Err(status) if status == error(Kernel::Occupied) => from = slot + 1,
                Err(_) => break None,
            }
        };
        let Some(slot) = installed else {
            lost += 1;
            continue;
        };
        if !Arch::copy_to_user(slots.address + u64::from(put) * 4, &slot.to_le_bytes()) {
            return Err(error(Kernel::BadAddress));
        }
        put += 1;
        from = slot + 1;
    }
    Ok((put, lost))
}

/// The lowest free slot of the caller's space from `from` below `end`,
/// growing the space when every slot it has is taken, as the C library
/// does for a new descriptor (radix-posix(7)).
fn free_slot(user: &UserRef, mut from: u32, end: u32) -> Option<u32> {
    while from < end {
        let (used, slots) = caps::with(|store| -> core::result::Result<_, CapError> {
            Ok((store.used_slots(user.cspace, from)?, store.slots(user.cspace)?))
        })
        .and_then(|found| found)
        .ok()?;
        if from < slots {
            // Only the slots the space has count as free.
            let mut free = !used;
            if slots - from < 64 {
                free &= (1 << (slots - from)) - 1;
            }
            if free != 0 {
                let slot = from + free.trailing_zeros();
                return (slot < end).then_some(slot);
            }
            from += 64;
            if from < slots {
                continue;
            }
        }
        if slots >= MAX_SLOTS {
            return None;
        }
        user.process.grow((slots * 2).min(MAX_SLOTS)).ok()?;
    }
    None
}

/// radix-file(2)'s `lock` and `lock_holder`, on the file or directory
/// (of kind `kind`) `found` designates: `flock` locks directories too.
fn lock_method(
    user: &UserRef,
    found: &Found,
    kind: ObjectKind,
    label: u32,
    args: &[u64; 8],
) -> Result<Answer> {
    let (owner, lock_kind, start, length, wait) = if label == file::LOCK {
        let a = file::LockArgs::decode(args)?;
        (a.owner, a.kind, a.start, a.length, a.wait)
    } else {
        let a = file::LockHolderArgs::decode(args)?;
        (a.owner, a.kind, a.start, a.length, false)
    };
    let space = match owner {
        LockOwner::Description => locks::Space::Whole,
        LockOwner::Process | LockOwner::DescriptionRange => locks::Space::Range,
    };
    let wanted = match lock_kind {
        LockKind::None => None,
        LockKind::Shared => Some(locks::Kind::Shared),
        LockKind::Exclusive => Some(locks::Kind::Exclusive),
    };
    // A range lock needs the right to do what it guards, as POSIX's need
    // the descriptor open for reading or writing.
    let needs = match (space, wanted) {
        (locks::Space::Range, Some(locks::Kind::Shared)) if label == file::LOCK => Rights::READ,
        (locks::Space::Range, Some(locks::Kind::Exclusive)) if label == file::LOCK => Rights::WRITE,
        _ => Rights::NONE,
    };
    let file = match kind {
        ObjectKind::File => file_of(found, needs)?,
        _ => directory_of(found, needs)?,
    };
    let (start, end) = match space {
        locks::Space::Whole => (0, u64::MAX),
        locks::Space::Range if length == 0 => (start, u64::MAX),
        locks::Space::Range => {
            (start, start.checked_add(length - 1).ok_or(Kernel::InvalidArgument.status(3))?)
        }
    };
    let process = &user.process;
    let lock = locks::Lock {
        owner: match owner {
            LockOwner::Process => locks::Owner::Process(process.number()),
            _ => locks::Owner::Description(file.address()),
        },
        space,
        kind: wanted.unwrap_or(locks::Kind::Shared),
        start,
        end,
    };
    if label == file::LOCK {
        if wanted.is_some() {
            match lock.owner {
                locks::Owner::Process(_) => process.note_record_locks(),
                locks::Owner::Description(_) => file.note_locked(),
            }
        }
        locks::lock(&file.at.node, lock, wanted, wait, process.number())?;
        return answer(label, 0, [0; 8]);
    }
    let results = match wanted.and_then(|_| locks::holder(&file.at.node, &lock)) {
        None => file::LockHolderResults { kind: LockKind::None, start: 0, length: 0, process: 0 },
        Some(held) => file::LockHolderResults {
            kind: match held.kind {
                locks::Kind::Shared => LockKind::Shared,
                locks::Kind::Exclusive => LockKind::Exclusive,
            },
            start: held.start,
            length: if held.end == u64::MAX { 0 } else { held.end - held.start + 1 },
            process: match held.owner {
                locks::Owner::Process(number) => number,
                locks::Owner::Description(_) => u64::MAX,
            },
        },
    };
    answer(label, file::LockHolderResults::WORDS, results.encode())
}

/// What a file's reads and writes go to.
enum Target<'a> {
    /// An end of a pipe.
    Pipe(&'a pipe::End),
    /// A socket.
    Socket(&'a socket::Socket),
    /// A side of a terminal.
    Tty(&'a tty::Handle),
    /// `/devices/kmsg`.
    Kmsg,
    /// `/devices/kbd`.
    Keyboard,
    /// `/devices/mouse`.
    Mouse,
    /// `/devices/fb0`.
    Framebuffer,
    /// A file of the tree, through its node.
    Node,
}

fn target(file: &FileObject) -> Target<'_> {
    if let Some(end) = file.pipe_end() {
        Target::Pipe(end)
    } else if let Some(socket) = file.socket() {
        Target::Socket(socket)
    } else if let Some(handle) = file.tty_handle() {
        Target::Tty(handle)
    } else if file.is_kmsg() {
        Target::Kmsg
    } else if file.is_keyboard() {
        Target::Keyboard
    } else if file.is_mouse() {
        Target::Mouse
    } else if file.is_framebuffer() {
        Target::Framebuffer
    } else {
        Target::Node
    }
}

/// Reads from `file` into the caller's `len` bytes at `address`, checked
/// already: from `offset`, or at CURRENT from the description's offset,
/// which moves past what was read. A pipe or a terminal takes any offset,
/// and waits for something to read unless the description is non-blocking
/// or `never_wait`.
#[inline]
pub fn read(
    user: &UserThread,
    file: &FileObject,
    offset: u64,
    address: u64,
    len: usize,
    never_wait: bool,
) -> Result<u64> {
    let mut out = UserOut { address, len };
    if file.is_special() {
        let read = read_special(user, file, &mut out, never_wait)?;
        user.process.note_read(read);
        return Ok(read);
    }
    let read = if offset == file::CURRENT {
        let mut at = file.offset.lock();
        let read = file.at.node.read(*at, &mut out);
        if let Ok(n) = read {
            *at += n as u64;
        }
        read
    } else {
        file.at.node.read(offset, &mut out)
    };
    let read = read.map_err(fs::status)? as u64;
    user.process.note_read(read);
    Ok(read)
}

/// [`read`] from a pipe or a terminal: kept out of line, so that reads of
/// files, which are most of them, skip its frame.
#[inline(never)]
fn read_special(
    user: &UserThread,
    file: &FileObject,
    out: &mut UserOut,
    never_wait: bool,
) -> Result<u64> {
    let non_blocking = never_wait || file.description().non_blocking;
    match target(file) {
        Target::Pipe(end) => end.pipe().read(out, non_blocking).map(|n| n as u64),
        Target::Socket(socket) => socket.receive(out, non_blocking).map(|n| n as u64),
        Target::Tty(handle) => handle.read(user, out, non_blocking).map(|n| n as u64),
        Target::Kmsg => fs::kmsg::read(file, out, non_blocking).map(|n| n as u64),
        Target::Keyboard => fs::keyboard::read(file, out, non_blocking).map(|n| n as u64),
        Target::Mouse => fs::mouse::read(file, out, non_blocking).map(|n| n as u64),
        Target::Framebuffer | Target::Node => Ok(0),
    }
}

/// Writes the caller's `len` bytes at `address` to `file`: at `offset`,
/// or at CURRENT at the description's offset (the end of the file if it
/// appends), which moves past what was written. A pipe or a terminal
/// takes any offset, and waits for room as [`read`] waits.
#[inline]
pub fn write(
    user: &UserThread,
    file: &FileObject,
    offset: u64,
    address: u64,
    len: usize,
    never_wait: bool,
) -> Result<u64> {
    let data = UserIn { address, len };
    if file.is_special() {
        let written = write_special(user, file, offset, &data, never_wait)?;
        user.process.note_written(written);
        return Ok(written);
    }
    file.at.writable().map_err(fs::status)?;
    let node = &file.at.node;
    let written = if offset != file::CURRENT {
        node.write(offset, &data).map_err(fs::status)?
    } else {
        let mut at = file.offset.lock();
        if file.description().append {
            let written = node.write(file::APPEND, &data).map_err(fs::status)?;
            *at = node.stat().size;
            written
        } else {
            let written = node.write(*at, &data).map_err(fs::status)?;
            *at += written as u64;
            written
        }
    } as u64;
    user.process.note_written(written);
    Ok(written)
}

/// [`write`] to a pipe or a terminal, kept out of line as [`read_special`]
/// is.
#[inline(never)]
fn write_special(
    user: &UserThread,
    file: &FileObject,
    offset: u64,
    data: &UserIn,
    never_wait: bool,
) -> Result<u64> {
    let non_blocking = never_wait || file.description().non_blocking;
    match target(file) {
        Target::Pipe(end) => end.pipe().write(data, non_blocking, true).map(|n| n as u64),
        Target::Socket(socket) => socket.send(data, non_blocking, true).map(|n| n as u64),
        Target::Tty(handle) => handle.write(user, data, non_blocking).map(|n| n as u64),
        Target::Kmsg => fs::kmsg::write(data).map(|n| n as u64),
        Target::Framebuffer if offset == file::CURRENT => {
            let mut at = file.offset.lock();
            let written = fs::framebuffer::write(file, *at, data)? as u64;
            *at += written;
            Ok(written)
        }
        Target::Framebuffer => fs::framebuffer::write(file, offset, data).map(|n| n as u64),
        Target::Keyboard | Target::Mouse => Err(error(Kernel::InvalidArgument)),
        Target::Node => Ok(0),
    }
}

/// `seek`: moves `file`'s offset `offset` bytes from where `whence` says.
fn seek(file: &FileObject, offset: i64, whence: Whence) -> Result<u64> {
    match target(file) {
        Target::Node | Target::Framebuffer => {}
        Target::Kmsg => return fs::kmsg::seek(file, offset, whence),
        _ => return Err(error(Kernel::NotSeekable)),
    }
    let mut at = file.offset.lock();
    let base = match whence {
        Whence::Start => 0,
        Whence::Current => *at,
        Whence::End => file.at.node.stat().size,
    };
    let new = i128::from(base) + i128::from(offset);
    if !(0..=i128::from(i64::MAX)).contains(&new) {
        return Err(Kernel::InvalidArgument.status(0));
    }
    *at = new as u64;
    Ok(*at)
}

/// Where `path` leads from `at`, following a symbolic link that is its
/// last name unless `flags` says no_follow.
/// What the node `at` opened stands for, when it is a filesystem's named
/// pipe (an end of its pipe, opened with `rights`), device node (the
/// device, which the kernel must have) or socket's name (which no open
/// takes: no_device, ENXIO); none for any other node.
fn stands_for(at: &Location, rights: Rights, non_blocking: bool) -> Result<Option<NodeRef>> {
    let kind = at.node.kind();
    if !matches!(kind, Kind::Pipe | Kind::CharDevice | Kind::BlockDevice | Kind::Socket)
        || matches!(at.mount.fs.kind(), FsKind::Dev | FsKind::Pipe)
        || pipe::end_of(&at.node).is_some()
        || socket::socket_of(&at.node).is_some()
    {
        return Ok(None);
    }
    let no_device = || radix_abi::errors::File::NoDevice.status(0);
    match kind {
        Kind::Pipe => {
            let (read, write) = (rights.contains(Rights::READ), rights.contains(Rights::WRITE));
            pipe::open_named(at, read, write, non_blocking).map(Some)
        }
        Kind::Socket => Err(no_device()),
        _ => {
            let device = fs::devfs::by_number(kind, at.node.stat().device).ok_or_else(no_device)?;
            Ok(Some(device.open().map_err(fs::status)?.unwrap_or(device)))
        }
    }
}

/// A filesystem error as a status, with what a filesystem does not do
/// (FAT's hard links or device nodes) refused as Linux does it: EPERM.
fn refused_as_not_permitted(err: radix_vfs::Error) -> radix_abi::Status {
    match err {
        radix_vfs::Error::NotSupported => error(Kernel::NotPermitted),
        err => fs::status(err),
    }
}

/// Writes `stat`, of a file on the mount numbered `mount`, to the
/// caller's `buffer` as a FileStatus.
fn put_status(stat: &radix_vfs::Stat, mount: u64, buffer: UserBuffer) -> Result<()> {
    use directory::FileStatus as S;
    let (address, len) = user_range(buffer)?;
    if len < S::SIZE {
        return Err(error(Kernel::InvalidArgument));
    }
    let mut bytes = [0u8; S::SIZE];
    for (at, value) in [
        (S::OFFSET_SIZE, stat.size),
        (S::OFFSET_ALLOCATED, stat.allocated),
        (S::OFFSET_ID, stat.id),
        (S::OFFSET_MOUNT, mount),
        (S::OFFSET_CREATED, stat.created),
        (S::OFFSET_MODIFIED, stat.modified),
        (S::OFFSET_ACCESSED, stat.accessed),
        (S::OFFSET_CHANGED, stat.changed),
        (S::OFFSET_DEVICE, stat.device),
    ] {
        bytes[at..at + 8].copy_from_slice(&value.to_le_bytes());
    }
    bytes[S::OFFSET_LINKS..S::OFFSET_LINKS + 4].copy_from_slice(&stat.links.to_le_bytes());
    bytes[S::OFFSET_KIND] = file_kind(stat.kind).raw();
    UserOut { address, len }.put(0, &bytes).map_err(fs::status)
}

/// Writes what a scrub read and found to `out`, as a ScrubProgress.
fn put_scrub(counts: &radix_vfs::Scrub, mut out: UserOut) -> Result<()> {
    use directory::ScrubProgress as P;
    let mut bytes = [0u8; P::SIZE];
    for (at, value) in [
        (P::OFFSET_DATA_EXTENTS, counts.data_extents),
        (P::OFFSET_TREE_EXTENTS, counts.tree_extents),
        (P::OFFSET_DATA_BYTES, counts.data_bytes),
        (P::OFFSET_TREE_BYTES, counts.tree_bytes),
        (P::OFFSET_READ_ERRORS, counts.read_errors),
        (P::OFFSET_CSUM_ERRORS, counts.csum_errors),
        (P::OFFSET_VERIFY_ERRORS, counts.verify_errors),
        (P::OFFSET_NO_CSUM, counts.no_csum),
        (P::OFFSET_CSUM_DISCARDS, counts.csum_discards),
        (P::OFFSET_SUPER_ERRORS, counts.super_errors),
        (P::OFFSET_MALLOC_ERRORS, counts.malloc_errors),
        (P::OFFSET_UNCORRECTABLE_ERRORS, counts.uncorrectable_errors),
        (P::OFFSET_CORRECTED_ERRORS, counts.corrected_errors),
        (P::OFFSET_LAST_PHYSICAL, counts.last_physical),
        (P::OFFSET_UNVERIFIED_ERRORS, counts.unverified_errors),
    ] {
        bytes[at..at + 8].copy_from_slice(&value.to_le_bytes());
    }
    bytes[P::OFFSET_FSID..P::OFFSET_FSID + 16].copy_from_slice(&counts.fsid);
    out.put(0, &bytes).map_err(fs::status)
}

fn resolve(at: &Location, path: &str, flags: PathFlags) -> Result<Location> {
    let vfs = vfs()?;
    if flags.contains(PathFlags::NO_FOLLOW) {
        vfs.resolve_link(at, path)
    } else {
        vfs.resolve(at, path)
    }
    .map_err(fs::status)
}

/// `set_times`: sets the times of the node at `at`, TIME_NOW meaning now
/// and TIME_OMIT leaving one as it is. A node that keeps no times
/// refuses with not_permitted, as Linux does a file whose times a
/// process may not set.
fn set_times(at: &Location, accessed: u64, modified: u64) -> Result<()> {
    let now = radix_vfs::now();
    let time = |t: u64| match t {
        file::TIME_OMIT => None,
        file::TIME_NOW => Some(now),
        t => Some(t),
    };
    let (accessed, modified) = (time(accessed), time(modified));
    if accessed.is_none() && modified.is_none() {
        return Ok(());
    }
    at.writable().map_err(fs::status)?;
    at.node.set_times(accessed, modified).map_err(|err| match err {
        radix_vfs::Error::NotSupported => error(Kernel::NotPermitted),
        err => fs::status(err),
    })
}

/// Makes `file`'s changes durable, as its node's filesystem does: FAT,
/// exFAT and ext4 write all their changes, Btrfs a regular file's alone,
/// with its log of fsyncs (ADR 0154).
pub fn sync(file: &FileRef) -> Result<()> {
    file.at.node.sync().map_err(fs::status)
}

/// The ring's read operation: `args` are the offset, address and length.
/// A pipe or a terminal never waits here.
pub fn ring_read(user: &UserThread, cap: u32, args: [u64; 3]) -> Result<[u64; 2]> {
    let (_, file) = objects::file(user.cspace, cap, Rights::READ)?;
    let (address, len) = user_range(UserBuffer { address: args[1], len: args[2] })?;
    Ok([read(user, &file, args[0], address, len, true)?, 0])
}

/// The ring's write operation.
pub fn ring_write(user: &UserThread, cap: u32, args: [u64; 3]) -> Result<[u64; 2]> {
    let (_, file) = objects::file(user.cspace, cap, Rights::WRITE)?;
    let (address, len) = user_range(UserBuffer { address: args[1], len: args[2] })?;
    Ok([write(user, &file, args[0], address, len, true)?, 0])
}

/// The ring's sync operation.
pub fn ring_sync(user: &UserThread, cap: u32) -> Result<[u64; 2]> {
    let (_, file) = objects::file(user.cspace, cap, Rights::NONE)?;
    sync(&file)?;
    Ok([0; 2])
}
