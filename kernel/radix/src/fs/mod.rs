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
//! - `devfs`, `procfs` and `sysfs` make the generated trees of `/devices`,
//!   `/processes` and `/kernel`, and `kmsg` is `/devices/kmsg`, the kernel log;
//! - `writeback` is the thread that writes dirty filesystems to their
//!   disks every five seconds, and drops clean cached pages when memory
//!   runs short (the out-of-memory policy asks it first);
//! - [`FileObject`] is what a file or directory capability names: a place
//!   in the tree, and since Phase 8 an open file description's offset and
//!   flags. `user::files` carries out their methods;
//! - `pipe` and `tty` are pipes and terminals (Phase 8), and `socket`
//!   sockets (Phase 9), files outside the tree's filesystems; `trace`
//!   writes the records of radix-trace(2) to a pipe for its tracer;
//! - `locks` keeps files' advisory locks (Phase 8): `flock`'s, POSIX's
//!   record locks and open file descriptions' range locks.
//!
//! A file object's last reference can be the last one to a FAT file
//! removed while open, whose clusters are freed when it goes, which
//! reads and writes the disk. The capability store releases objects with
//! its lock held, so released file objects are only queued there
//! ([`defer_drop`]) and dropped once the lock has gone ([`drop_deferred`],
//! from `caps::with`): no disk is ever waited for with the store locked.

pub mod devfs;
pub mod disk;
mod events;
pub mod framebuffer;
pub mod inet;
pub mod keyboard;
pub mod kmsg;
pub mod locks;
pub mod mouse;
pub mod pipe;
pub mod procfs;
pub mod socket;
pub mod sysfs;
pub mod trace;
pub mod tty;
pub mod writeback;

use core::sync::atomic::{AtomicBool, AtomicU32, Ordering};

use radix_abi::Status;
use radix_abi::errors::{File as FileError, Kernel};
use radix_abi::file::DescriptionFlags;
use radix_hal::{Devices, Timer};
use radix_klog::{error, info, warn};
use radix_mm::{FRAMES, Kmalloc, PAGE_SIZE};
use radix_sync::{Mutex, Once, SpinLock};
use radix_vfs::mount::{Location, OpenFlags, Vfs};
use radix_vfs::{Error, FileSystem, FsInfo, FsKind, FsRef, Kind, NodeRef, Who, mem};

use crate::Arch;
use crate::user::names::Identity;

/// A file or directory object, shared.
pub type FileRef = alloc::sync::Arc<FileObject, Kmalloc>;

/// An open file description's status flags (radix-file(2)'s
/// DescriptionFlags).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Description {
    /// Writes at the description's offset go at the end of the file.
    pub append: bool,
    /// Reads and writes of a pipe or terminal fail with would_block
    /// rather than wait.
    pub non_blocking: bool,
}

impl Description {
    fn bits(self) -> u32 {
        let mut bits = 0;
        if self.append {
            bits |= DescriptionFlags::APPEND.0;
        }
        if self.non_blocking {
            bits |= DescriptionFlags::NON_BLOCKING.0;
        }
        bits
    }

    fn from_bits(bits: u32) -> Description {
        Description {
            append: bits & DescriptionFlags::APPEND.0 != 0,
            non_blocking: bits & DescriptionFlags::NON_BLOCKING.0 != 0,
        }
    }
}

/// What kind of node a [`FileObject`] is of, as its reads and writes see
/// it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Special {
    /// A file or directory of a filesystem.
    No,
    /// An end of a pipe.
    Pipe,
    /// A socket of the Unix family.
    Socket,
    /// A network socket.
    Inet,
    /// A side of a terminal.
    Tty,
    /// `/devices/kmsg`.
    Kmsg,
    /// `/devices/kbd`.
    Keyboard,
    /// `/devices/mouse`.
    Mouse,
    /// `/devices/fb0`.
    Framebuffer,
}

