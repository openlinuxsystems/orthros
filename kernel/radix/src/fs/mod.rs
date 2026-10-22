// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Files (Phase 7): the kernel's side of storage.
//!
//! The storage compartments do the work, in safe code: `radix-block`
//! (block devices, partition tables, the sector cache), `radix-vfs` (nodes,
//! the mount table, paths beneath a directory, the page cache, tmpfs,
//! generated files, the boot archive) and `radix-fat`. This module wires
//! them into the kernel (docs/design/storage.md):
//!
//! - [`init`] gives them pages from the frame allocator (owned by
//!   `Owner::PageCache`) and the wall clock, and makes the tree: a tmpfs
//!   root holding only mount points, on which Spark mounts the rest
//!   (`mount` in radix-directory(2)) and lays out the spec 7.1 hierarchy;
//! - `disk` finds virtio block devices on PCI and drives them, one MSI-X
//!   interrupt each, and names them and their partitions (`vda`, `vda1`);
//! - `devfs`, `procfs` and `sysfs` make the generated trees of `/dev`,
//!   `/proc` and `/sys`;
//! - `writeback` is the thread that writes dirty filesystems to their
//!   disks every five seconds, and drops clean cached pages when memory
//!   runs short (the out-of-memory policy asks it first);
//! - [`FileObject`] is what a file or directory capability names: a place
//!   in the tree. `user::files` carries out their methods.
//!
//! A file object's last reference can be the last one to a FAT file
//! removed while open, whose clusters are freed when it goes, which
//! reads and writes the disk. The capability store releases objects with
//! its lock held, so released file objects are only queued there
//! ([`defer_drop`]) and dropped once the lock has gone ([`drop_deferred`],
//! from `caps::with`): no disk is ever waited for with the store locked.

pub mod devfs;
pub mod disk;
pub mod procfs;
pub mod sysfs;
pub mod writeback;

use core::sync::atomic::{AtomicBool, Ordering};

use radix_abi::Status;
use radix_abi::errors::{File as FileError, Kernel};
use radix_hal::{Devices, Timer};
use radix_klog::{error, info};
use radix_mm::{FRAMES, Kmalloc, PAGE_SIZE};
use radix_sync::{Once, SpinLock};
use radix_vfs::mount::{Location, OpenFlags, Vfs};
use radix_vfs::{Error, FileSystem, FsInfo, FsKind, FsRef, NodeRef, mem};

use crate::Arch;

/// A file or directory object, shared.
pub type FileRef = alloc::sync::Arc<FileObject, Kmalloc>;

/// What a file or directory capability names.
pub struct FileObject {
    /// The place in the tree: a mount and a node of it (for a generated
    /// file, the snapshot its `open` took).
    pub at: Location,
    /// The next released object to drop, while it waits in [`DROPPED`].
    next: SpinLock<Option<FileRef>>,
}

impl FileObject {
    /// An object for `at`.
    pub fn new(at: Location) -> Option<FileRef> {
        let object = FileObject { at, next: SpinLock::new(None) };
        alloc::sync::Arc::try_new_in(object, radix_mm::HEAP.with(radix_mm::Flags::MAY_SLEEP)).ok()
    }
}

/// The first of the released objects waiting to be dropped, linked
/// through their `next`, so that queueing one never needs memory.
static DROPPED: SpinLock<Option<FileRef>> = SpinLock::new(None);
/// Whether any object waits in [`DROPPED`], so that the store's changes
/// need not take its lock to see that none does.
static ANY_DROPPED: AtomicBool = AtomicBool::new(false);

/// Lets go of a released object, with the store's lock held: at once if
/// that can only free memory, or else queued until the lock has gone.
/// Only its node can be the last reference that goes: no mount is ever
/// unmounted while a handle holds a place on it.
pub fn release(file: FileRef) {
    if file.at.mount.fs.light_drops() {
        drop(file);
    } else {
        defer_drop(file);
    }
}

