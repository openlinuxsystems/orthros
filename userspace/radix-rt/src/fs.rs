// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Files and directories, through the capabilities that reach them
//! (radix-directory(2), radix-file(2)).
//!
//! A [`Dir`] is a capability to a directory; every path is resolved
//! beneath it, never above it, and never from `/`. A program that has a
//! root directory finds it in `SLOT_ROOT` ([`Dir::root`]). Opening gives
//! a new capability in a slot of the program's ([`slots`]), which goes
//! back when the [`Dir`] or [`File`] is dropped.
//!
//! Reads and writes go through the kernel's page cache; [`File::sync`]
//! and [`Dir::sync`] wait until the disk has the changes.

use alloc::string::String;
use alloc::vec::Vec;
use core::fmt;

use libradix::SYS;
use radix_abi::Status;
use radix_abi::capability::{self, ObjectKind, Rights};
use radix_abi::directory::{
    self, DirEntry, FileKind, FileStatus, FilesystemKind, LinkFlags, MountFlags, OpenFlags,
    PathFlags, ScrubProgress,
};
use radix_abi::errors::{File as FileError, Kernel};
use radix_abi::{file, startup};

use crate::slots;

/// Why a call on a file or directory failed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
    /// The kernel said no: the status it returned.
    Status(Status),
    /// The program has no free capability slot left.
    NoSlot,
    /// The capability is not to what it was opened as.
    WrongKind,
}

impl Error {
    /// The file error, if it is one.
    pub fn file(self) -> Option<FileError> {
        match self {
            Error::Status(status) => FileError::from_status(status),
            _ => None,
        }
    }

    /// Whether it is file error `error`.
    pub fn is(self, error: FileError) -> bool {
        self.file() == Some(error)
    }
}

impl From<Status> for Error {
    fn from(status: Status) -> Error {
        Error::Status(status)
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            Error::Status(status) => {
                match (FileError::from_status(status), Kernel::from_status(status)) {
                    (Some(error), _) => f.write_str(error.name()),
                    (None, Some(error)) => f.write_str(error.name()),
                    (None, None) => write!(f, "status {:#x}", status.0),
                }
            }
            Error::NoSlot => f.write_str("no free capability slot"),
            Error::WrongKind => f.write_str("not a file or directory capability"),
        }
    }
}

/// What a call returns.
pub type Result<T> = core::result::Result<T, Error>;

/// What `stat` says of a file.
#[derive(Clone, Copy, Debug)]
pub struct Stat {
    /// What it is.
    pub kind: FileKind,
    /// Its size in bytes.
    pub size: u64,
    /// The bytes it takes on its filesystem.
    pub allocated: u64,
    /// A number unique on its filesystem while it exists.
    pub id: u64,
    /// When it was made, in nanoseconds since 1970 (0 if unknown).
    pub created: u64,
    /// When it last changed, in nanoseconds since 1970 (0 if unknown).
    pub modified: u64,
    /// When it was last read: when it was made, unless a program set it
    /// (0 if unknown).
    pub accessed: u64,
    /// The number of the mount it is on, unique since boot.
    pub mount: u64,
    /// When it last changed in any way, its names, times or size too
    /// (0 if unknown).
    pub changed: u64,
    /// How many names it has.
    pub links: u32,
    /// The device a device node stands for (FileStatus's `device`).
    pub device: u64,
    /// Its permission bits (ADR 0213).
    pub mode: u16,
    /// The number of the user it belongs to.
    pub owner: u32,
    /// The number of its group.
    pub group: u32,
}

/// The modes [`Dir`] asks for what it makes, less the program's file mode
/// mask, as Linux programs ask: read and write for all for a file, and
/// search too for a directory.
const FILE_MODE: u32 = 0o666;
const DIR_MODE: u32 = 0o777;

