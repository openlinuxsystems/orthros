// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Pipes (Phase 8, radix-process(2)'s `pipe`): a buffer of [`CAPACITY`]
//! bytes with a reading end and a writing end.
//!
//! Each end is a node of its own on a filesystem that is mounted nowhere
//! (`pipefs`, [`radix_vfs::mount::Mount::detached`]), so a pipe's ends are file
//! descriptions like any other: capabilities to them are copied, passed
//! and inherited across `fork`, and the pipe learns that an end has gone
//! when the last description of it goes (the node's `Drop`). Reading takes
//! what is there, waiting while nothing is (unless the description is
//! non-blocking); once no writing end is left, an empty pipe reads as its
//! end. Writing waits for room: a write of up to [`ATOMIC`] bytes goes in
//! whole or not at all, so writers never mix their pieces, and a longer
//! one goes in as room comes. Writing to a pipe no end reads raises
//! SIGPIPE for the writing thread and fails with gone.
//!
//! The waits are the kernel's wait queue's, on the pipe's address; their
//! conditions read only the atomic counts kept beside the buffer. Copies
//! to and from the program's memory are made with the buffer's lock held:
//! it is a sleeping lock, so a page the copy needs may be made meanwhile.
//!
//! The pipes of a socket's connection carry files too (`socket`,
//! radix-file(2)'s `send_files`): a write may bring some, which ride on
//! its first byte. A read never goes past a byte that has files on it
//! unless it starts there, so the files come out with the bytes they were
//! sent with; a read that does not take them closes them.

use alloc::sync::Weak;
use alloc::vec::Vec;
use core::any::Any;
use core::sync::atomic::{AtomicU32, AtomicUsize, Ordering};

use radix_abi::Status;
use radix_abi::errors::{File as FileError, Kernel};
use radix_abi::file::FILES_IN_FLIGHT_MAX;
use radix_abi::process::PipeFlags;
use radix_abi::signal::SIGPIPE;
use radix_cap::{ObjectKind, Rights, SlotRef};
use radix_mm::Kmalloc;
use radix_sync::{Mutex, Once};
use radix_tty::Ring;
use radix_vfs::mount::{Location, MountRef};
use radix_vfs::synthetic::FixedDir;
use radix_vfs::{
    FileSystem, FsInfo, FsKind, FsRef, In, KArc, Kind, Node, NodeRef, Out, Stat, mem, next_id,
};

use super::{Description, FileObject, FileRef};
use crate::sched::{self, wait};
use crate::user::Fifo;
use crate::user::posix::poll::{self, Polled};
use crate::user::posix::signal::{Restart, interrupted};

/// How many bytes a pipe holds, as on Linux.
pub const CAPACITY: usize = 65536;

/// The most bytes a write puts in at once, unmixed with others: POSIX's
/// PIPE_BUF.
pub const ATOMIC: usize = 4096;

/// A file sent through a socket: a copy of a capability to it.
pub struct Passed {
    /// What the capability is to: a file or a directory.
    pub kind: ObjectKind,
    /// The rights it has.
    pub rights: Rights,
    /// The file or directory.
    pub file: FileRef,
}

/// Files sent together.
pub type Files = alloc::vec::Vec<Passed, Kmalloc>;

/// Files riding on a byte of a pipe.
struct Attachment {
    /// The byte's place in all that was ever written to the pipe.
    at: u64,
    files: Files,
}

/// A pipe's buffer, and the files riding on its bytes.
struct Buffer {
    ring: Ring<alloc::vec::Vec<u8, Kmalloc>>,
    /// How many bytes have been read from it in all: the place of the
    /// first byte in `ring`.
    taken: u64,
    /// The files riding on its bytes, in the order of their places.
    attachments: Fifo<Attachment>,
}

impl Buffer {
    /// How many bytes a read may take, and whether files ride on the
    /// first: a read ends before the next byte files ride on.
    fn boundary(&self) -> (usize, bool) {
        let mut ahead = self.attachments.iter().map(|a| (a.at - self.taken) as usize);
        match ahead.next() {
            None => (usize::MAX, false),
            Some(0) => (ahead.next().unwrap_or(usize::MAX), true),
            Some(n) => (n, false),
        }
    }
}

