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
//! advisory locks do (Phase 8).
//!
//! The crate is a compartment: it has no `unsafe` code. It builds for the
//! host (feature `host`), where its tests run.

#![cfg_attr(not(any(test, kani, feature = "host")), no_std)]
#![forbid(unsafe_code)]
#![cfg_attr(not(kani), feature(allocator_ext))]
// Kani's own, older nightly has the allocator API unstable still.
#![cfg_attr(kani, feature(allocator_api))]
#![feature(vec_push_within_capacity)]

extern crate alloc;

pub mod bootfs;
pub mod cache;
pub mod locks;
pub mod mount;
pub mod path;
pub mod synthetic;
pub mod tmpfs;

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
    /// The file is in use: a mount point, or a filesystem with files open.
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
    /// Out of memory.
    NoMemory,
    /// A buffer of the caller's is not mapped for the access.
    BadAddress,
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
            Error::NoMemory => "out of memory",
            Error::BadAddress => "bad address",
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
}

/// A node, shared.
pub type NodeRef = KArc<dyn Node>;

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

    /// The node a new handle to it uses: itself, or for a generated file a
    /// snapshot of its contents taken now, which reads see unchanged.
    fn open(&self, this: &NodeRef) -> Result<NodeRef> {
        Ok(this.clone())
    }

    /// The entry `name` of a directory.
    fn lookup(&self, _name: &str) -> Result<NodeRef> {
        Err(Error::NotDirectory)
    }

    /// Makes an empty file or directory `name` in a directory.
    fn create(&self, _name: &str, _kind: Kind) -> Result<NodeRef> {
        Err(Error::NotDirectory)
    }

    /// Makes a symbolic link `name` to `target` in a directory: a
    /// directory of a filesystem that has none fails with
    /// [`Error::NotSupported`].
    fn make_link(&self, _name: &str, _target: &str) -> Result<NodeRef> {
        if self.kind() == Kind::Directory {
            Err(Error::NotSupported)
        } else {
            Err(Error::NotDirectory)
        }
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

    /// Sets when it was last read and when its contents last changed, in
    /// nanoseconds since 1970 (none: as they are); a filesystem keeps them
    /// as finely as it can (FAT: two seconds, and the day for reads).
    /// Nodes that keep no times (devices, pipes, the synthetic
    /// filesystems' files) fail with [`Error::NotSupported`].
    fn set_times(&self, _accessed: Option<u64>, _modified: Option<u64>) -> Result<()> {
        Err(Error::NotSupported)
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
    /// The processes (`/proc`).
    Proc,
    /// The kernel's devices and settings (`/sys`).
    Sys,
    /// The devices (`/dev`).
    Dev,
    /// The boot archive (`/system`), read-only.
    Boot,
    /// Pipes and pseudo-terminals, which live outside the tree (Phase 8):
    /// their files are on a mount of their own ([`mount::Mount::detached`]).
    Pipe,
}

impl FsKind {
    /// Its name, as `mount` takes it.
    pub fn name(self) -> &'static str {
        match self {
            FsKind::Tmpfs => "tmpfs",
            FsKind::Fat => "fat",
            FsKind::Proc => "proc",
            FsKind::Sys => "sys",
            FsKind::Dev => "dev",
            FsKind::Boot => "bootfs",
            FsKind::Pipe => "pipefs",
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
    KArc::as_ptr(node) as *const u8 as usize
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
}

impl fmt::Write for Text {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        if self.bytes.len() + s.len() > Text::LIMIT
            || mem::extend(&mut self.bytes, s.as_bytes()).is_err()
        {
            self.failed = true;
            return Err(fmt::Error);
        }
        Ok(())
    }
}