impl From<&[u8; FileStatus::SIZE]> for Stat {
    /// What a FileStatus `stat` wrote says.
    fn from(bytes: &[u8; FileStatus::SIZE]) -> Stat {
        let word = |at: usize| u64::from_le_bytes(bytes[at..at + 8].try_into().unwrap_or([0; 8]));
        let half = |at: usize| bytes[at..at + 4].try_into().map_or(0, u32::from_le_bytes);
        let mode = bytes[FileStatus::OFFSET_MODE..FileStatus::OFFSET_MODE + 2]
            .try_into()
            .map_or(0, u16::from_le_bytes);
        Stat {
            kind: FileKind::from_raw(bytes[FileStatus::OFFSET_KIND]).unwrap_or(FileKind::File),
            size: word(FileStatus::OFFSET_SIZE),
            allocated: word(FileStatus::OFFSET_ALLOCATED),
            id: word(FileStatus::OFFSET_ID),
            created: word(FileStatus::OFFSET_CREATED),
            modified: word(FileStatus::OFFSET_MODIFIED),
            accessed: word(FileStatus::OFFSET_ACCESSED),
            mount: word(FileStatus::OFFSET_MOUNT),
            changed: word(FileStatus::OFFSET_CHANGED),
            links: half(FileStatus::OFFSET_LINKS),
            device: word(FileStatus::OFFSET_DEVICE),
            mode,
            owner: half(FileStatus::OFFSET_OWNER),
            group: half(FileStatus::OFFSET_GROUP),
        }
    }
}

/// What [`Dir::scrub`] read and found.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Scrubbed {
    /// The bytes it read, every copy.
    pub bytes: u64,
    /// The sectors it failed to read or found bad, and the bad copies of
    /// the superblock.
    pub errors: u64,
    /// The bad sectors with a good copy.
    pub corrected: u64,
    /// The bad sectors with none.
    pub uncorrectable: u64,
}

impl Scrubbed {
    /// What the ScrubProgress `progress` counts.
    fn of(progress: &[u8; ScrubProgress::SIZE]) -> Scrubbed {
        let word =
            |at: usize| u64::from_le_bytes(progress[at..at + 8].try_into().unwrap_or([0; 8]));
        Scrubbed {
            bytes: word(ScrubProgress::OFFSET_DATA_BYTES) + word(ScrubProgress::OFFSET_TREE_BYTES),
            errors: [
                ScrubProgress::OFFSET_READ_ERRORS,
                ScrubProgress::OFFSET_CSUM_ERRORS,
                ScrubProgress::OFFSET_VERIFY_ERRORS,
                ScrubProgress::OFFSET_SUPER_ERRORS,
            ]
            .into_iter()
            .map(word)
            .sum(),
            corrected: word(ScrubProgress::OFFSET_CORRECTED_ERRORS),
            uncorrectable: word(ScrubProgress::OFFSET_UNCORRECTABLE_ERRORS),
        }
    }
}

/// What `info` says of a directory's filesystem.
#[derive(Clone, Copy, Debug)]
pub struct Info {
    /// Its kind.
    pub kind: FilesystemKind,
    /// How it is mounted.
    pub flags: MountFlags,
    /// Its block size in bytes.
    pub block_size: u32,
    /// Its blocks.
    pub blocks: u64,
    /// Its free blocks.
    pub free: u64,
    /// How many files and directories it can hold, or zero if it keeps no
    /// count.
    pub files: u64,
    /// How many more it can take.
    pub free_files: u64,
}

/// An entry of a directory.
#[derive(Clone, Debug)]
pub struct Entry {
    /// Its name.
    pub name: String,
    /// What it is.
    pub kind: FileKind,
}

/// A slot that holds a capability of the program's, deleted and given
/// back when dropped unless it is one of the fixed start-up slots.
struct Held {
    slot: u32,
    owned: bool,
}

impl Held {
    /// A free slot, for a capability a call is to put there.
    fn take() -> Result<Held> {
        slots::take().map(|slot| Held { slot, owned: true }).ok_or(Error::NoSlot)
    }
}

impl Drop for Held {
    fn drop(&mut self) {
        if self.owned {
            let _ = capability::delete(&SYS, self.slot);
            slots::give_back(self.slot);
        }
    }
}

/// A directory.
pub struct Dir {
    held: Held,
}

/// A file.
pub struct File {
    held: Held,
}

/// Whether the capability in `slot` is to an object of `kind`.
fn holds(slot: u32, kind: ObjectKind) -> bool {
    capability::info(&SYS, slot).is_ok_and(|info| info.kind == kind)
}

impl Dir {
    /// The program's root directory, in `SLOT_ROOT`, if it was given one.
    pub fn root() -> Option<Dir> {
        Dir::at(startup::SLOT_ROOT).ok()
    }

    /// The directory whose capability is in `slot`, which stays the
    /// program's: dropping the [`Dir`] leaves it.
    pub fn at(slot: u32) -> Result<Dir> {
        if !holds(slot, ObjectKind::Directory) {
            return Err(Error::WrongKind);
        }
        Ok(Dir { held: Held { slot, owned: false } })
    }