/// Queues a released object to be dropped once the store's lock has gone.
fn defer_drop(file: FileRef) {
    let mut head = DROPPED.lock();
    *file.next.lock() = head.take();
    *head = Some(file);
    ANY_DROPPED.store(true, Ordering::Release);
}

/// Drops the objects [`defer_drop`] queued. Called with no lock held.
pub fn drop_deferred() {
    if !ANY_DROPPED.load(Ordering::Acquire) {
        return;
    }
    let mut list = {
        let mut head = DROPPED.lock();
        ANY_DROPPED.store(false, Ordering::Release);
        head.take()
    };
    // One after another, never recursively.
    while let Some(file) = list {
        list = file.next.lock().take();
        drop(file);
    }
}

/// The tree, once [`init`] has run.
static VFS: Once<Vfs> = Once::new();

/// The tree.
pub fn vfs() -> Option<&'static Vfs> {
    VFS.get()
}

/// The most bytes the root's files may hold: it holds mount points.
const ROOT_MAX: u64 = 1 << 20;

/// Sets the storage compartments up and makes the tree's root.
pub fn init() {
    radix_block::mem::set_page_source(<Arch as Devices>::alloc_cache_page);
    radix_vfs::set_clock(|| <Arch as Timer>::wall_clock_ns().unwrap_or(0));
    let made =
        radix_vfs::tmpfs::Tmpfs::new(ROOT_MAX).and_then(|root| Vfs::new(fs_ref(root)?, "tmpfs"));
    match made {
        Ok(tree) => {
            let _ = VFS.set(tree);
            info!("Files: the root is a tmpfs; Spark mounts the rest");
        }
        Err(err) => error!("Files: could not make the root: {err}"),
    }
    devfs::init();
}

/// Makes an empty tmpfs of at most `max` bytes, mounted on a new
/// directory `name` in the root, and returns its root: the tree the fuzz
/// test lets its programs loose in.
pub fn scratch_tree(name: &str, max: u64) -> radix_vfs::Result<Location> {
    let tree = vfs().ok_or(Error::NotFound)?;
    let root = tree.root();
    tree.make_directory(&root, name)?;
    let mounted = radix_vfs::tmpfs::Tmpfs::new(max)
        .and_then(fs_ref)
        .and_then(|fs| tree.mount(&root, name, fs, "tmpfs", false));
    if let Err(err) = mounted {
        let _ = tree.remove(&root, name);
        return Err(err);
    }
    tree.open(&root, name, OpenFlags { directory: true, ..OpenFlags::default() })
}

/// Takes a tree [`scratch_tree`] made away, with whatever was mounted in
/// it since. Fails with in_use while anything holds a place in it.
pub fn remove_scratch_tree(name: &str) -> radix_vfs::Result<()> {
    let tree = vfs().ok_or(Error::NotFound)?;
    let root = tree.root();
    tree.unmount_tree(&root, name)?;
    tree.remove(&root, name)
}

/// A filesystem, as the mount table takes it.
fn fs_ref<T: FileSystem + 'static>(fs: radix_vfs::KArc<T>) -> radix_vfs::Result<FsRef> {
    Ok(fs)
}

/// What a filesystem made for `mount` is: the filesystem, and what it was
/// mounted from, for `/proc/mounts`.
pub struct Made {
    /// The filesystem.
    pub fs: FsRef,
    /// The device's name, or the filesystem kind's.
    pub source: radix_klog::TextBuf<32>,
}