/// The most bytes of a path an [`Opened`] keeps.
pub const OPENED_MAX: usize = 104;

/// Where a file object was opened, as an absolute path, for
/// `/processes/<n>/files` and lsof(1): the path its open was given, beneath the
/// path of the directory it was opened from, with `.` and `..` worked out
/// by name. Symbolic links on the way are not followed: it is the path
/// the program named. It is kept in the object itself, so that an open
/// takes no more memory for it, and a longer path than [`OPENED_MAX`]
/// bytes keeps its first ones and is marked cut.
#[derive(Clone, Copy)]
pub struct Opened {
    len: u8,
    state: u8,
    bytes: [u8; OPENED_MAX],
}

impl Opened {
    /// [`Opened::state`]: nothing is known of it.
    const UNKNOWN: u8 = 0;
    /// It is all there.
    const WHOLE: u8 = 1;
    /// Its end was cut off.
    const CUT: u8 = 2;

    /// No path.
    pub const NONE: Opened = Opened { len: 0, state: Opened::UNKNOWN, bytes: [0; OPENED_MAX] };

    /// Makes it where an object for `at` was opened by a path from a
    /// directory: `from` holds where the directory was opened and the
    /// path; it stays none if that is none. A mount's root has its own
    /// path, which the tree keeps.
    fn set(&mut self, at: &Location, from: Option<(&Opened, &[u8])>) {
        let path = if let Some(path) = at.mount_path() {
            self.len = 1;
            self.state = Opened::WHOLE;
            self.bytes[0] = b'/';
            path
        } else if let Some((directory, path)) = from {
            // Two copies of fixed lengths are plain moves; one of the
            // path's length would be a call, and without SSE one of more
            // than 64 bytes a string move, which costs more to start.
            let (low, high) = self.bytes.split_at_mut(64);
            low.copy_from_slice(&directory.bytes[..64]);
            high.copy_from_slice(&directory.bytes[64..]);
            self.len = directory.len;
            self.state = directory.state;
            path
        } else {
            return;
        };
        self.join(path);
    }

    /// Makes it where `path` beneath the directory it was leads. One pass,
    /// which copies each name in as it looks for the name's end: names are
    /// short, and a copy call for each would cost more than the name.
    fn join(&mut self, path: &[u8]) {
        let mut rest = path;
        while !rest.is_empty() && self.state == Opened::WHOLE {
            let len = usize::from(self.len);
            // Where the name goes: after a slash, unless the path is `/`.
            let start = len + usize::from(len > 1);
            let room = self.bytes.get_mut(start..).unwrap_or_default();
            let mut taken = 0;
            for &byte in rest {
                if byte == b'/' {
                    break;
                }
                // Past the end, so harmless if the name is not kept.
                if let Some(slot) = room.get_mut(taken) {
                    *slot = byte;
                }
                taken += 1;
            }
            match &rest[..taken] {
                b"" | b"." => {}
                b".." => self.pop(),
                name if start + name.len() > OPENED_MAX => self.state = Opened::CUT,
                name => {
                    if start > len {
                        self.bytes[len] = b'/';
                    }
                    self.len = (start + name.len()) as u8;
                }
            }
            rest = rest.get(taken + 1..).unwrap_or_default();
        }
    }

    /// Takes the last name off, as `..` does; `/..` is `/`.
    fn pop(&mut self) {
        if self.state != Opened::WHOLE {
            return;
        }
        let len = usize::from(self.len);
        self.len = match self.bytes[..len].iter().rposition(|&b| b == b'/') {
            Some(0) | None => 1,
            Some(at) => at as u8,
        };
    }

    /// The path, and whether its end was cut off; `None` if it is not
    /// known.
    pub fn path(&self) -> Option<(&[u8], bool)> {
        (self.state != Opened::UNKNOWN)
            .then(|| (&self.bytes[..usize::from(self.len)], self.state == Opened::CUT))
    }
}