    /// The slot of its capability, to share it with a program.
    pub fn slot(&self) -> u32 {
        self.held.slot
    }

    fn open_raw(&self, path: &str, flags: OpenFlags, rights: Rights) -> Result<(Held, FileKind)> {
        let held = Held::take()?;
        let slot = self.held.slot;
        let opened =
            directory::open(&SYS, slot, path.as_bytes(), flags, rights, held.slot, FILE_MODE)?;
        Ok((held, opened.kind))
    }

    /// Opens the directory at `path` with `rights` (of read, write and
    /// control).
    pub fn open_dir(&self, path: &str, rights: Rights) -> Result<Dir> {
        let (held, _) = self.open_raw(path, OpenFlags::DIRECTORY, rights)?;
        Ok(Dir { held })
    }

    /// Opens the file at `path` as `flags` say, with `rights` (of read
    /// and write).
    pub fn open(&self, path: &str, flags: OpenFlags, rights: Rights) -> Result<File> {
        let (held, kind) = self.open_raw(path, flags, rights)?;
        if kind == FileKind::Directory {
            return Err(Error::Status(FileError::IsDirectory.status(0)));
        }
        Ok(File { held })
    }

    /// Opens the file at `path` to read it.
    pub fn open_read(&self, path: &str) -> Result<File> {
        self.open(path, OpenFlags::NONE, Rights::READ)
    }

    /// Makes the file at `path`, or cuts it to 0 bytes, to read and write.
    pub fn create(&self, path: &str) -> Result<File> {
        let rw = Rights::READ.union(Rights::WRITE);
        self.open(path, OpenFlags::CREATE.union(OpenFlags::TRUNCATE), rw)
    }

    /// The whole of the file at `path`.
    pub fn read_file(&self, path: &str) -> Result<Vec<u8>> {
        self.open_read(path)?.read_to_end()
    }

    /// Makes the file at `path` hold `data`, and nothing else.
    pub fn write_file(&self, path: &str, data: &[u8]) -> Result<()> {
        self.create(path)?.write_all_at(0, data)
    }

    /// Makes an empty directory at `path`.
    pub fn make_dir(&self, path: &str) -> Result<()> {
        Ok(directory::make_directory(&SYS, self.held.slot, path.as_bytes(), DIR_MODE)?)
    }

    /// Changes the owner, group or permission bits of the file or
    /// directory at `path` (`.` for this one), each that is `None` staying
    /// (ADR 0213). Without a mode, it is a chown(2), which takes a file's
    /// set-user-ID bit away.
    pub fn set_access(
        &self,
        path: &str,
        owner: Option<u32>,
        group: Option<u32>,
        mode: Option<u16>,
    ) -> Result<()> {
        let keep = |value: Option<u32>| value.unwrap_or(directory::KEEP);
        let (owner, group, mode) = (keep(owner), keep(group), keep(mode.map(u32::from)));
        let (slot, path) = (self.held.slot, path.as_bytes());
        Ok(directory::set_access(&SYS, slot, path, PathFlags::NONE, owner, group, mode)?)
    }

    /// Removes the file or empty directory at `path`.
    pub fn remove(&self, path: &str) -> Result<()> {
        Ok(directory::remove(&SYS, self.held.slot, path.as_bytes())?)
    }

    /// Moves the file or directory at `from` to `to` beneath `to_dir`.
    pub fn rename(&self, from: &str, to_dir: &Dir, to: &str) -> Result<()> {
        let slot = self.held.slot;
        Ok(directory::rename(&SYS, slot, from.as_bytes(), to_dir.held.slot, to.as_bytes())?)
    }

    /// Makes a symbolic link at `path` that points to `target`.
    pub fn make_link(&self, path: &str, target: &str) -> Result<()> {
        Ok(directory::make_link(&SYS, self.held.slot, path.as_bytes(), target.as_bytes())?)
    }

    /// Makes `to` beneath `to_dir` another name of the file at `path`, a
    /// hard link: of a symbolic link itself, unless `follow`.
    pub fn link(&self, path: &str, follow: bool, to_dir: &Dir, to: &str) -> Result<()> {
        let flags = if follow { LinkFlags::FOLLOW } else { LinkFlags::NONE };
        let (slot, to_slot) = (self.held.slot, to_dir.held.slot);
        Ok(directory::link(&SYS, slot, path.as_bytes(), flags, to_slot, to.as_bytes())?)
    }

