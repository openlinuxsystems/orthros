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

use core::mem::size_of;

use radix_abi::directory::{self, FileKind, FilesystemKind, MountFlags, OpenFlags};
use radix_abi::errors::Kernel;
use radix_abi::{UserBuffer, file};
use radix_cap::{CapError, ObjectKind, Rights, SlotRef};
use radix_hal::UserMode;
use radix_klog::info;
use radix_mm::{Flags, HEAP};
use radix_vfs::mount::{Location, OpenFlags as VfsOpen};
use radix_vfs::{FsKind, In, Kind, Out, PATH_MAX};

use super::account::Account;
use super::memory::cap_rights;
use super::methods::{Answer, answer};
use super::objects::{self, Object};
use super::{Result, UserRef, cap_error, error, process};
use crate::Arch;
use crate::fs::{self, FileObject, FileRef};

/// The most bytes one read or write moves, as on Linux.
pub const IO_MAX: u64 = 0x7fff_f000;

/// A path copied in from a program: on the stack when it is short, on the
/// heap when it is long.
enum Path {
    Short([u8; 128], usize),
    Long(alloc::vec::Vec<u8, radix_mm::Kmalloc>),
}

impl Path {
    /// Copies the path in `buffer` from the caller.
    fn copy(buffer: UserBuffer) -> Result<Path> {
        if buffer.len > PATH_MAX as u64 {
            return Err(fs::status(radix_vfs::Error::NameTooLong));
        }
        if !radix_ipc::range::user_bytes(buffer.address, buffer.len) {
            return Err(error(Kernel::BadAddress));
        }
        let len = buffer.len as usize;
        let mut path = if len <= 128 {
            Path::Short([0; 128], len)
        } else {
            let mut bytes = alloc::vec::Vec::new_in(HEAP.with(Flags::MAY_SLEEP));
            bytes.try_reserve_exact(len).map_err(|_| error(Kernel::NoMemory))?;
            while bytes.len() < len {
                // Room was reserved above.
                let _ = bytes.push_within_capacity(0);
            }
            Path::Long(bytes)
        };
        let bytes = match &mut path {
            Path::Short(bytes, len) => &mut bytes[..*len],
            Path::Long(bytes) => &mut bytes[..],
        };
        if !Arch::copy_from_user(bytes, buffer.address) {
            return Err(error(Kernel::BadAddress));
        }
        Ok(path)
    }

    /// The path, checked (radix_vfs::path::parse).
    fn as_str(&self) -> Result<&str> {
        let bytes = match self {
            Path::Short(bytes, len) => &bytes[..*len],
            Path::Long(bytes) => &bytes[..],
        };
        radix_vfs::path::parse(bytes).map_err(fs::status)
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
    }
}

fn filesystem_kind(kind: FsKind) -> FilesystemKind {
    match kind {
        FsKind::Tmpfs => FilesystemKind::Tmpfs,
        FsKind::Fat => FilesystemKind::Fat,
        FsKind::Proc => FilesystemKind::Proc,
        FsKind::Sys => FilesystemKind::Sys,
        FsKind::Dev => FilesystemKind::Dev,
        FsKind::Boot => FilesystemKind::Bootfs,
    }
}

/// The rights a directory capability's `open` hands on, besides copy and
/// transfer.
const OPEN_RIGHTS: Rights = Rights::READ.union(Rights::WRITE).union(Rights::CONTROL);