/// What a file or directory capability names: what POSIX calls an open
/// file description, which copies of the capability share.
pub struct FileObject {
    /// The place in the tree: a mount and a node of it (for a generated
    /// file, the snapshot its `open` took; for a pipe or a terminal, its
    /// own node).
    pub at: Location,
    /// The offset that reads and writes at CURRENT use and move. Locked
    /// for the length of such a read or write, so that two of them on the
    /// description follow one another, as POSIX asks.
    pub offset: Mutex<u64>,
    /// Its [`Description`], as bits.
    flags: AtomicU32,
    /// Whether it has taken a lock (`locks`), which it lets go of when it
    /// goes.
    locked: AtomicBool,
    /// Whether its node is a pipe's end or a terminal's side, found when
    /// it is made: finding out takes dynamic type checks, which every
    /// read and write of a plain file would otherwise make.
    special: Special,
    /// The next released object to drop, while it waits in [`DROPPED`].
    next: SpinLock<Option<FileRef>>,
    /// Where it was opened.
    pub opened: Opened,
    /// The name it acts for (ADR 0214): what the second gate checks on
    /// the files opened or made through it, which take it too.
    pub name: Identity,
}

impl FileObject {
    /// An object for `at`, acting for `name`.
    pub fn new(at: Location, name: Identity) -> Option<FileRef> {
        FileObject::with(at, Description::default(), name)
    }

    /// An object for `at`, with `description`'s flags, acting for `name`.
    pub fn with(at: Location, description: Description, name: Identity) -> Option<FileRef> {
        FileObject::opened(at, description, None, name)
    }

    /// An object for the place `of` is, with its flags and where it was
    /// opened, acting for `name` (radix-names(2)'s `stamp`).
    pub fn stamped(of: &FileObject, name: Identity) -> Option<FileRef> {
        let mut made = FileObject::opened(of.at.clone(), of.description(), None, name)?;
        // Nothing else refers to it yet.
        alloc::sync::Arc::get_mut(&mut made)?.opened = of.opened;
        Some(made)
    }

    /// An object for `at`, with `description`'s flags, opened by a path
    /// from a directory and acting for `name`: `from` holds where the
    /// directory was opened and the path. A mount's root has its own path,
    /// which the tree keeps.
    #[allow(unsafe_code)]
    pub fn opened(
        at: Location,
        description: Description,
        from: Option<(&Opened, &[u8])>,
        name: Identity,
    ) -> Option<FileRef> {
        // Only a node of a pipe's, a socket's or a character device's kind
        // can be one.
        let special = match at.node.kind() {
            Kind::Pipe if pipe::end_of(&at.node).is_some() => Special::Pipe,
            Kind::Socket if socket::socket_of(&at.node).is_some() => Special::Socket,
            Kind::Socket if inet::socket_of(&at.node).is_some() => Special::Inet,
            Kind::CharDevice if tty::handle_of(&at.node).is_some() => Special::Tty,
            Kind::CharDevice if kmsg::is_kmsg(&at.node) => Special::Kmsg,
            Kind::CharDevice if keyboard::is_keyboard(&at.node) => Special::Keyboard,
            Kind::CharDevice if mouse::is_mouse(&at.node) => Special::Mouse,
            Kind::CharDevice if framebuffer::is_framebuffer(&at.node) => Special::Framebuffer,
            _ => Special::No,
        };
        // The devices are the last kinds: one comparison for the others.
        let offset = if special >= Special::Kmsg { device_opened(special) } else { 0 };
        let heap = radix_mm::HEAP.with(radix_mm::Flags::MAY_SLEEP);
        // Made where it stays, and its `Opened` worked out there: an object
        // is too big to move for free. Nothing else refers to it yet, so it
        // needs no check that it is the only reference.
        let mut object = alloc::sync::Arc::try_new_uninit_in(heap).ok()?;
        // SAFETY: the allocation was just made; no other `Arc` or `Weak`
        // to it exists.
        let place = unsafe { alloc::sync::Arc::get_mut_unchecked(&mut object) };
        let made = place.write(FileObject {
            at,
            offset: Mutex::new(offset),
            flags: AtomicU32::new(description.bits()),
            locked: AtomicBool::new(false),
            special,
            next: SpinLock::new(None),
            opened: Opened::NONE,
            name,
        });
        made.opened.set(&made.at, from);
        // SAFETY: written just above.
        Some(unsafe { object.assume_init() })
    }

