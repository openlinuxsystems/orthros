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
//! writes at CURRENT use its offset, and those of a pipe's end or a
//! terminal's side go to the pipe or the terminal (`fs::pipe`, `fs::tty`),
//! which may wait. Opening a terminal makes it the controlling terminal of
//! a session leader that has none, unless asked not to.

use core::mem::size_of;

use radix_abi::capability::SlotFlags;
use radix_abi::directory::{self, FileKind, FilesystemKind, MountFlags, OpenFlags};
use radix_abi::errors::Kernel;
use radix_abi::file::{DescriptionFlags, LockKind, LockOwner, Whence};
use radix_abi::{UserBuffer, file, terminal};
use radix_cap::{CapError, ObjectKind, Rights, SlotRef};
use radix_hal::UserMode;
use radix_klog::info;
use radix_mm::{Flags, HEAP};
use radix_vfs::mount::{Location, OpenFlags as VfsOpen};
use radix_vfs::{FsKind, In, Kind, Out, PATH_MAX};

use super::account::Account;
use super::memory::cap_rights;
use super::methods::{Answer, answer};
use super::objects::{self, Found, Object};
use super::{Result, UserRef, UserThread, cap_error, error};
use crate::fs::{self, Description, FileObject, FileRef, locks, pipe, tty};
use crate::{Arch, caps};

/// The most bytes one read or write moves, as on Linux.
pub const IO_MAX: u64 = 0x7fff_f000;

/// A path copied in from a program: on the stack when it is short, on the
/// heap when it is long.
enum Path {
    Short([u8; 128], usize),
    Long(alloc::vec::Vec<u8, radix_mm::Kmalloc>),
}

impl Path {
    /// Copies the path in `buffer` from the caller. Inlined, so that the
    /// path is copied in where the caller keeps it, not moved there after.
    #[inline(always)]
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
        Kind::Pipe => FileKind::Pipe,
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
            let path = Path::copy(a.path)?;
            let flags = VfsOpen {
                create: a.flags.contains(OpenFlags::CREATE),
                exclusive: a.flags.contains(OpenFlags::EXCLUSIVE),
                truncate: a.flags.contains(OpenFlags::TRUNCATE),
                directory: a.flags.contains(OpenFlags::DIRECTORY),
            };
            let at = vfs()?.open(&dir.at, path.as_str()?, flags).map_err(fs::status)?;
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
            if object_kind == ObjectKind::File && wanted.contains(Rights::WRITE) {
                at.writable().map_err(fs::status)?;
            }
            let rights = wanted.union(Rights::COPY).union(Rights::TRANSFER);
            let object = FileObject::with(at, description).ok_or(error(Kernel::NoMemory))?;
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
            let path = Path::copy(a.path)?;
            vfs()?.make_directory(&dir.at, path.as_str()?).map_err(fs::status)?;
            answer(label, 0, [0; 8])
        }
        directory::REMOVE => {
            let a = directory::RemoveArgs::decode(args)?;
            let dir = directory_of(found, Rights::WRITE)?;
            let path = Path::copy(a.path)?;
            vfs()?.remove(&dir.at, path.as_str()?).map_err(fs::status)?;
            answer(label, 0, [0; 8])
        }
        directory::RENAME => {
            let a = directory::RenameArgs::decode(args)?;
            let dir = directory_of(found, Rights::WRITE)?;
            let (_, to_dir) = objects::directory(cspace, a.to_dir, Rights::WRITE)?;
            let (from, to) = (Path::copy(a.from)?, Path::copy(a.to)?);
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
            let path = Path::copy(a.path)?;
            let at = vfs()?.resolve(&dir.at, path.as_str()?).map_err(fs::status)?;
            let stat = at.node.stat();
            let results = directory::StatResults {
                kind: file_kind(stat.kind),
                size: stat.size,
                allocated: stat.allocated,
                id: stat.id,
                created: stat.created,
                modified: stat.modified,
                accessed: stat.accessed,
                mount: at.mount.id,
            };
            answer(label, directory::StatResults::WORDS, results.encode())
        }
        directory::SET_TIMES => {
            let a = directory::SetTimesArgs::decode(args)?;
            let dir = directory_of(found, Rights::WRITE)?;
            let path = Path::copy(a.path)?;
            let at = vfs()?.resolve(&dir.at, path.as_str()?).map_err(fs::status)?;
            set_times(&at, a.accessed, a.modified)?;
            answer(label, 0, [0; 8])
        }
        directory::MOUNT => {
            let a = directory::MountArgs::decode(args)?;
            let dir = directory_of(found, Rights::CONTROL)?;
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
            let dir = directory_of(found, Rights::CONTROL)?;
            let path = Path::copy(a.path)?;
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
            file::StatArgs::decode(args)?;
            let file = file_of(found, Rights::NONE)?;
            let stat = file.at.node.stat();
            let results = file::StatResults {
                kind: file_kind(stat.kind),
                size: stat.size,
                allocated: stat.allocated,
                id: stat.id,
                created: stat.created,
                modified: stat.modified,
                accessed: stat.accessed,
                mount: file.at.mount.id,
            };
            answer(label, file::StatResults::WORDS, results.encode())
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
            let file = file_of(found, Rights::WRITE)?;
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
                Target::Pipe(end) if a.request == terminal::READABLE => {
                    let len = end.pipe().len() as u32;
                    if a.size < 4 || !Arch::copy_to_user(a.buffer, &len.to_le_bytes()) {
                        return Err(error(Kernel::BadAddress));
                    }
                    0
                }
                _ => return Err(error(Kernel::NotTerminal)),
            };
            answer(label, file::ControlResults::WORDS, file::ControlResults { result }.encode())
        }
        file::LOCK | file::LOCK_HOLDER => lock_method(user, found, ObjectKind::File, label, args),
        _ => Err(error(Kernel::NoSuchMethod)),
    }
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
    /// A side of a terminal.
    Tty(&'a tty::Handle),
    /// A file of the tree, through its node.
    Node,
}