/// Makes a filesystem of the kind named `kind` for `mount`: `fat` on the
/// block device `source` stands for, the others from nothing.
pub fn make(kind: &str, source: Option<&NodeRef>, read_only: bool) -> radix_vfs::Result<Made> {
    use core::fmt::Write;
    let mut name = radix_klog::TextBuf::<32>::new();
    let fs: FsRef = match kind {
        "tmpfs" => {
            // As on Linux, a tmpfs may fill half of memory.
            let max = FRAMES.managed_pages() * PAGE_SIZE as u64 / 2;
            fs_ref(radix_vfs::tmpfs::Tmpfs::new(max)?)?
        }
        "fat" => {
            let node = source.ok_or(Error::NotBlockDevice)?;
            let device = node.block_device().ok_or(Error::NotBlockDevice)?;
            let _ = match disk::name_of(&device) {
                Some(disk) => name.write_str(disk.as_str()),
                None => name.write_str("fat"),
            };
            fs_ref(radix_fat::FatFs::mount(device, read_only)?)?
        }
        "proc" => procfs::make()?,
        "sys" => sysfs::make()?,
        "dev" => devfs::make()?,
        "bootfs" => boot_system()?,
        _ => return Err(Error::UnknownFilesystem),
    };
    if name.as_str().is_empty() {
        let _ = name.write_str(fs.kind().name());
    }
    Ok(Made { fs, source: name })
}

/// The boot archive's `system` directory as a filesystem, for `/system`:
/// the archive's paths start with `system/`, as the kernel and Spark name
/// programs in it.
fn boot_system() -> radix_vfs::Result<FsRef> {
    static BOOT: Once<FsRef> = Once::new();
    if let Some(fs) = BOOT.get() {
        return Ok(fs.clone());
    }
    let bytes = crate::user::init::archive().ok_or(Error::NotFound)?;
    let archive = radix_vfs::bootfs::BootFs::new(bytes)?;
    let root = archive.root().lookup("system")?;
    let fs: FsRef = mem::arc(Subtree { inner: fs_ref(archive)?, root })?;
    // Another thread may have mounted it meanwhile: either one will do,
    // and both read the same archive.
    Ok(match BOOT.set(fs) {
        Ok(fs) => fs.clone(),
        Err(fs) => fs,
    })
}

/// A directory of a filesystem as a filesystem of its own.
struct Subtree {
    inner: FsRef,
    root: NodeRef,
}

impl FileSystem for Subtree {
    fn kind(&self) -> FsKind {
        self.inner.kind()
    }

    fn root(&self) -> NodeRef {
        self.root.clone()
    }

    fn info(&self) -> FsInfo {
        self.inner.info()
    }

    fn read_only(&self) -> bool {
        self.inner.read_only()
    }

    fn sync(&self) -> radix_vfs::Result<()> {
        self.inner.sync()
    }

    fn is_dirty(&self) -> bool {
        self.inner.is_dirty()
    }

    fn shrink(&self, max: usize) -> usize {
        self.inner.shrink(max)
    }

    fn light_drops(&self) -> bool {
        self.inner.light_drops()
    }
}

/// The status a file error becomes (radix-errors(2)).
pub fn status(err: Error) -> Status {
    let file = match err {
        Error::NotFound => FileError::NotFound,
        Error::Exists => FileError::Exists,
        Error::NotDirectory => FileError::NotDirectory,
        Error::IsDirectory => FileError::IsDirectory,
        Error::NotEmpty => FileError::NotEmpty,
        Error::ReadOnly => FileError::ReadOnly,
        Error::NoSpace => FileError::NoSpace,
        Error::NameTooLong => FileError::NameTooLong,
        Error::BadName => FileError::BadName,
        Error::Outside => FileError::Outside,
        Error::CrossMount => FileError::CrossMount,
        Error::InUse => FileError::InUse,
        Error::Io => FileError::Io,
        Error::Corrupt => FileError::Corrupt,
        Error::TooBig => FileError::TooBig,
        Error::UnknownFilesystem => FileError::UnknownFilesystem,
        Error::NotBlockDevice => FileError::NotBlockDevice,
        // What a node does not do is an argument it does not take.
        Error::NotSupported => return Kernel::InvalidArgument.status(0),
        Error::NoMemory => return Kernel::NoMemory.status(0),
        Error::BadAddress => return Kernel::BadAddress.status(0),
    };
    file.status(0)
}