/// The directory interface.
pub fn directory_method(user: &UserRef, slot: u32, label: u32, args: &[u64; 8]) -> Result<Answer> {
    let cspace = user.cspace;
    match label {
        directory::OPEN => {
            let a = directory::OpenArgs::decode(args)?;
            let asked = cap_rights(a.rights).intersection(OPEN_RIGHTS);
            let changes =
                a.flags.contains(OpenFlags::CREATE) || a.flags.contains(OpenFlags::TRUNCATE);
            let mut needs =
                Rights::READ.union(asked.intersection(Rights::READ.union(Rights::WRITE)));
            if changes {
                needs = needs.union(Rights::WRITE);
            }
            let (cap, dir) = objects::directory(cspace, slot, needs)?;
            let to = SlotRef::new(cspace, a.to);
            // The slot first, so that a file is not made for nothing.
            match crate::caps::get(to) {
                Err(CapError::Empty) => {}
                Ok(_) => return Err(error(Kernel::Occupied)),
                Err(err) => return Err(cap_error(err)),
            }
            let path = Path::copy(a.path)?;
            let flags = VfsOpen {
                create: a.flags.contains(OpenFlags::CREATE),
                exclusive: a.flags.contains(OpenFlags::EXCLUSIVE),
                truncate: a.flags.contains(OpenFlags::TRUNCATE),
                directory: a.flags.contains(OpenFlags::DIRECTORY),
            };
            let at = vfs()?.open(&dir.at, path.as_str()?, flags).map_err(fs::status)?;
            drop(dir);
            let kind = at.node.stat().kind;
            let object_kind =
                if kind == Kind::Directory { ObjectKind::Directory } else { ObjectKind::File };
            let wanted = asked.intersection(object_kind.rights());
            let missing = wanted.difference(cap.rights());
            if !missing.is_empty() {
                return Err(cap_error(CapError::MissingRights(missing)));
            }
            if object_kind == ObjectKind::File && wanted.contains(Rights::WRITE) {
                at.writable().map_err(fs::status)?;
            }
            let rights = wanted.union(Rights::COPY).union(Rights::TRANSFER);
            install(user, to, object_kind, rights, at)?;
            let results = directory::OpenResults { kind: file_kind(kind) };
            answer(label, directory::OpenResults::WORDS, results.encode())
        }
        directory::MAKE_DIRECTORY => {
            let a = directory::MakeDirectoryArgs::decode(args)?;
            let (_, dir) = objects::directory(cspace, slot, Rights::WRITE)?;
            let path = Path::copy(a.path)?;
            vfs()?.make_directory(&dir.at, path.as_str()?).map_err(fs::status)?;
            answer(label, 0, [0; 8])
        }
        directory::REMOVE => {
            let a = directory::RemoveArgs::decode(args)?;
            let (_, dir) = objects::directory(cspace, slot, Rights::WRITE)?;
            let path = Path::copy(a.path)?;
            vfs()?.remove(&dir.at, path.as_str()?).map_err(fs::status)?;
            answer(label, 0, [0; 8])
        }
        directory::RENAME => {
            let a = directory::RenameArgs::decode(args)?;
            let (_, dir) = objects::directory(cspace, slot, Rights::WRITE)?;
            let (_, to_dir) = objects::directory(cspace, a.to_dir, Rights::WRITE)?;
            let (from, to) = (Path::copy(a.from)?, Path::copy(a.to)?);
            vfs()?.rename(&dir.at, from.as_str()?, &to_dir.at, to.as_str()?).map_err(fs::status)?;
            answer(label, 0, [0; 8])
        }
        directory::LIST => {
            let a = directory::ListArgs::decode(args)?;
            let (_, dir) = objects::directory(cspace, slot, Rights::READ)?;
            let (address, len) = user_range(a.buffer)?;
            let results = list(&dir.at, a.cursor, address, len)?;
            answer(label, directory::ListResults::WORDS, results.encode())
        }
        directory::STAT => {
            let a = directory::StatArgs::decode(args)?;
            let (_, dir) = objects::directory(cspace, slot, Rights::READ)?;
            let path = Path::copy(a.path)?;
            let stat = vfs()?.stat(&dir.at, path.as_str()?).map_err(fs::status)?;
            let results = directory::StatResults {
                kind: file_kind(stat.kind),
                size: stat.size,
                allocated: stat.allocated,
                id: stat.id,
                created: stat.created,
                modified: stat.modified,
            };
            answer(label, directory::StatResults::WORDS, results.encode())
        }
        directory::MOUNT => {
            let a = directory::MountArgs::decode(args)?;
            let (_, dir) = objects::directory(cspace, slot, Rights::CONTROL)?;
            let read_only = a.flags.contains(MountFlags::READ_ONLY);
            let path = Path::copy(a.path)?;
            let kind = Path::copy(a.filesystem)?;
            let kind = kind.as_str()?;
            let source = if kind == "fat" {
                let needs =
                    if read_only { Rights::READ } else { Rights::READ.union(Rights::WRITE) };
                Some(objects::file(cspace, a.source, needs)?.1)
            } else if a.source != 0 {
                return Err(Kernel::InvalidArgument.status(2));
            } else {
                None
            };
            let made = fs::make(kind, source.as_ref().map(|s| &s.at.node), read_only)
                .map_err(fs::status)?;
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
            let (_, dir) = objects::directory(cspace, slot, Rights::CONTROL)?;
            let path = Path::copy(a.path)?;
            vfs()?.unmount(&dir.at, path.as_str()?).map_err(fs::status)?;
            answer(label, 0, [0; 8])
        }
        directory::SYNC => {
            directory::SyncArgs::decode(args)?;
            let (_, dir) = objects::directory(cspace, slot, Rights::NONE)?;
            dir.at.mount.fs.sync().map_err(fs::status)?;
            answer(label, 0, [0; 8])
        }
        directory::INFO => {
            directory::InfoArgs::decode(args)?;
            let (_, dir) = objects::directory(cspace, slot, Rights::READ)?;
            let mount = &dir.at.mount;
            let info = mount.fs.info();
            let results = directory::InfoResults {
                filesystem: filesystem_kind(mount.fs.kind()),
                flags: if mount.read_only() { MountFlags::READ_ONLY } else { MountFlags::NONE },
                block_size: info.block_size,
                blocks: info.blocks,
                free: info.free,
            };
            answer(label, directory::InfoResults::WORDS, results.encode())
        }
        _ => Err(error(Kernel::NoSuchMethod)),
    }
}