    /// Makes a file of `kind` at `path`: a named pipe, a device node that
    /// stands for `device` (its major number in the high 32 bits), an
    /// empty file or a socket's name.
    pub fn make_node(&self, path: &str, kind: FileKind, device: u64) -> Result<()> {
        let (slot, path) = (self.held.slot, path.as_bytes());
        Ok(directory::make_node(&SYS, slot, path, kind, device, FILE_MODE)?)
    }

    /// Makes a new, empty Btrfs subvolume at `path`.
    pub fn make_subvolume(&self, path: &str) -> Result<()> {
        Ok(directory::make_subvolume(&SYS, self.held.slot, path.as_bytes())?)
    }

    /// Makes `to` beneath `to_dir` a snapshot of the Btrfs subvolume that
    /// holds `path`, read-only if `read_only`.
    pub fn snapshot(&self, path: &str, to_dir: &Dir, to: &str, read_only: bool) -> Result<()> {
        let (slot, to_slot) = (self.held.slot, to_dir.held.slot);
        Ok(directory::snapshot(&SYS, slot, path.as_bytes(), to_slot, to.as_bytes(), read_only)?)
    }

    /// Removes the Btrfs subvolume at `path`, with everything in it.
    pub fn remove_subvolume(&self, path: &str) -> Result<()> {
        Ok(directory::remove_subvolume(&SYS, self.held.slot, path.as_bytes())?)
    }

    /// Reads every block of the directory's Btrfs filesystem, every copy,
    /// and checks it, writing a good copy over a bad one if `repair`
    /// (ADR 0150).
    pub fn scrub(&self, repair: bool) -> Result<Scrubbed> {
        let mut progress = [0u8; ScrubProgress::SIZE];
        directory::scrub(&SYS, self.held.slot, repair, &mut progress)?;
        Ok(Scrubbed::of(&progress))
    }

    /// Puts the block device `device` in the place of the disk of device
    /// ID `devid` of the directory's Btrfs filesystem, and rebuilds onto
    /// it (ADR 0155).
    pub fn replace_disk(&self, devid: u64, device: &File) -> Result<Scrubbed> {
        let mut progress = [0u8; ScrubProgress::SIZE];
        let (slot, device) = (self.held.slot, device.held.slot);
        directory::replace_disk(&SYS, slot, devid, device, &mut progress)?;
        Ok(Scrubbed::of(&progress))
    }

    /// Adds the block device `device` to the disks of the directory's
    /// Btrfs filesystem (ADR 0159); returns the device ID it takes.
    pub fn add_disk(&self, device: &File) -> Result<u64> {
        Ok(directory::add_disk(&SYS, self.held.slot, device.held.slot)?.devid)
    }

    /// Moves every chunk off the disk of device ID `devid` of the
    /// directory's Btrfs filesystem, then takes it out (ADR 0159).
    pub fn remove_disk(&self, devid: u64) -> Result<()> {
        Ok(directory::remove_disk(&SYS, self.held.slot, devid)?)
    }

    /// Moves every chunk of the directory's Btrfs filesystem to a new one
    /// (ADR 0159), converting each to the profile `convert` names, a Btrfs
    /// block group profile flag, unless it is 0; returns how many chunks
    /// it looked at and how many it moved.
    pub fn balance(&self, convert: u64) -> Result<(u64, u64)> {
        let kinds = directory::BalanceKinds::DATA
            .union(directory::BalanceKinds::METADATA)
            .union(directory::BalanceKinds::SYSTEM);
        let done = directory::balance(&SYS, self.held.slot, kinds, convert, convert, convert)?;
        Ok((done.considered, done.moved))
    }

    /// Turns the simple quotas of the directory's Btrfs filesystem on or
    /// off (ADR 0170).
    pub fn quota(&self, enable: bool) -> Result<()> {
        Ok(directory::quota(&SYS, self.held.slot, enable)?)
    }

    /// The quota groups of the directory's Btrfs filesystem (ADR 0170),
    /// by ID: each one's ID and the bytes it counts.
    pub fn qgroups(&self) -> Result<Vec<(u64, u64)>> {
        use directory::Qgroup as Q;
        let mut buffer = alloc::vec![0u8; 64 * Q::SIZE];
        let count = directory::qgroups(&SYS, self.held.slot, &mut buffer)?.count;
        let field = |record: &[u8], at: usize| {
            u64::from_le_bytes(record[at..at + 8].try_into().unwrap_or([0; 8]))
        };
        Ok(buffer
            .as_chunks::<{ Q::SIZE }>()
            .0
            .iter()
            .take(usize::try_from(count).unwrap_or(usize::MAX))
            .map(|r| (field(r, Q::OFFSET_ID), field(r, Q::OFFSET_REFERENCED)))
            .collect())
    }