    /// Its status flags.
    pub fn description(&self) -> Description {
        Description::from_bits(self.flags.load(Ordering::Acquire))
    }

    /// Changes its status flags.
    pub fn set_description(&self, description: Description) {
        self.flags.store(description.bits(), Ordering::Release);
    }

    /// Its address, which owns its locks.
    pub fn address(&self) -> usize {
        core::ptr::from_ref(self).addr()
    }

    /// Notes that it has taken a lock, which goes when it does.
    pub fn note_locked(&self) {
        self.locked.store(true, Ordering::Release);
    }

    /// The end of a pipe it is of, if it is of one.
    pub fn pipe_end(&self) -> Option<&pipe::End> {
        if self.special == Special::Pipe { pipe::end_of(&self.at.node) } else { None }
    }

    /// The socket it is of, if it is of one.
    pub fn socket(&self) -> Option<&socket::Socket> {
        if self.special == Special::Socket { socket::socket_of(&self.at.node) } else { None }
    }

    /// The network socket it is of, if it is of one.
    pub fn inet(&self) -> Option<&inet::InetSocket> {
        if self.special == Special::Inet { inet::socket_of(&self.at.node) } else { None }
    }

    /// The side of a terminal it is of, if it is of one.
    pub fn tty_handle(&self) -> Option<&tty::Handle> {
        if self.special == Special::Tty { tty::handle_of(&self.at.node) } else { None }
    }

    /// Whether it is of `/devices/kmsg`.
    pub fn is_kmsg(&self) -> bool {
        self.special == Special::Kmsg
    }

    /// Whether it is of `/devices/kbd`.
    pub fn is_keyboard(&self) -> bool {
        self.special == Special::Keyboard
    }

    /// Whether it is of `/devices/mouse`.
    pub fn is_mouse(&self) -> bool {
        self.special == Special::Mouse
    }

    /// Whether it is of `/devices/fb0`.
    pub fn is_framebuffer(&self) -> bool {
        self.special == Special::Framebuffer
    }

    /// Whether its reads and writes are a pipe's, a socket's, a terminal's
    /// or a device's, not a node's: one comparison, for the paths that
    /// plain files take.
    pub fn is_special(&self) -> bool {
        self.special != Special::No
    }
}

/// A description of a device was made: returns its first offset.
#[cold]
fn device_opened(special: Special) -> u64 {
    match special {
        Special::Kmsg => {
            kmsg::opened();
            0
        }
        Special::Keyboard => keyboard::opened(),
        Special::Mouse => mouse::opened(),
        _ => 0,
    }
}