/// Makes the object for `at` and puts a capability to it with `rights` in
/// `to`, charged to the caller's process.
fn install(
    user: &UserRef,
    to: SlotRef,
    kind: ObjectKind,
    rights: Rights,
    at: Location,
) -> Result<()> {
    let charge = Account::charge(user.process.account(), size_of::<FileObject>() as u64)?;
    let object = FileObject::new(at).ok_or(error(Kernel::NoMemory))?;
    let object = match kind {
        ObjectKind::Directory => Object::Directory(object),
        _ => Object::File(object),
    };
    process::install_with_rights(to, kind, rights, object, Some(charge))
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

/// The file interface.
pub fn file_method(user: &UserRef, slot: u32, label: u32, args: &[u64; 8]) -> Result<Answer> {
    let cspace = user.cspace;
    match label {
        file::READ => {
            let a = file::ReadArgs::decode(args)?;
            let (_, file) = objects::file(cspace, slot, Rights::READ)?;
            let (address, len) = user_range(a.buffer)?;
            let count = read(&file, a.offset, address, len)?;
            let results = file::ReadResults { count };
            answer(label, file::ReadResults::WORDS, results.encode())
        }
        file::WRITE => {
            let a = file::WriteArgs::decode(args)?;
            let (_, file) = objects::file(cspace, slot, Rights::WRITE)?;
            let (address, len) = user_range(a.data)?;
            let count = write(&file, a.offset, address, len)?;
            let results = file::WriteResults { count };
            answer(label, file::WriteResults::WORDS, results.encode())
        }
        file::STAT => {
            file::StatArgs::decode(args)?;
            let (_, file) = objects::file(cspace, slot, Rights::NONE)?;
            let stat = file.at.node.stat();
            let results = file::StatResults {
                kind: file_kind(stat.kind),
                size: stat.size,
                allocated: stat.allocated,
                id: stat.id,
                created: stat.created,
                modified: stat.modified,
            };
            answer(label, file::StatResults::WORDS, results.encode())
        }
        file::SET_SIZE => {
            let a = file::SetSizeArgs::decode(args)?;
            let (_, file) = objects::file(cspace, slot, Rights::WRITE)?;
            file.at.writable().map_err(fs::status)?;
            file.at.node.set_size(a.size).map_err(fs::status)?;
            answer(label, 0, [0; 8])
        }
        file::SYNC => {
            file::SyncArgs::decode(args)?;
            let (_, file) = objects::file(cspace, slot, Rights::WRITE)?;
            sync(&file)?;
            answer(label, 0, [0; 8])
        }
        _ => Err(error(Kernel::NoSuchMethod)),
    }
}

/// Reads from `offset` of `file` into the caller's `len` bytes at
/// `address`, checked already.
pub fn read(file: &FileRef, offset: u64, address: u64, len: usize) -> Result<u64> {
    let mut out = UserOut { address, len };
    file.at.node.read(offset, &mut out).map(|n| n as u64).map_err(fs::status)
}

/// Writes the caller's `len` bytes at `address` to `offset` of `file`.
pub fn write(file: &FileRef, offset: u64, address: u64, len: usize) -> Result<u64> {
    file.at.writable().map_err(fs::status)?;
    let data = UserIn { address, len };
    file.at.node.write(offset, &data).map(|n| n as u64).map_err(fs::status)
}

/// Writes `file`'s filesystem's changes to its disk.
pub fn sync(file: &FileRef) -> Result<()> {
    file.at.node.sync().map_err(fs::status)?;
    file.at.mount.fs.sync().map_err(fs::status)
}

/// The ring's read operation: `args` are the offset, address and length.
pub fn ring_read(user: &super::UserThread, cap: u32, args: [u64; 3]) -> Result<[u64; 2]> {
    let (_, file) = objects::file(user.cspace, cap, Rights::READ)?;
    let (address, len) = user_range(UserBuffer { address: args[1], len: args[2] })?;
    Ok([read(&file, args[0], address, len)?, 0])
}

/// The ring's write operation.
pub fn ring_write(user: &super::UserThread, cap: u32, args: [u64; 3]) -> Result<[u64; 2]> {
    let (_, file) = objects::file(user.cspace, cap, Rights::WRITE)?;
    let (address, len) = user_range(UserBuffer { address: args[1], len: args[2] })?;
    Ok([write(&file, args[0], address, len)?, 0])
}

/// The ring's sync operation.
pub fn ring_sync(user: &super::UserThread, cap: u32) -> Result<[u64; 2]> {
    let (_, file) = objects::file(user.cspace, cap, Rights::WRITE)?;
    sync(&file)?;
    Ok([0; 2])
}
