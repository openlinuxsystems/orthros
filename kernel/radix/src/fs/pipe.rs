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

use core::any::Any;
use core::sync::atomic::{AtomicU32, AtomicUsize, Ordering};

use radix_abi::Status;
use radix_abi::errors::Kernel;
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
use crate::user::posix::poll;
use crate::user::posix::signal::{Restart, interrupted};

/// How many bytes a pipe holds, as on Linux.
pub const CAPACITY: usize = 65536;

/// The most bytes a write puts in at once, unmixed with others: POSIX's
/// PIPE_BUF.
pub const ATOMIC: usize = 4096;

/// A pipe.
pub struct Pipe {
    /// Its buffer.
    buffer: Mutex<Ring<alloc::vec::Vec<u8, Kmalloc>>>,
    /// How many bytes it holds, as the buffer says, for the waits.
    len: AtomicUsize,
    /// How many reading and writing ends are open.
    readers: AtomicU32,
    writers: AtomicU32,
    /// Its number, which both ends' nodes show.
    id: u64,
}

impl Pipe {
    fn key(&self) -> usize {
        self as *const Pipe as usize
    }

    /// Something changed: the waits look again, and so does `poll`.
    fn changed(&self) {
        wait::wake_all(self.key());
        poll::changed();
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

    /// Reads up to `out.len()` bytes into `out`, waiting for some unless
    /// `non_blocking`; returns 0 once no writer is left.
    pub fn read(&self, out: &mut dyn Out, non_blocking: bool) -> Result<usize, Status> {
        let want = out.len();
        if want == 0 {
            return Ok(0);
        }
        loop {
            {
                let mut buffer = self.buffer.lock();
                if !buffer.is_empty() {
                    let mut at = 0;
                    let read = buffer.read_with(want, &mut |run| {
                        out.put(at, run)?;
                        at += run.len();
                        Ok::<(), radix_vfs::Error>(())
                    });
                    self.len.store(buffer.len(), Ordering::Release);
                    drop(buffer);
                    self.changed();
                    return read.map_err(super::status);
                }
                if self.writers.load(Ordering::Acquire) == 0 {
                    return Ok(0);
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
    /// some had.
    pub fn write(&self, data: &dyn In, non_blocking: bool) -> Result<usize, Status> {
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
                    return broken(done);
                }
                let left = total - done;
                let free = buffer.free();
                if (atomic && free >= left) || (!atomic && free > 0) {
                    let mut at = done;
                    let wrote = buffer.write_with(left, &mut |run| {
                        data.get(at, run)?;
                        at += run.len();
                        Ok::<(), radix_vfs::Error>(())
                    });
                    self.len.store(buffer.len(), Ordering::Release);
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
            let room = if atomic { total - done } else { 1 };
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

/// A write found no reader: SIGPIPE for the writer, and gone (unless some
/// of it went in first).
fn broken(done: usize) -> Result<usize, Status> {
    crate::user::posix::signal::raise_current(SIGPIPE);
    if done > 0 { Ok(done) } else { Err(Kernel::Gone.status(0)) }
}

/// One end of a pipe, as a node.
pub struct End {
    pipe: KArc<Pipe>,
    /// Whether it is the writing end.
    pub write: bool,
}

impl End {
    /// The pipe.
    pub fn pipe(&self) -> &Pipe {
        &self.pipe
    }
}

impl Drop for End {
    fn drop(&mut self) {
        let count = if self.write { &self.pipe.writers } else { &self.pipe.readers };
        count.fetch_sub(1, Ordering::AcqRel);
        self.pipe.changed();
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
        self.pipe.write(data, true).map_err(|_| radix_vfs::Error::NotSupported)
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

/// The mount pipes' ends are on, made when first needed.
fn mount() -> radix_vfs::Result<MountRef> {
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
    let mut buffer = alloc::vec::Vec::new_in(radix_mm::HEAP.with(radix_mm::Flags::MAY_SLEEP));
    buffer.try_reserve_exact(CAPACITY).map_err(|_| Kernel::NoMemory.status(0))?;
    while buffer.len() < CAPACITY {
        // Room was reserved above.
        let _ = buffer.push_within_capacity(0);
    }
    let pipe = mem::arc(Pipe {
        buffer: Mutex::new(Ring::new(buffer)),
        len: AtomicUsize::new(0),
        readers: AtomicU32::new(1),
        writers: AtomicU32::new(1),
        id: next_id(),
    })
    .map_err(|_| Kernel::NoMemory.status(0))?;
    let description =
        Description { non_blocking: flags.contains(PipeFlags::NON_BLOCKING), append: false };
    let end = |write: bool| -> Result<FileRef, Status> {
        let node: NodeRef =
            mem::arc(End { pipe: pipe.clone(), write }).map_err(|_| Kernel::NoMemory.status(0))?;
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

/// The pipe end `node` is, if it is one.
pub fn end_of(node: &NodeRef) -> Option<&End> {
    radix_vfs::downcast::<End>(&**node)
}