impl Drop for FileObject {
    fn drop(&mut self) {
        match self.special {
            Special::Kmsg => kmsg::closed(),
            Special::Framebuffer => framebuffer::closed(self.address()),
            _ => {}
        }
        if self.locked.load(Ordering::Acquire) {
            locks::description_gone(&self.at.node, self.address());
        }
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
/// unmounted while a handle holds a place on it. A terminal's side, whose
/// last description going may hang the terminal up and send signals, is
/// on `dev`, whose drops are light otherwise.
pub fn release(file: FileRef) {
    if file.at.mount.fs.light_drops() && file.tty_handle().is_none() {
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
    radix_vfs::set_random(radix_random::fill);
    // The root's own mode is Linux's for its rootfs; Spark makes the
    // directories in it.
    let made = tmpfs(ROOT_MAX, 0o755).and_then(|root| Vfs::new(fs_ref(root)?, "tmpfs"));
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
    let k = &Who::KERNEL;
    tree.make_directory(k, &root, name, radix_vfs::Access::default_for(Kind::Directory))?;
    let mounted = tmpfs(max, TMPFS_MODE)
        .and_then(fs_ref)
        .and_then(|fs| tree.mount(k, &root, name, fs, "tmpfs", false));
    if let Err(err) = mounted {
        let _ = tree.remove(k, &root, name);
        return Err(err);
    }
    tree.open(k, &root, name, OpenFlags { directory: true, ..OpenFlags::default() })
}

/// Takes a tree [`scratch_tree`] made away, with whatever was mounted in
/// it since. Fails with in_use while anything holds a place in it.
pub fn remove_scratch_tree(name: &str) -> radix_vfs::Result<()> {
    let tree = vfs().ok_or(Error::NotFound)?;
    let root = tree.root();
    tree.unmount_tree(&Who::KERNEL, &root, name)?;
    tree.remove(&Who::KERNEL, &root, name)
}

/// The mode of a mounted tmpfs's root, as Linux has it: all may make
/// files there, and remove only their own (ADR 0213). Spark narrows those
/// that are not `/tmp`.
const TMPFS_MODE: u16 = 0o1777;

/// An empty tmpfs whose files may hold `max` bytes together, and whose
/// root has `mode`. As on Linux, it may hold one file or directory for
/// every two pages of memory.
fn tmpfs(max: u64, mode: u16) -> radix_vfs::Result<radix_vfs::KArc<radix_vfs::tmpfs::Tmpfs>> {
    radix_vfs::tmpfs::Tmpfs::new(max, FRAMES.managed_pages() / 2, mode)
}

/// A filesystem, as the mount table takes it.
fn fs_ref<T: FileSystem + 'static>(fs: radix_vfs::KArc<T>) -> radix_vfs::Result<FsRef> {
    Ok(fs)
}

/// What a filesystem made for `mount` is: the filesystem, and what it was
/// mounted from, for `/processes/mounts`.
pub struct Made {
    /// The filesystem.
    pub fs: FsRef,
    /// The device's name, or the filesystem kind's.
    pub source: radix_klog::TextBuf<32>,
}

/// How a disk's filesystem is mounted.
#[derive(Clone, Copy, Default)]
pub struct Flags {
    /// Refusing every change.
    pub read_only: bool,
    /// Compressing the data written with zstd: Btrfs only (ADR 0134).
    pub compress: bool,
    /// Writing an NTFS that Windows hibernated with anyway, throwing the
    /// hibernation away: NTFS only (ADR 0238).
    pub discard_hibernation: bool,
}

/// Makes a filesystem of the kind named `kind` for `mount`: `fat`,
/// `exfat`, `btrfs`, `ext4` and `ntfs` on the block device `source` stands
/// for, the others from nothing, mounted as `flags` say. A Btrfs that
/// Radix cannot write yet, an exFAT with two tables, an ext4 with features
/// Radix only reads, or an NTFS that Windows left dirty, hibernated or
/// with a log to replay, fails a writable mount with `NotSupported` (ADRs
/// 0130, 0140, 0141 and 0237), and mounts read-only.
pub fn make(kind: &str, source: Option<&NodeRef>, flags: Flags) -> radix_vfs::Result<Made> {
    let Flags { read_only, compress, discard_hibernation } = flags;
    use core::fmt::Write;
    let mut name = radix_klog::TextBuf::<32>::new();
    let fs: FsRef = match kind {
        "tmpfs" => {
            // As on Linux, a tmpfs may fill half of memory.
            let max = FRAMES.managed_pages() * PAGE_SIZE as u64 / 2;
            fs_ref(tmpfs(max, TMPFS_MODE)?)?
        }
        "fat" | "exfat" | "btrfs" | "ext4" | "ntfs" => {
            let node = source.ok_or(Error::NotBlockDevice)?;
            let device = node.block_device().ok_or(Error::NotBlockDevice)?;
            if radix_block::swap::is_on(&*device) {
                return Err(Error::InUse);
            }
            let _ = match disk::name_of(&device) {
                Some(disk) => name.write_str(disk.as_str()),
                None => name.write_str(kind),
            };
            match kind {
                "fat" => mount_fat(device, read_only)?,
                "exfat" => mount_exfat(device, read_only)?,
                "ext4" => mount_ext4(device, read_only)?,
                "ntfs" => mount_ntfs(device, name.as_str(), read_only, discard_hibernation)?,
                _ => mount_btrfs(device, name.as_str(), read_only, compress)?,
            }
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

/// Mounts the FAT filesystem on `device`. A function of its own, as each
/// filesystem's mount is, so that a mount system call's stack holds only
/// the frames of the one it makes.
#[inline(never)]
fn mount_fat(device: radix_block::DeviceRef, read_only: bool) -> radix_vfs::Result<FsRef> {
    fs_ref(radix_fat::FatFs::mount(device, read_only)?)
}

/// Mounts the exFAT filesystem on `device` ([`mount_fat`]).
#[inline(never)]
fn mount_exfat(device: radix_block::DeviceRef, read_only: bool) -> radix_vfs::Result<FsRef> {
    fs_ref(radix_exfat::ExfatFs::mount(device, read_only)?)
}

/// Mounts the ext4 filesystem on `device` ([`mount_fat`]).
#[inline(never)]
fn mount_ext4(device: radix_block::DeviceRef, read_only: bool) -> radix_vfs::Result<FsRef> {
    fs_ref(radix_ext4::Ext4Fs::mount(device, read_only)?)
}

/// Mounts the NTFS filesystem on `device`, the disk `name` ([`mount_fat`]),
/// throwing away Windows' hibernation on it if `discard_hibernation`.
/// Mounted read-only, it says why it could not be written, if that is so
/// (ADR 0237); and it says when it threw a hibernation away (ADR 0238).
#[inline(never)]
fn mount_ntfs(
    device: radix_block::DeviceRef,
    name: &str,
    read_only: bool,
    discard_hibernation: bool,
) -> radix_vfs::Result<FsRef> {
    let options = radix_ntfs::fs::Options { read_only, discard_hibernation };
    let fs = radix_ntfs::NtfsFs::mount_with(device, options)?;
    if let Some(why) = fs.kept_from_writing() {
        warn!("Files: the NTFS of {name} is only read: {why}");
    }
    if fs.discarded_hibernation() {
        warn!(
            "Files: the NTFS of {name} is written although Windows hibernated with it, as its \
             owner asked: Windows starts afresh, without what it left open, and checks the volume"
        );
    }
    fs_ref(fs)
}

/// A Btrfs filesystem mounted: its ID, the disk it was mounted from, and
/// the filesystem while it lasts.
struct Mounted {
    id: [u8; 16],
    first: radix_block::DeviceRef,
    /// How many of its disks were missing when the log last said.
    missing: usize,
    fs: alloc::sync::Weak<radix_btrfs::BtrfsFs, mem::Alloc>,
}

/// The Btrfs filesystems mounted, so that a disk of one is not mounted
/// again on its own (ADR 0153). It is held through a Btrfs mount, so that
/// two mounts cannot make one filesystem twice.
static BTRFS: Mutex<Option<mem::KVec<Mounted>>> = Mutex::new(None);

/// Mounts the Btrfs filesystem on `device`, the disk or partition `name`,
/// with the disks of it among the others ([`mount_fat`]). With a disk
/// missing it mounts while the others hold a copy of all of it, and the
/// log warns (ADR 0153). A disk of a Btrfs mounted already fails with
/// `InUse`, unless it is the disk that one was mounted from and both
/// mounts write, or neither does: then this mount gets that filesystem
/// too, as on Linux.
#[inline(never)]
fn mount_btrfs(
    device: radix_block::DeviceRef,
    name: &str,
    read_only: bool,
    compress: bool,
) -> radix_vfs::Result<FsRef> {
    let id = radix_btrfs::BtrfsFs::id_of(&device)?;
    let mut mounted = BTRFS.lock();
    let mounted = mounted.get_or_insert_with(mem::vec);
    mounted.retain(|m| m.fs.strong_count() > 0);
    let fs = match mounted.iter().find(|m| m.id == id) {
        Some(m) => {
            let fs = m.fs.upgrade().ok_or(Error::InUse)?;
            if !radix_vfs::KArc::ptr_eq(&m.first, &device) || fs.writable() == read_only {
                return Err(Error::InUse);
            }
            fs
        }
        None => {
            let fs = mount_btrfs_disks(&device, read_only)?;
            warn_missing(name, &fs);
            let (first, missing) = (device, fs.disks().1);
            let weak = radix_vfs::KArc::downgrade(&fs);
            mem::push(mounted, Mounted { id, first, missing, fs: weak })?;
            fs
        }
    };
    if !read_only {
        fs.set_compress(compress);
    }
    fs_ref(fs)
}

/// Puts `device` in the place of disk `devid` of the Btrfs `fs`, and
/// rebuilds onto it (ADR 0155). Fails with `InUse` while `device` is a
/// swap area, holds a mount, or is a disk of a mounted Btrfs, and as
/// [`radix_vfs::FileSystem::replace_disk`] does. Btrfs mounts wait
/// meanwhile, so that none takes the disk.
pub fn replace_disk(
    fs: &FsRef,
    devid: u64,
    device: radix_block::DeviceRef,
) -> radix_vfs::Result<radix_vfs::Scrub> {
    let mounted = BTRFS.lock();
    let name = free_disk(&mounted, &device)?;
    let counts = fs.replace_disk(devid, device)?;
    let name = name.as_ref().map_or("?", |n| n.as_str());
    info!(
        "Btrfs: disk {devid} replaced by {name}, and rebuilt from the other disks' copies; {} \
         errors no copy mends",
        counts.uncorrectable_errors
    );
    Ok(counts)
}

/// Adds `device` to the disks of the Btrfs `fs` (ADR 0159), and returns
/// its device ID. Fails with `InUse` as [`replace_disk`] does, and as
/// [`radix_vfs::FileSystem::add_disk`] does. Btrfs mounts wait meanwhile,
/// so that none takes the disk.
pub fn add_disk(fs: &FsRef, device: radix_block::DeviceRef) -> radix_vfs::Result<u64> {
    let mounted = BTRFS.lock();
    let name = free_disk(&mounted, &device)?;
    let devid = fs.add_disk(device)?;
    let name = name.as_ref().map_or("?", |n| n.as_str());
    info!("Btrfs: {name} added as disk {devid}");
    Ok(devid)
}

/// The name of `device`, if it has one, when no mount, swap area or
/// mounted Btrfs uses it; `InUse` otherwise.
fn free_disk(
    mounted: &Option<mem::KVec<Mounted>>,
    device: &radix_block::DeviceRef,
) -> radix_vfs::Result<Option<disk::Name>> {
    let mut name = None;
    disk::for_each(|entry| {
        if radix_vfs::KArc::ptr_eq(&entry.device, device) {
            name = Some(entry.name.clone());
        }
    });
    let mut holds_mount = false;
    if let (Some(name), Some(vfs)) = (&name, vfs()) {
        vfs.mounts(|mount| holds_mount |= mount.source() == name.as_str());
    }
    let btrfs_disk =
        mounted.iter().flatten().filter_map(|m| m.fs.upgrade()).any(|other| other.has_disk(device));
    if holds_mount || btrfs_disk || radix_block::swap::is_on(&**device) {
        return Err(Error::InUse);
    }
    Ok(name)
}

/// Warns when the Btrfs `fs`, mounted from `name`, mounted with disks
/// missing, or with disks that missed changes: in a frame of its own, off
/// the mount's deep reads.
#[inline(never)]
fn warn_missing(name: &str, fs: &radix_btrfs::BtrfsFs) {
    let (disks, missing) = fs.disks();
    if missing > 0 {
        warn!(
            "Btrfs: {name}: {missing} of its {disks} disks missing; mounted from the copies on \
             the others. Replace the missing, or a disk more lost may lose data"
        );
    }
    let behind = fs.behind();
    if behind > 0 {
        warn!(
            "Btrfs: {name}: {behind} of its {disks} disks missed changes; read from the others \
             until a scrub catches them up"
        );
    }
}

/// Warns of each Btrfs mounted that a disk failed and was left out since
/// the log last said (ADR 0153), as the mount warns of a disk missing.
/// The writeback thread asks after each of its rounds, so the log says
/// within seconds of the failure, holding no lock.
pub fn warn_failed_disks() {
    let mut failed: mem::KVec<(radix_block::DeviceRef, usize, usize)> = mem::vec();
    let mut held = mem::vec();
    {
        let mut mounted = BTRFS.lock();
        for m in mounted.iter_mut().flatten() {
            let Some(fs) = m.fs.upgrade() else { continue };
            let (disks, missing) = fs.disks();
            if missing > m.missing {
                let _ = mem::push(&mut failed, (m.first.clone(), disks, missing));
            }
            m.missing = missing;
            // Dropped once the lock is let go: the last reference
            // unmounts.
            let _ = mem::push(&mut held, fs);
        }
    }
    drop(held);
    for (device, disks, missing) in &failed {
        let name = disk::name_of(device);
        let name = name.as_ref().map_or("?", |n| n.as_str());
        warn!(
            "Btrfs: {name}: a disk failed and was left out; {missing} of its {disks} disks \
             missing now, read from the copies on the others. Replace the missing, or a disk \
             more lost may lose data"
        );
    }
}

/// Mounts the Btrfs filesystem on `device` with the disks of it among the
/// others the kernel found, but those it swaps to, as with Linux's
/// `degraded` mount option.
#[inline(never)]
fn mount_btrfs_disks(
    device: &radix_block::DeviceRef,
    read_only: bool,
) -> radix_vfs::Result<radix_vfs::KArc<radix_btrfs::BtrfsFs>> {
    let mut devices = mem::vec();
    mem::push(&mut devices, device.clone())?;
    let mut pushed = Ok(());
    disk::for_each(|entry| {
        if pushed.is_ok() && !radix_vfs::KArc::ptr_eq(&entry.device, device) {
            pushed = mem::push(&mut devices, entry.device.clone());
        }
    });
    pushed?;
    devices.retain(|other| !radix_block::swap::is_on(&**other));
    radix_btrfs::BtrfsFs::mount_disks(&devices, !read_only, true)
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
        Error::NoDevice => FileError::NoDevice,
        Error::Loop => FileError::TooManyLinks,
        Error::NotLink => FileError::NotLink,
        Error::TooManyNames => FileError::TooManyNames,
        Error::QuotaExceeded => FileError::QuotaExceeded,
        Error::Denied => FileError::Denied,
        Error::NoAttribute => FileError::NoAttribute,
        Error::OutOfRange => return Kernel::OutOfRange.status(0),
        Error::NotPermitted => return Kernel::NotPermitted.status(0),
        // What a node does not do is an argument it does not take.
        Error::NotSupported => return Kernel::InvalidArgument.status(0),
        Error::NoMemory => return Kernel::NoMemory.status(0),
        Error::BadAddress => return Kernel::BadAddress.status(0),
        Error::InvalidArgument => return Kernel::InvalidArgument.status(0),
    };
    file.status(0)
}