    /// Where the symbolic link at `path` points.
    pub fn read_link(&self, path: &str) -> Result<Vec<u8>> {
        let mut target = alloc::vec![0u8; 256];
        loop {
            let read = directory::read_link(&SYS, self.held.slot, path.as_bytes(), &mut target)?;
            let len = usize::try_from(read.len).unwrap_or(usize::MAX);
            if len <= target.len() {
                target.truncate(len);
                return Ok(target);
            }
            target.resize(len, 0);
        }
    }

    /// What the file at `path` is (`.` for this directory), following a
    /// symbolic link there.
    pub fn stat(&self, path: &str) -> Result<Stat> {
        let mut status = [0u8; FileStatus::SIZE];
        directory::stat(&SYS, self.held.slot, path.as_bytes(), PathFlags::NONE, &mut status)?;
        Ok(Stat::from(&status))
    }

    /// The directory's entries, but `.` and `..`, in the order its
    /// filesystem keeps them.
    pub fn entries(&self) -> Result<Vec<Entry>> {
        let mut entries = Vec::new();
        let mut buffer = alloc::vec![0u8; 4096];
        let mut cursor = 0;
        loop {
            let listed = directory::list(&SYS, self.held.slot, cursor, &mut buffer)?;
            if listed.count == 0 {
                return Ok(entries);
            }
            let mut at = 0;
            for _ in 0..listed.count {
                let header = &buffer[at..at + DirEntry::SIZE];
                let word = |offset: usize, len: usize| {
                    header[offset..offset + len]
                        .iter()
                        .rev()
                        .fold(0u64, |v, &b| v << 8 | u64::from(b))
                };
                let len = word(DirEntry::OFFSET_NAME_LEN, 2) as usize;
                let name = &buffer[at + DirEntry::SIZE..at + DirEntry::SIZE + len];
                let kind =
                    FileKind::from_raw(header[DirEntry::OFFSET_KIND]).unwrap_or(FileKind::File);
                entries.push(Entry { name: String::from_utf8_lossy(name).into_owned(), kind });
                at += (DirEntry::SIZE + len).next_multiple_of(8);
            }
            cursor = listed.next;
        }
    }

    /// Mounts a filesystem of `kind` on the directory at `path`: from the
    /// block device `source` for FAT, from nothing for the others.
    pub fn mount(
        &self,
        path: &str,
        kind: FilesystemKind,
        source: Option<&File>,
        flags: MountFlags,
    ) -> Result<()> {
        let source = source.map_or(0, |file| file.held.slot);
        let name = kind.name().as_bytes();
        Ok(directory::mount(&SYS, self.held.slot, path.as_bytes(), name, source, flags)?)
    }

    /// Mounts the directory `from` on the directory at `path` too, as
    /// Linux's bind mounts do: read-only if `flags` says so, or if
    /// `from`'s mount is.
    pub fn bind(&self, path: &str, from: &Dir, flags: MountFlags) -> Result<()> {
        let (slot, path) = (self.held.slot, path.as_bytes());
        Ok(directory::mount(&SYS, slot, path, b"bind", from.held.slot, flags)?)
    }

    /// Unmounts the filesystem whose root is at `path`.
    pub fn unmount(&self, path: &str) -> Result<()> {
        Ok(directory::unmount(&SYS, self.held.slot, path.as_bytes())?)
    }

    /// A copy of the capability with only `rights` of those it has.
    pub fn derive(&self, rights: Rights) -> Result<Dir> {
        let held = Held::take()?;
        capability::derive(&SYS, self.held.slot, held.slot, rights, 0)?;
        Ok(Dir { held })
    }

    /// Makes an empty view acting for this capability's name (ADR 0215),
    /// to fill with [`Dir::attach`] and [`Dir::attach_link`]: its root.
    pub fn make_view(&self) -> Result<Dir> {
        let held = Held::take()?;
        directory::make_view(&SYS, self.held.slot, held.slot)?;
        Ok(Dir { held })
    }

    /// Attaches the directory or file the capability in `what` holds at
    /// `path` in this view: read-only unless it has the write right.
    pub fn attach(&self, path: &str, what: u32) -> Result<()> {
        Ok(directory::attach(&SYS, self.held.slot, path.as_bytes(), what)?)
    }