/// The heap a pipe's memory comes from.
fn heap() -> Kmalloc {
    radix_mm::HEAP.with(radix_mm::Flags::MAY_SLEEP)
}

/// A pipe.
pub struct Pipe {
    /// Its buffer.
    buffer: Mutex<Buffer>,
    /// How many bytes it holds, as the buffer says, for the waits.
    len: AtomicUsize,
    /// How many reading and writing ends are open.
    readers: AtomicU32,
    writers: AtomicU32,
    /// How many files ride on its bytes.
    files: AtomicUsize,
    /// Its number, which both ends' nodes show.
    id: u64,
    /// Its pollers.
    polled: Polled,
    /// How many times an end was opened to read, and to write: a named
    /// pipe's open waits for the other kind's to change.
    opened: [AtomicU32; 2],
    /// For a named pipe, its node, kept while the pipe is open so that
    /// [`NAMED`] finds the pipe by the node's address.
    _named: Option<NodeRef>,
}

/// Pipes by the address of their node.
type ByNode = Vec<(usize, Weak<Pipe, Kmalloc>), Kmalloc>;

/// The named pipes open now, by the address of their node.
static NAMED: Mutex<Option<ByNode>> = Mutex::new(None);

impl Pipe {
    /// A new, empty pipe, with one end reading it and one writing it.
    pub(super) fn new() -> Result<KArc<Pipe>, Status> {
        Pipe::with_ends(1, None)
    }

    /// A new, empty pipe, with `ends` ends reading it and as many writing
    /// it, the pipe `named` is if any.
    fn with_ends(ends: u32, named: Option<NodeRef>) -> Result<KArc<Pipe>, Status> {
        let mut buffer = alloc::vec::Vec::new_in(heap());
        buffer.try_reserve_exact(CAPACITY).map_err(|_| Kernel::NoMemory.status(0))?;
        while buffer.len() < CAPACITY {
            // Room was reserved above.
            let _ = buffer.push_within_capacity(0);
        }
        let buffer = Buffer { ring: Ring::new(buffer), taken: 0, attachments: Fifo::new() };
        mem::arc(Pipe {
            buffer: Mutex::new(buffer),
            len: AtomicUsize::new(0),
            readers: AtomicU32::new(ends),
            writers: AtomicU32::new(ends),
            files: AtomicUsize::new(0),
            id: next_id(),
            polled: Polled::new(),
            opened: [AtomicU32::new(0), AtomicU32::new(0)],
            _named: named,
        })
        .map_err(|_| Kernel::NoMemory.status(0))
    }

    fn key(&self) -> usize {
        core::ptr::from_ref::<Pipe>(self) as usize
    }

    /// Something changed: the waits look again, and so do its pollers.
    fn changed(&self) {
        wait::wake_all(self.key());
        self.polled.changed();
    }

    /// What its pollers count themselves in.
    pub fn polled(&self) -> &Polled {
        &self.polled
    }

    /// The events `poll` finds on an end of it, as Linux's: a reading end
    /// is readable while it holds data, and hung up once no end writes it;
    /// a writing end is writable while [`ATOMIC`] bytes fit, and in error
    /// once no end reads it.
    pub fn events(&self, write: bool) -> u16 {
        let len = self.len.load(Ordering::Acquire);
        let mut events = 0;
        if write {
            if CAPACITY - len >= ATOMIC {
                events |= poll::WRITABLE;
            }
            if self.readers.load(Ordering::Acquire) == 0 {
                events |= poll::ERROR;
            }
        } else {
            if len > 0 {
                events |= poll::READABLE;
            }
            if self.writers.load(Ordering::Acquire) == 0 {
                events |= poll::HANG_UP;
            }
        }
        events
    }

    /// How many bytes it holds.
    pub fn len(&self) -> usize {
        self.len.load(Ordering::Acquire)
    }

    /// Whether a write of [`ATOMIC`] bytes would fit now.
    pub fn has_room(&self) -> bool {
        CAPACITY - self.len.load(Ordering::Acquire) >= ATOMIC
    }