fn target(file: &FileObject) -> Target<'_> {
    if let Some(end) = file.pipe_end() {
        Target::Pipe(end)
    } else if let Some(handle) = file.tty_handle() {
        Target::Tty(handle)
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
    if !matches!(target(file), Target::Node) {
        return read_special(user, file, &mut out, never_wait);
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
    read.map(|n| n as u64).map_err(fs::status)
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
        Target::Tty(handle) => handle.read(user, out, non_blocking).map(|n| n as u64),
        Target::Node => Ok(0),
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
    if !matches!(target(file), Target::Node) {
        return write_special(user, file, &data, never_wait);
    }
    file.at.writable().map_err(fs::status)?;
    let node = &file.at.node;
    if offset != file::CURRENT {
        return node.write(offset, &data).map(|n| n as u64).map_err(fs::status);
    }
    let mut at = file.offset.lock();
    let written = if file.description().append {
        let written = node.write(file::APPEND, &data).map_err(fs::status)?;
        *at = node.stat().size;
        written
    } else {
        let written = node.write(*at, &data).map_err(fs::status)?;
        *at += written as u64;
        written
    };
    Ok(written as u64)
}

/// [`write`] to a pipe or a terminal, kept out of line as [`read_special`]
/// is.
#[inline(never)]
fn write_special(
    user: &UserThread,
    file: &FileObject,
    data: &UserIn,
    never_wait: bool,
) -> Result<u64> {
    let non_blocking = never_wait || file.description().non_blocking;
    match target(file) {
        Target::Pipe(end) => end.pipe().write(data, non_blocking).map(|n| n as u64),
        Target::Tty(handle) => handle.write(user, data, non_blocking).map(|n| n as u64),
        Target::Node => Ok(0),
    }
}

/// `seek`: moves `file`'s offset `offset` bytes from where `whence` says.
fn seek(file: &FileObject, offset: i64, whence: Whence) -> Result<u64> {
    if !matches!(target(file), Target::Node) {
        return Err(error(Kernel::NotSeekable));
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

/// Writes `file`'s filesystem's changes to its disk.
pub fn sync(file: &FileRef) -> Result<()> {
    file.at.node.sync().map_err(fs::status)?;
    file.at.mount.fs.sync().map_err(fs::status)
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
    let (_, file) = objects::file(user.cspace, cap, Rights::WRITE)?;
    sync(&file)?;
    Ok([0; 2])
}