    /// Makes a symbolic link at `path` in this view, to `target`.
    pub fn attach_link(&self, path: &str, target: &str) -> Result<()> {
        let (slot, path) = (self.held.slot, path.as_bytes());
        Ok(directory::attach_link(&SYS, slot, path, target.as_bytes())?)
    }

    /// Writes what the directory's filesystem holds in memory to its disk.
    pub fn sync(&self) -> Result<()> {
        Ok(directory::sync(&SYS, self.held.slot)?)
    }

    /// The directory's filesystem.
    pub fn info(&self) -> Result<Info> {
        let i = directory::info(&SYS, self.held.slot)?;
        Ok(Info {
            kind: i.filesystem,
            flags: i.flags,
            block_size: i.block_size,
            blocks: i.blocks,
            free: i.free,
            files: i.files,
            free_files: i.free_files,
        })
    }
}

impl File {
    /// The file whose capability is in `slot`, which stays the program's.
    pub fn at(slot: u32) -> Result<File> {
        if !holds(slot, ObjectKind::File) {
            return Err(Error::WrongKind);
        }
        Ok(File { held: Held { slot, owned: false } })
    }

    /// The file whose capability a call put in `slot`, a slot from
    /// [`slots::take`] that the [`File`] takes over: dropping it deletes the
    /// capability and gives the slot back. A slot of another kind is given
    /// back empty.
    pub fn own(slot: u32) -> Result<File> {
        let held = Held { slot, owned: true };
        if !holds(slot, ObjectKind::File) {
            return Err(Error::WrongKind);
        }
        Ok(File { held })
    }

    /// The slot of its capability, to share it with a program.
    pub fn slot(&self) -> u32 {
        self.held.slot
    }

    /// Reads from byte `offset` into `buffer`; returns how many bytes it
    /// read, fewer than asked at the end of the file.
    pub fn read_at(&self, offset: u64, buffer: &mut [u8]) -> Result<usize> {
        Ok(file::read(&SYS, self.held.slot, offset, buffer)?.count as usize)
    }

    /// Writes `data` at byte `offset` ([`file::APPEND`]: at the end);
    /// returns how many bytes it wrote.
    pub fn write_at(&self, offset: u64, data: &[u8]) -> Result<usize> {
        Ok(file::write(&SYS, self.held.slot, offset, data)?.count as usize)
    }

    /// Writes all of `data` at byte `offset`.
    pub fn write_all_at(&self, mut offset: u64, mut data: &[u8]) -> Result<()> {
        while !data.is_empty() {
            let wrote = self.write_at(offset, data)?;
            if wrote == 0 {
                return Err(Error::Status(FileError::NoSpace.status(0)));
            }
            offset += wrote as u64;
            data = &data[wrote..];
        }
        Ok(())
    }

    /// The whole file, from its start.
    pub fn read_to_end(&self) -> Result<Vec<u8>> {
        let size = self.stat()?.size;
        let mut data = alloc::vec![0u8; size.min(1 << 24) as usize];
        let mut at = 0;
        while at < data.len() {
            let read = self.read_at(at as u64, &mut data[at..])?;
            if read == 0 {
                data.truncate(at);
                return Ok(data);
            }
            at += read;
        }
        // It may have grown since, or not know its size (a generated file
        // of /processes says 0): read on in pieces until the end.
        let mut piece = [0u8; 512];
        loop {
            let read = self.read_at(at as u64, &mut piece)?;
            if read == 0 {
                return Ok(data);
            }
            data.extend_from_slice(&piece[..read]);
            at += read;
        }
    }

    /// What the file is.
    pub fn stat(&self) -> Result<Stat> {
        let mut status = [0u8; FileStatus::SIZE];
        file::stat(&SYS, self.held.slot, &mut status)?;
        Ok(Stat::from(&status))
    }

    /// Makes the file `size` bytes long.
    pub fn set_size(&self, size: u64) -> Result<()> {
        Ok(file::set_size(&SYS, self.held.slot, size)?)
    }

    /// Makes the file share all of `source`'s data, which takes no room
    /// until one of the two changes it (Linux's FICLONE, ADR 0139).
    pub fn clone_from(&self, source: &File) -> Result<()> {
        file::control(&SYS, self.held.slot, file::CLONE, source.held.slot.into(), 0, 0)?;
        Ok(())
    }

    /// Waits until the disk has the changes of the file's filesystem.
    pub fn sync(&self) -> Result<()> {
        Ok(file::sync(&SYS, self.held.slot)?)
    }
}