    /// How many ends read it, and how many write it.
    pub fn readers(&self) -> u32 {
        self.readers.load(Ordering::Acquire)
    }

    pub fn writers(&self) -> u32 {
        self.writers.load(Ordering::Acquire)
    }

    /// A reading end has gone. Once none is left, the files riding on
    /// its bytes, which no one can read now, are closed.
    pub(super) fn close_reader(&self) {
        if self.readers.fetch_sub(1, Ordering::AcqRel) == 1 {
            let unread = {
                let mut buffer = self.buffer.lock();
                self.files.store(0, Ordering::Release);
                buffer.attachments.take()
            };
            drop(unread);
        }
        self.changed();
    }

    /// A writing end has gone.
    pub(super) fn close_writer(&self) {
        self.writers.fetch_sub(1, Ordering::AcqRel);
        self.changed();
    }

    /// Reads up to `out.len()` bytes into `out`, waiting for some unless
    /// `non_blocking`; returns 0 once no writer is left. Files that came
    /// with what it read are closed.
    pub fn read(&self, out: &mut dyn Out, non_blocking: bool) -> Result<usize, Status> {
        self.read_files(out, non_blocking).map(|(read, _)| read)
    }

    /// Reads as [`Pipe::read`] does, and returns the files that came with
    /// what it read, if any did: a read that starts at a byte files ride
    /// on takes them, and one that does not ends before such a byte.
    pub fn read_files(
        &self,
        out: &mut dyn Out,
        non_blocking: bool,
    ) -> Result<(usize, Option<Files>), Status> {
        let want = out.len();
        if want == 0 {
            return Ok((0, None));
        }
        loop {
            {
                let mut buffer = self.buffer.lock();
                if !buffer.ring.is_empty() {
                    let (most, with_files) = buffer.boundary();
                    let mut at = 0;
                    let read = buffer.ring.read_with(want.min(most), &mut |run| {
                        out.put(at, run)?;
                        at += run.len();
                        Ok::<(), radix_vfs::Error>(())
                    });
                    let taken = *read.as_ref().unwrap_or(&0);
                    buffer.taken += taken as u64;
                    let files = if with_files && taken > 0 {
                        buffer.attachments.pop_front().map(|attachment| attachment.files)
                    } else {
                        None
                    };
                    if let Some(files) = &files {
                        self.files.fetch_sub(files.len(), Ordering::AcqRel);
                    }
                    self.len.store(buffer.ring.len(), Ordering::Release);
                    drop(buffer);
                    self.changed();
                    return read.map(|read| (read, files)).map_err(super::status);
                }
                if self.writers.load(Ordering::Acquire) == 0 {
                    return Ok((0, None));
                }
            }
            if non_blocking {
                return Err(Kernel::WouldBlock.status(0));
            }
            wait::wait_until(self.key(), "pipe read", || {
                self.len.load(Ordering::Acquire) > 0
                    || self.writers.load(Ordering::Acquire) == 0
                    || sched::interrupted()
            });
            if sched::interrupted() && self.len.load(Ordering::Acquire) == 0 {
                return Err(interrupted(Restart::Always));
            }
        }
    }

    /// Writes `data`, waiting for room unless `non_blocking`; returns how
    /// many bytes went in, all of them unless a wait was cut short after
    /// some had. With no reader left, it fails with gone, and with
    /// `signal` raises SIGPIPE for the writing thread first.
    pub fn write(&self, data: &dyn In, non_blocking: bool, signal: bool) -> Result<usize, Status> {
        self.write_files(data, None, non_blocking, signal)
    }

    /// Writes as [`Pipe::write`] does, with `files` riding on the first
    /// byte of `data`; fails with too_many if they would make more than
    /// FILES_IN_FLIGHT_MAX on their way. Files that do not go in are
    /// closed.
    pub fn write_files(
        &self,
        data: &dyn In,
        files: Option<Files>,
        non_blocking: bool,
        signal: bool,
    ) -> Result<usize, Status> {
        self.put(data, files, 0, non_blocking, signal)
    }

