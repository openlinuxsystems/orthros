// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Socket pairs (Phase 9, radix-process(2)'s `socket_pair`): two connected
//! stream sockets, as POSIX's `socketpair(AF_UNIX, SOCK_STREAM)` makes.
//!
//! A pair is two pipes ([`Pipe`]), one each way. Each end reads one and
//! writes the other, and counts as that pipe's reader and this one's
//! writer, so the pipes' rules carry over: a read waits for data, and
//! returns 0 once the other end is gone or shut for writing; a write
//! waits for room, and once the other end is gone or shut for reading
//! fails with gone, raising SIGPIPE unless the send asked not to. Shutting
//! an end down lets go of its part of one pipe or both early, once for
//! every description of the end, as POSIX's `shutdown` does. Each end is
//! a node on the pipes' detached filesystem, so its descriptions are
//! copied, passed and inherited as a pipe's are (ADR 0085).
//!
//! Named sockets, listening, and passing descriptors come with tmux, later
//! in Phase 9; network sockets with Phase 12.

use core::any::Any;
use core::sync::atomic::{AtomicBool, Ordering};

use radix_abi::Status;
use radix_abi::errors::Kernel;
use radix_abi::process::PipeFlags;
use radix_cap::{ObjectKind, Rights, SlotRef};
use radix_vfs::mount::Location;
use radix_vfs::{In, KArc, Kind, Node, NodeRef, Out, Stat, mem, next_id};

use super::pipe::{self, Pipe};
use super::{Description, FileObject, FileRef};
use crate::user::posix::poll;

/// One end of a socket pair, as a node.
pub struct End {
    /// The pipe it reads, which the other end writes.
    rx: KArc<Pipe>,
    /// The pipe it writes, which the other end reads.
    tx: KArc<Pipe>,
    /// The pair's number, which both ends' nodes show.
    id: u64,
    /// Whether it has been shut for reading, and for writing: its part of
    /// `rx` or `tx` has gone.
    shut_read: AtomicBool,
    shut_write: AtomicBool,
}

impl End {
    /// Receives up to `out.len()` bytes into `out`, waiting for some
    /// unless `non_blocking`; returns 0 once the other end has stopped
    /// writing and all it wrote has been read.
    pub fn receive(&self, out: &mut dyn Out, non_blocking: bool) -> Result<usize, Status> {
        if self.shut_read.load(Ordering::Acquire) {
            return Ok(0);
        }
        self.rx.read(out, non_blocking)
    }

    /// Sends `data`, waiting for room unless `non_blocking`; fails with
    /// gone, after SIGPIPE if `signal`, once the other end has stopped
    /// reading or this one was shut for writing.
    pub fn send(&self, data: &dyn In, non_blocking: bool, signal: bool) -> Result<usize, Status> {
        if self.shut_write.load(Ordering::Acquire) {
            return pipe::broken(0, signal);
        }
        self.tx.write(data, non_blocking, signal)
    }

    /// Shuts it for reading, for writing, or both: POSIX's `shutdown`.
    pub fn shutdown(&self, read: bool, write: bool) {
        if read && !self.shut_read.swap(true, Ordering::AcqRel) {
            self.rx.close_reader();
        }
        if write && !self.shut_write.swap(true, Ordering::AcqRel) {
            self.tx.close_writer();
        }
    }

    /// How many bytes wait to be received.
    pub fn pending(&self) -> usize {
        self.rx.len()
    }

    /// The events `poll` finds on it, as Linux's for a stream socket: it
    /// is readable while data waits or once a read would return the end,
    /// writable while [`pipe::ATOMIC`] bytes fit or once a write would
    /// fail, and hung up once nothing can come or go either way.
    pub fn events(&self) -> u16 {
        let read_done = self.shut_read.load(Ordering::Acquire) || self.rx.writers() == 0;
        let write_done = self.shut_write.load(Ordering::Acquire) || self.tx.readers() == 0;
        let mut events = 0;
        if self.rx.len() > 0 || read_done {
            events |= poll::READABLE;
        }
        if self.tx.has_room() || write_done {
            events |= poll::WRITABLE;
        }
        if read_done && write_done {
            events |= poll::HANG_UP;
        }
        events
    }
}

impl Drop for End {
    fn drop(&mut self) {
        self.shutdown(true, true);
    }
}

impl Node for End {
    fn stat(&self) -> Stat {
        Stat {
            kind: Kind::Socket,
            size: self.rx.len() as u64,
            allocated: 0,
            id: self.id,
            created: 0,
            modified: 0,
            accessed: 0,
        }
    }

    fn kind(&self) -> Kind {
        Kind::Socket
    }

    // Reads and writes through the node itself (the kernel's own) never
    // wait.
    fn read(&self, _offset: u64, out: &mut dyn Out) -> radix_vfs::Result<usize> {
        self.receive(out, true).map_err(|_| radix_vfs::Error::NotSupported)
    }

    fn write(&self, _offset: u64, data: &dyn In) -> radix_vfs::Result<usize> {
        self.send(data, true, true).map_err(|_| radix_vfs::Error::NotSupported)
    }

    fn set_size(&self, _size: u64) -> radix_vfs::Result<()> {
        Err(radix_vfs::Error::NotSupported)
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

/// Makes a socket pair for `user`'s process: a description of one end in
/// slot `first` and of the other in slot `second`, with `flags`.
pub fn make(
    user: &crate::user::UserRef,
    first: u32,
    second: u32,
    flags: PipeFlags,
) -> Result<(), Status> {
    if first == second {
        return Err(Kernel::InvalidArgument.status(1));
    }
    let mount = pipe::mount().map_err(super::status)?;
    let (one, other) = (Pipe::new()?, Pipe::new()?);
    let id = next_id();
    let description =
        Description { non_blocking: flags.contains(PipeFlags::NON_BLOCKING), append: false };
    let end = |rx: &KArc<Pipe>, tx: &KArc<Pipe>| -> Result<FileRef, Status> {
        let node: NodeRef = mem::arc(End {
            rx: rx.clone(),
            tx: tx.clone(),
            id,
            shut_read: AtomicBool::new(false),
            shut_write: AtomicBool::new(false),
        })
        .map_err(|_| Kernel::NoMemory.status(0))?;
        let at = Location { mount: mount.clone(), node };
        FileObject::with(at, description).ok_or(Kernel::NoMemory.status(0))
    };
    // Each pipe starts with one reader and one writer, which the ends'
    // nodes are from here: if a step fails, the nodes made go, and with
    // them the counts.
    let a = end(&one, &other)?;
    let b = end(&other, &one)?;
    let close_on_exec = flags.contains(PipeFlags::CLOSE_ON_EXEC);
    let cspace = user.process.cspace();
    let rights = Rights::COPY.union(Rights::TRANSFER).union(Rights::READ).union(Rights::WRITE);
    let install = |slot: u32, file: FileRef| {
        crate::user::files::install_file(
            user,
            SlotRef::new(cspace, slot),
            ObjectKind::File,
            rights,
            file,
            close_on_exec,
        )
    };
    install(first, a)?;
    if let Err(err) = install(second, b) {
        let _ = crate::caps::delete(SlotRef::new(cspace, first));
        return Err(err);
    }
    Ok(())
}

/// The socket end `node` is, if it is one.
pub fn end_of(node: &NodeRef) -> Option<&End> {
    radix_vfs::downcast::<End>(&**node)
}