    /// Writes `data`, of at most [`ATOMIC`] bytes, whole, as the kernel
    /// writes a trace's records (`trace`): waits, unless `non_blocking`,
    /// until it fits with `keep` bytes still free after it. Fails with
    /// gone once no end reads the pipe, without SIGPIPE.
    pub(super) fn write_keeping(
        &self,
        data: &dyn In,
        keep: usize,
        non_blocking: bool,
    ) -> Result<usize, Status> {
        debug_assert!(data.len() <= ATOMIC);
        self.put(data, None, keep, non_blocking, false)
    }

    /// [`Pipe::write_files`], leaving `keep` bytes free after a write of
    /// up to [`ATOMIC`] bytes.
    fn put(
        &self,
        data: &dyn In,
        mut files: Option<Files>,
        keep: usize,
        non_blocking: bool,
        signal: bool,
    ) -> Result<usize, Status> {
        let total = data.len();
        if total == 0 {
            return Ok(0);
        }
        let atomic = total <= ATOMIC;
        let mut done = 0;
        while done < total {
            {
                let mut buffer = self.buffer.lock();
                if self.readers.load(Ordering::Acquire) == 0 {
                    drop(buffer);
                    return broken(done, signal);
                }
                let left = total - done;
                let free = buffer.ring.free();
                if (atomic && free >= left + keep) || (!atomic && free > 0) {
                    // The files go with the first piece written.
                    let count = files.as_ref().map_or(0, alloc::vec::Vec::len);
                    if count > 0 {
                        let flying = self.files.load(Ordering::Acquire);
                        if flying + count > FILES_IN_FLIGHT_MAX as usize {
                            return Err(Kernel::TooMany.status(0));
                        }
                        if !buffer.attachments.reserve_one() {
                            return Err(Kernel::NoMemory.status(0));
                        }
                    }
                    let place = buffer.taken + buffer.ring.len() as u64;
                    let mut at = done;
                    let wrote = buffer.ring.write_with(left, &mut |run| {
                        data.get(at, run)?;
                        at += run.len();
                        Ok::<(), radix_vfs::Error>(())
                    });
                    if let Ok(1..) = wrote
                        && let Some(files) = files.take()
                    {
                        self.files.fetch_add(files.len(), Ordering::AcqRel);
                        // Room was reserved above.
                        let _ = buffer.attachments.push_back(Attachment { at: place, files });
                    }
                    self.len.store(buffer.ring.len(), Ordering::Release);
                    drop(buffer);
                    self.changed();
                    match wrote {
                        Ok(n) => done += n,
                        Err(err) if done == 0 => return Err(super::status(err)),
                        Err(_) => return Ok(done),
                    }
                    continue;
                }
            }
            if non_blocking {
                return if done > 0 { Ok(done) } else { Err(Kernel::WouldBlock.status(0)) };
            }
            let room = if atomic { total - done + keep } else { 1 };
            wait::wait_until(self.key(), "pipe write", || {
                CAPACITY - self.len.load(Ordering::Acquire) >= room
                    || self.readers.load(Ordering::Acquire) == 0
                    || sched::interrupted()
            });
            if sched::interrupted() {
                return if done > 0 { Ok(done) } else { Err(interrupted(Restart::Always)) };
            }
        }
        Ok(done)
    }
}

/// A write found no reader: SIGPIPE for the writer if `signal`, and gone
/// (unless some of it went in first).
pub(super) fn broken(done: usize, signal: bool) -> Result<usize, Status> {
    if signal {
        crate::user::posix::signal::raise_current(SIGPIPE);
    }
    if done > 0 { Ok(done) } else { Err(Kernel::Gone.status(0)) }
}

/// One end of a pipe, as a node.
pub struct End {
    pipe: KArc<Pipe>,
    /// Whether it is the writing end.
    pub write: bool,
    /// Whether a writing end reads too: a named pipe opened to read and
    /// write, as Linux allows.
    reads_too: bool,
}

impl End {
    /// The pipe.
    pub fn pipe(&self) -> &Pipe {
        &self.pipe
    }

    /// The events `poll` finds on it ([`Pipe::events`]).
    pub fn events(&self) -> u16 {
        let events = self.pipe.events(self.write);
        if self.reads_too { events | self.pipe.events(false) } else { events }
    }
}

impl Drop for End {
    fn drop(&mut self) {
        if self.write {
            self.pipe.close_writer();
        }
        if !self.write || self.reads_too {
            self.pipe.close_reader();
        }
    }
}

impl Node for End {
    fn stat(&self) -> Stat {
        Stat {
            kind: Kind::Pipe,
            size: self.pipe.len() as u64,
            allocated: 0,
            id: self.pipe.id,
            created: 0,
            modified: 0,
            accessed: 0,
            changed: 0,
            links: 1,
            device: 0,
        }
    }

    fn kind(&self) -> Kind {
        Kind::Pipe
    }

    // Reads and writes through the node itself (the kernel's own, such as
    // a ring's) never wait.
    fn read(&self, _offset: u64, out: &mut dyn Out) -> radix_vfs::Result<usize> {
        if self.write {
            return Err(radix_vfs::Error::NotSupported);
        }
        self.pipe.read(out, true).map_err(|_| radix_vfs::Error::NotSupported)
    }

    fn write(&self, _offset: u64, data: &dyn In) -> radix_vfs::Result<usize> {
        if !self.write {
            return Err(radix_vfs::Error::NotSupported);
        }
        self.pipe.write(data, true, true).map_err(|_| radix_vfs::Error::NotSupported)
    }

    fn set_size(&self, _size: u64) -> radix_vfs::Result<()> {
        Err(radix_vfs::Error::NotSupported)
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

/// The filesystem pipes' ends are on, which is mounted nowhere.
struct PipeFs {
    root: NodeRef,
}

impl FileSystem for PipeFs {
    fn kind(&self) -> FsKind {
        FsKind::Pipe
    }

    fn root(&self) -> NodeRef {
        self.root.clone()
    }

    fn info(&self) -> FsInfo {
        FsInfo { block_size: radix_abi::address_space::PAGE_SIZE as u32, ..FsInfo::default() }
    }

    // The last description of an end wakes the other end's waiters, which
    // must not happen with the capability store locked: dropped later.
    fn light_drops(&self) -> bool {
        false
    }
}

/// The mount pipes' ends are on, made when first needed; sockets'
/// (`socket`) are on it too.
pub(super) fn mount() -> radix_vfs::Result<MountRef> {
    static MOUNT: Once<MountRef> = Once::new();
    if let Some(mount) = MOUNT.get() {
        return Ok(mount.clone());
    }
    let root: NodeRef = FixedDir::new(next_id())?;
    let fs: FsRef = mem::arc(PipeFs { root })?;
    let mount = radix_vfs::mount::Mount::detached(fs, "pipefs")?;
    Ok(match MOUNT.set(mount) {
        Ok(mount) => mount.clone(),
        Err(mount) => mount,
    })
}

/// Makes a pipe for `user`'s process: a description of its reading end
/// in slot `read` and of its writing end in slot `write`, with `flags`.
pub fn make(
    user: &crate::user::UserRef,
    read: u32,
    write: u32,
    flags: PipeFlags,
) -> Result<(), Status> {
    if read == write {
        return Err(Kernel::InvalidArgument.status(1));
    }
    let mount = mount().map_err(super::status)?;
    let pipe = Pipe::new()?;
    let description =
        Description { non_blocking: flags.contains(PipeFlags::NON_BLOCKING), append: false };
    let end = |write: bool| -> Result<FileRef, Status> {
        let node: NodeRef = mem::arc(End { pipe: pipe.clone(), write, reads_too: false })
            .map_err(|_| Kernel::NoMemory.status(0))?;
        let at = Location { mount: mount.clone(), node };
        FileObject::with(at, description).ok_or(Kernel::NoMemory.status(0))
    };
    // Each end counts as open from here: if a step fails, its node goes,
    // and the other end sees it gone.
    let reader = end(false)?;
    let writer = end(true)?;
    let close_on_exec = flags.contains(PipeFlags::CLOSE_ON_EXEC);
    let cspace = user.process.cspace();
    let base = Rights::COPY.union(Rights::TRANSFER);
    crate::user::files::install_file(
        user,
        SlotRef::new(cspace, read),
        ObjectKind::File,
        base.union(Rights::READ),
        reader,
        close_on_exec,
    )?;
    if let Err(err) = crate::user::files::install_file(
        user,
        SlotRef::new(cspace, write),
        ObjectKind::File,
        base.union(Rights::WRITE),
        writer,
        close_on_exec,
    ) {
        let _ = crate::caps::delete(SlotRef::new(cspace, read));
        return Err(err);
    }
    Ok(())
}

/// Makes a pipe the kernel writes (`trace`): the pipe, whose writing end
/// is the kernel's to close with [`Pipe::close_writer`], and a
/// description of its reading end.
pub(super) fn for_kernel() -> Result<(KArc<Pipe>, FileRef), Status> {
    let mount = mount().map_err(super::status)?;
    let pipe = Pipe::new()?;
    let node: NodeRef = mem::arc(End { pipe: pipe.clone(), write: false, reads_too: false })
        .map_err(|_| Kernel::NoMemory.status(0))?;
    let reader = FileObject::new(Location { mount, node }).ok_or(Kernel::NoMemory.status(0))?;
    Ok((pipe, reader))
}

/// Opens the named pipe `at`, a filesystem's node of the pipe kind, to
/// read, to write, or both (ADR 0135): an end of the one pipe its opens
/// share while any is open, which starts empty. As POSIX has it, an open
/// only to read waits until a writer opens, and one only to write until a
/// reader does, unless `non_blocking`, when one to write that finds no
/// reader fails with no_device (ENXIO); one to read and write waits for
/// nothing.
pub fn open_named(
    at: &Location,
    read: bool,
    write: bool,
    non_blocking: bool,
) -> Result<NodeRef, Status> {
    let key = radix_vfs::address(&at.node);
    let pipe = {
        let mut named = NAMED.lock();
        let named = named.get_or_insert_with(|| Vec::new_in(heap()));
        named.retain(|(_, pipe)| pipe.strong_count() > 0);
        match named.iter().find(|(k, _)| *k == key).and_then(|(_, pipe)| pipe.upgrade()) {
            Some(pipe) => pipe,
            None => {
                let pipe = Pipe::with_ends(0, Some(at.node.clone()))?;
                named.try_reserve(1).map_err(|_| Kernel::NoMemory.status(0))?;
                let _ = named.push_within_capacity((key, KArc::downgrade(&pipe)));
                pipe
            }
        }
    };
    let (other, mine) = if write { (&pipe.readers, 1) } else { (&pipe.writers, 0) };
    if write && !read && non_blocking && other.load(Ordering::Acquire) == 0 {
        return Err(FileError::NoDevice.status(0));
    }
    // Counted before the wait, so that an open of the other kind waiting
    // meanwhile sees it.
    if write {
        pipe.writers.fetch_add(1, Ordering::AcqRel);
    }
    if read {
        pipe.readers.fetch_add(1, Ordering::AcqRel);
    }
    pipe.opened[mine].fetch_add(1, Ordering::AcqRel);
    let end = mem::arc(End { pipe: pipe.clone(), write, reads_too: read && write });
    let end: NodeRef = match end {
        Ok(end) => end,
        Err(_) => {
            // No end holds the counts: they go back as one dropped would.
            drop(End { pipe, write, reads_too: read && write });
            return Err(Kernel::NoMemory.status(0));
        }
    };
    pipe.changed();
    if read != write && !non_blocking {
        let theirs = &pipe.opened[1 - mine];
        let seen = theirs.load(Ordering::Acquire);
        wait::wait_until(pipe.key(), "fifo open", || {
            other.load(Ordering::Acquire) > 0
                || theirs.load(Ordering::Acquire) != seen
                || sched::interrupted()
        });
        if other.load(Ordering::Acquire) == 0 && theirs.load(Ordering::Acquire) == seen {
            return Err(interrupted(Restart::Always));
        }
    }
    Ok(end)
}

/// The pipe end `node` is, if it is one.
pub fn end_of(node: &NodeRef) -> Option<&End> {
    radix_vfs::downcast::<End>(&**node)
}
