// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Sockets (Phase 9): POSIX's stream sockets of the AF_UNIX family. Two
//! connected ones (radix-process(2)'s `socket_pair`), as `socketpair`
//! makes; and one alone (`socket`), which can be given a name in the tree
//! (radix-directory(2)'s `bind_socket`), connect to a name
//! (`connect_socket`), or listen (radix-file(2)'s `listen`) and take the
//! connections made to its name (`accept`).
//!
//! A connection is two pipes ([`Pipe`]), one each way. Each [`End`] reads
//! one and writes the other, and counts as that pipe's reader and this
//! one's writer, so the pipes' rules carry over: a read waits for data,
//! and returns 0 once the other end is gone or shut for writing; a write
//! waits for room, and once the other end is gone or shut for reading
//! fails with gone, raising SIGPIPE unless the send asked not to. Shutting
//! an end down lets go of its part of one pipe or both early, once for
//! every description of the end, as POSIX's `shutdown` does. Files sent
//! with `send_files` ride on the pipe's bytes (`pipe`).
//!
//! A [`Socket`] is a node on the pipes' detached filesystem, so its
//! descriptions are copied, passed and inherited as a pipe's are (ADR
//! 0085). It starts fresh and moves on once: to listening, or to
//! connected, when it gets an end. Its name is a node of the tree that
//! holds a [`Binding`], through which a connecting socket finds the
//! socket's [`Listener`] while it listens: the connection's pipes are made
//! then, and the listening side's end waits in the listener's queue until
//! `accept` gives it a socket of its own.
//!
//! A socket cannot be sent with `send_files`, so the files riding on a
//! connection never hold a socket, and no socket can keep itself alive
//! through its own connection: there are no cycles for a collector to
//! find, as Linux's must (ADR 0091). Network sockets come with Phase 12.

use core::any::Any;
use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use radix_abi::Status;
use radix_abi::errors::Kernel;
use radix_abi::file::{BACKLOG_MAX, SOCKET_NAME_MAX};
use radix_abi::process::PipeFlags;
use radix_cap::{ObjectKind, Rights, SlotRef};
use radix_sync::{Mutex, Once, SpinLock};
use radix_vfs::mount::{Location, Vfs};
use radix_vfs::{In, KArc, Kind, Node, NodeRef, Out, SocketBinding, Stat, mem, next_id};

use super::pipe::{self, Files, Pipe};
use super::{Description, FileObject, FileRef};
use crate::sched::{self, wait};
use crate::user::Fifo;
use crate::user::posix::poll::{self, Polled};
use crate::user::posix::signal::{Restart, interrupted};

/// A socket's name: the path `bind_socket` was given, as the program gave
/// it to `bind`, of at most SOCKET_NAME_MAX bytes; empty for none.
#[derive(Clone, Copy)]
pub struct Name {
    len: u8,
    bytes: [u8; SOCKET_NAME_MAX as usize],
}

impl Name {
    /// No name.
    pub const NONE: Name = Name { len: 0, bytes: [0; SOCKET_NAME_MAX as usize] };

    /// `bytes` as a name, unless it is too long for one.
    pub fn of(bytes: &[u8]) -> Option<Name> {
        let mut name = Name::NONE;
        name.bytes.get_mut(..bytes.len())?.copy_from_slice(bytes);
        name.len = bytes.len() as u8;
        Some(name)
    }

    /// Its bytes.
    pub fn bytes(&self) -> &[u8] {
        &self.bytes[..self.len as usize]
    }
}

/// One end of a connection.
pub struct End {
    /// The pipe it reads, which the other end writes.
    rx: KArc<Pipe>,
    /// The pipe it writes, which the other end reads.
    tx: KArc<Pipe>,
    /// Whether it has been shut for reading, and for writing: its part of
    /// `rx` or `tx` has gone.
    shut_read: AtomicBool,
    shut_write: AtomicBool,
    /// The name of the socket at the other end, and the number of its
    /// process, as they were when the connection was made.
    peer_name: Name,
    peer_process: u64,
}

impl End {
    /// The ends of a new connection: the first's peer is `first_peer`,
    /// a name and a process's number, and the second's `second_peer`.
    fn pair(first_peer: (Name, u64), second_peer: (Name, u64)) -> Result<(End, End), Status> {
        // Each pipe starts with one reader and one writer, which the ends
        // are from here.
        let (one, other) = (Pipe::new()?, Pipe::new()?);
        let end = |rx: &KArc<Pipe>, tx: &KArc<Pipe>, (peer_name, peer_process)| End {
            rx: rx.clone(),
            tx: tx.clone(),
            shut_read: AtomicBool::new(false),
            shut_write: AtomicBool::new(false),
            peer_name,
            peer_process,
        };
        Ok((end(&one, &other, first_peer), end(&other, &one, second_peer)))
    }

    /// Receives up to `out.len()` bytes into `out`, waiting for some
    /// unless `non_blocking`, with the files that came with them; returns
    /// 0 once the other end has stopped writing and all it wrote has been
    /// read.
    pub fn receive(
        &self,
        out: &mut dyn Out,
        non_blocking: bool,
    ) -> Result<(usize, Option<Files>), Status> {
        if self.shut_read.load(Ordering::Acquire) {
            return Ok((0, None));
        }
        self.rx.read_files(out, non_blocking)
    }

    /// Sends `data`, with `files` riding on its first byte, waiting for
    /// room unless `non_blocking`; fails with gone, after SIGPIPE if
    /// `signal`, once the other end has stopped reading or this one was
    /// shut for writing.
    pub fn send(
        &self,
        data: &dyn In,
        files: Option<Files>,
        non_blocking: bool,
        signal: bool,
    ) -> Result<usize, Status> {
        if self.shut_write.load(Ordering::Acquire) {
            return pipe::broken(0, signal);
        }
        self.tx.write_files(data, files, non_blocking, signal)
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

    /// The number of the process at the other end.
    pub fn peer_process(&self) -> u64 {
        self.peer_process
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

/// What takes a listening socket's connections.
pub struct Listener {
    /// The listening sides of the connections that wait for `accept`.
    queue: Mutex<Fifo<End>>,
    /// How many wait, as the queue says, for the waits.
    waiting: AtomicUsize,
    /// How many may wait at once.
    backlog: AtomicUsize,
    /// Whether the socket has gone.
    closed: AtomicBool,
    /// The socket's name, and the number of its process, as they were
    /// when it began to listen.
    name: Name,
    process: u64,
    /// Its pollers.
    polled: Polled,
}

impl Listener {
    fn key(&self) -> usize {
        core::ptr::from_ref::<Listener>(self) as usize
    }

    /// Something changed: the waits look again, and so do its pollers.
    fn changed(&self) {
        wait::wake_all(self.key());
        self.polled.changed();
    }

    /// Queues `end`, the listening side of a new connection, for
    /// `accept`, waiting for room unless `non_blocking`; fails with
    /// refused once the socket has gone.
    fn enqueue(&self, end: End, non_blocking: bool) -> Result<(), Status> {
        loop {
            {
                let mut queue = self.queue.lock();
                if self.closed.load(Ordering::Acquire) {
                    return Err(Kernel::Refused.status(0));
                }
                if queue.len() < self.backlog.load(Ordering::Acquire) {
                    queue.push_back(end).map_err(|_| Kernel::NoMemory.status(0))?;
                    self.waiting.store(queue.len(), Ordering::Release);
                    drop(queue);
                    self.changed();
                    return Ok(());
                }
            }
            if non_blocking {
                return Err(Kernel::WouldBlock.status(0));
            }
            wait::wait_until(self.key(), "socket connect", || {
                self.waiting.load(Ordering::Acquire) < self.backlog.load(Ordering::Acquire)
                    || self.closed.load(Ordering::Acquire)
                    || sched::interrupted()
            });
            if sched::interrupted() {
                return Err(interrupted(Restart::Always));
            }
        }
    }

    /// Takes the connection that has waited longest, waiting for one
    /// unless `non_blocking`.
    fn accept(&self, non_blocking: bool) -> Result<End, Status> {
        loop {
            {
                let mut queue = self.queue.lock();
                if let Some(end) = queue.pop_front() {
                    self.waiting.store(queue.len(), Ordering::Release);
                    drop(queue);
                    self.changed();
                    return Ok(end);
                }
            }
            if non_blocking {
                return Err(Kernel::WouldBlock.status(0));
            }
            wait::wait_until(self.key(), "socket accept", || {
                self.waiting.load(Ordering::Acquire) > 0 || sched::interrupted()
            });
            if sched::interrupted() && self.waiting.load(Ordering::Acquire) == 0 {
                return Err(interrupted(Restart::Always));
            }
        }
    }

    /// Puts `end`, which an accept took but could not give a socket, back
    /// at the front of the queue for the next accept; it goes if the
    /// socket has gone meanwhile, or if there is no memory to keep it.
    fn give_back(&self, end: End) {
        let lost = {
            let mut queue = self.queue.lock();
            if self.closed.load(Ordering::Acquire) {
                Some(end)
            } else {
                let lost = queue.push_front(end).err();
                self.waiting.store(queue.len(), Ordering::Release);
                lost
            }
        };
        // Outside the lock, as the other end then sees the end.
        drop(lost);
        self.changed();
    }

    /// The socket has gone: the connections that wait go too, and the
    /// sockets that connected see the end.
    fn close(&self) {
        let waiting = {
            let mut queue = self.queue.lock();
            self.closed.store(true, Ordering::Release);
            self.waiting.store(0, Ordering::Release);
            queue.take()
        };
        drop(waiting);
        self.changed();
    }
}

/// What a socket's name holds: the socket's listener, while it listens.
pub struct Binding {
    listener: SpinLock<Option<KArc<Listener>>>,
}

/// Where a socket is in its life.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Phase {
    /// Neither listening nor connected.
    Fresh,
    /// Being named or connected, by a call under way.
    Busy,
    /// Taking connections.
    Listening,
    /// Connected: it has an end.
    Connected,
}

/// What a socket keeps under its lock.
struct State {
    phase: Phase,
    /// Its name: the one it was given, or for one that `accept` made, the
    /// listening socket's.
    name: Name,
    /// What its name holds, once it was given one.
    binding: Option<KArc<Binding>>,
}

/// A socket, as a node.
pub struct Socket {
    /// Its number, which its node shows.
    id: u64,
    state: SpinLock<State>,
    /// Its end of a connection, once it is connected.
    end: Once<End>,
    /// What takes its connections, once it listens.
    listener: Once<KArc<Listener>>,
}

impl Socket {
    fn new(phase: Phase, name: Name) -> Socket {
        Socket {
            id: next_id(),
            state: SpinLock::new(State { phase, name, binding: None }),
            end: Once::new(),
            listener: Once::new(),
        }
    }

    /// A connected socket, with `end`.
    fn connected_with(end: End, name: Name) -> Socket {
        let socket = Socket::new(Phase::Connected, name);
        // It is new, so its end is not set.
        let _ = socket.end.set(end);
        socket
    }

    /// Its end of a connection, if it is connected.
    pub fn end(&self) -> Option<&End> {
        self.end.get()
    }

    /// Its end of a connection; not_connected if it is not.
    pub fn connected(&self) -> Result<&End, Status> {
        self.end().ok_or(Kernel::NotConnected.status(0))
    }

    /// Receives as [`End::receive`] does, closing the files that came.
    pub fn receive(&self, out: &mut dyn Out, non_blocking: bool) -> Result<usize, Status> {
        self.connected()?.receive(out, non_blocking).map(|(read, _)| read)
    }

    /// Sends as [`End::send`] does, without files.
    pub fn send(&self, data: &dyn In, non_blocking: bool, signal: bool) -> Result<usize, Status> {
        self.connected()?.send(data, None, non_blocking, signal)
    }

    /// How many bytes wait to be received.
    pub fn pending(&self) -> usize {
        self.end().map_or(0, End::pending)
    }

    /// Whether it listens.
    pub fn listens(&self) -> bool {
        self.listener.get().is_some()
    }

    /// Its name, or with `peer` the name of the socket at the other end
    /// of its connection.
    pub fn name(&self, peer: bool) -> Result<Name, Status> {
        if peer {
            return Ok(self.connected()?.peer_name);
        }
        Ok(self.state.lock().name)
    }

    /// The events `poll` finds on it: a connected one's are its end's; a
    /// listening one is readable while a connection waits; one that is
    /// neither is writable and hung up, as on Linux.
    pub fn events(&self) -> u16 {
        if let Some(end) = self.end() {
            end.events()
        } else if let Some(listener) = self.listener.get() {
            if listener.waiting.load(Ordering::Acquire) > 0 { poll::READABLE } else { 0 }
        } else {
            poll::WRITABLE | poll::HANG_UP
        }
    }

    /// Calls `each` with what its pollers count themselves in, as
    /// [`Socket::events`] looks: both pipes of its connection, or its
    /// listener.
    pub fn polled(&self, each: &mut dyn FnMut(&Polled)) {
        if let Some(end) = self.end() {
            each(end.rx.polled());
            each(end.tx.polled());
        } else if let Some(listener) = self.listener.get() {
            each(&listener.polled);
        }
    }
}

impl Drop for Socket {
    fn drop(&mut self) {
        let binding = self.state.lock().binding.take();
        if let Some(listener) = self.listener.get() {
            // No socket finds it through the name from here.
            if let Some(binding) = &binding {
                *binding.listener.lock() = None;
            }
            listener.close();
        }
    }
}

impl Node for Socket {
    fn stat(&self) -> Stat {
        Stat {
            kind: Kind::Socket,
            size: self.pending() as u64,
            allocated: 0,
            id: self.id,
            created: 0,
            modified: 0,
            accessed: 0,
            changed: 0,
            links: 1,
            device: 0,
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

/// A description of `socket`, with `flags`.
fn file(socket: Socket, flags: PipeFlags) -> Result<FileRef, Status> {
    let mount = pipe::mount().map_err(super::status)?;
    let node: NodeRef = mem::arc(socket).map_err(|_| Kernel::NoMemory.status(0))?;
    let description =
        Description { non_blocking: flags.contains(PipeFlags::NON_BLOCKING), append: false };
    FileObject::with(Location { mount, node }, description).ok_or(Kernel::NoMemory.status(0))
}

/// Puts a capability to `file`, a socket's description, in `user`'s slot
/// `slot`, closed by exec if `flags` say so.
fn install(
    user: &crate::user::UserRef,
    slot: u32,
    file: FileRef,
    flags: PipeFlags,
) -> Result<(), Status> {
    let rights = Rights::COPY.union(Rights::TRANSFER).union(Rights::READ).union(Rights::WRITE);
    crate::user::files::install_file(
        user,
        SlotRef::new(user.process.cspace(), slot),
        ObjectKind::File,
        rights,
        file,
        flags.contains(PipeFlags::CLOSE_ON_EXEC),
    )
}

/// Makes a socket pair for `user`'s process: a description of one end in
/// slot `first` and of the other in slot `second`, with `flags`.
pub fn make_pair(
    user: &crate::user::UserRef,
    first: u32,
    second: u32,
    flags: PipeFlags,
) -> Result<(), Status> {
    if first == second {
        return Err(Kernel::InvalidArgument.status(1));
    }
    // Each end's peer is the process that made the pair.
    let process = user.process.number();
    let (one, other) = End::pair((Name::NONE, process), (Name::NONE, process))?;
    // If a step fails, the sockets made go, and with them their ends.
    let one = file(Socket::connected_with(one, Name::NONE), flags)?;
    let other = file(Socket::connected_with(other, Name::NONE), flags)?;
    install(user, first, one, flags)?;
    if let Err(err) = install(user, second, other, flags) {
        let _ = crate::caps::delete(SlotRef::new(user.process.cspace(), first));
        return Err(err);
    }
    Ok(())
}

/// Makes a socket for `user`'s process, neither named nor connected, in
/// slot `to`, with `flags`.
pub fn make(user: &crate::user::UserRef, to: u32, flags: PipeFlags) -> Result<(), Status> {
    install(user, to, file(Socket::new(Phase::Fresh, Name::NONE), flags)?, flags)
}

/// Gives `socket` the name `path` beneath `dir` (radix-directory(2)'s
/// `bind_socket`), which it knows as `name`, with `access`.
pub fn bind(
    socket: &Socket,
    vfs: &Vfs,
    dir: &Location,
    path: &str,
    name: Name,
    access: radix_vfs::Access,
) -> Result<(), Status> {
    {
        let mut state = socket.state.lock();
        match state.phase {
            Phase::Fresh if state.binding.is_none() => state.phase = Phase::Busy,
            Phase::Busy => return Err(Kernel::Busy.status(0)),
            // Named already, listening or connected.
            _ => return Err(Kernel::InvalidArgument.status(0)),
        }
    }
    let made = mem::arc(Binding { listener: SpinLock::new(None) })
        .map_err(|_| Kernel::NoMemory.status(0))
        .and_then(|binding| {
            let held: SocketBinding = binding.clone();
            vfs.make_socket(dir, path, held, access).map_err(|err| match err {
                // FAT has no sockets' names: Linux says EPERM.
                radix_vfs::Error::NotSupported => Kernel::NotPermitted.status(0),
                err => super::status(err),
            })?;
            Ok(binding)
        });
    let mut state = socket.state.lock();
    state.phase = Phase::Fresh;
    state.binding = Some(made?);
    state.name = name;
    Ok(())
}

/// Has `socket`, which has a name and is not connected, take connections,
/// at most `backlog` waiting at once (radix-file(2)'s `listen`), for the
/// process numbered `process`. A socket that listens already takes the
/// new backlog.
pub fn listen(socket: &Socket, backlog: u32, process: u64) -> Result<(), Status> {
    let backlog = u64::from(backlog).clamp(1, BACKLOG_MAX) as usize;
    if let Some(listener) = socket.listener.get() {
        listener.backlog.store(backlog, Ordering::Release);
        listener.changed();
        return Ok(());
    }
    // Made before the lock is taken, as making it may wait for memory.
    let name = socket.state.lock().name;
    let listener = mem::arc(Listener {
        queue: Mutex::new(Fifo::new()),
        waiting: AtomicUsize::new(0),
        backlog: AtomicUsize::new(backlog),
        closed: AtomicBool::new(false),
        name,
        process,
        polled: Polled::new(),
    })
    .map_err(|_| Kernel::NoMemory.status(0))?;
    let mut state = socket.state.lock();
    let binding = match (state.phase, &state.binding) {
        (Phase::Fresh, Some(binding)) => binding.clone(),
        (Phase::Busy, _) => return Err(Kernel::Busy.status(0)),
        // Another thread made it listen meanwhile.
        (Phase::Listening, _) => return Ok(()),
        // Connected, or with no name, as POSIX and Linux have it.
        _ => return Err(Kernel::InvalidArgument.status(0)),
    };
    state.phase = Phase::Listening;
    // It was fresh, so no listener was set.
    let _ = socket.listener.set(listener.clone());
    *binding.listener.lock() = Some(listener);
    Ok(())
}

/// Connects `socket` to the socket listening at `named`, a node of the
/// tree (radix-directory(2)'s `connect_socket`), for the process numbered
/// `process`; waits for room in the listener's queue unless
/// `non_blocking`.
pub fn connect(
    socket: &Socket,
    named: &NodeRef,
    process: u64,
    non_blocking: bool,
) -> Result<(), Status> {
    let name = {
        let mut state = socket.state.lock();
        match state.phase {
            Phase::Fresh => state.phase = Phase::Busy,
            Phase::Busy => return Err(Kernel::Busy.status(0)),
            Phase::Connected => return Err(Kernel::Connected.status(0)),
            Phase::Listening => return Err(Kernel::InvalidArgument.status(0)),
        }
        state.name
    };
    let connected = connect_to(named, name, process, non_blocking).map(|end| {
        // It is busy, so no other call set its end.
        let _ = socket.end.set(end);
    });
    socket.state.lock().phase = if connected.is_ok() { Phase::Connected } else { Phase::Fresh };
    connected
}

/// The end of a new connection to the socket listening at `named`, from
/// a socket named `name` of the process numbered `process`.
fn connect_to(
    named: &NodeRef,
    name: Name,
    process: u64,
    non_blocking: bool,
) -> Result<End, Status> {
    let refused = || Kernel::Refused.status(0);
    let held = named.socket_binding().ok_or_else(refused)?;
    let binding = held.downcast_ref::<Binding>().ok_or_else(refused)?;
    let listener = binding.listener.lock().clone().ok_or_else(refused)?;
    let (end, theirs) = End::pair((listener.name, listener.process), (name, process))?;
    listener.enqueue(theirs, non_blocking)?;
    Ok(end)
}

/// Takes a connection that waits at `socket`, which listens, waiting for
/// one unless `non_blocking`, and puts a description of a new socket,
/// connected, with the listening one's name, in `user`'s slot `to`, with
/// `flags`. A connection that cannot go in the slot, as another thread
/// filled it first, goes back to the front of the queue, so none is lost.
pub fn accept(
    user: &crate::user::UserRef,
    socket: &Socket,
    to: u32,
    flags: PipeFlags,
    non_blocking: bool,
) -> Result<(), Status> {
    let listener = socket.listener.get().ok_or(Kernel::InvalidArgument.status(0))?;
    let end = listener.accept(non_blocking)?;
    // The new socket is busy until it is in the slot and has the end,
    // which can go back until then.
    let file = match file(Socket::new(Phase::Busy, listener.name), flags) {
        Ok(file) => file,
        Err(err) => {
            listener.give_back(end);
            return Err(err);
        }
    };
    if let Err(err) = install(user, to, file.clone(), flags) {
        listener.give_back(end);
        return Err(err);
    }
    let accepted = file.socket().ok_or(Kernel::NotSocket.status(0))?;
    // It is new and busy, so no other call set its end.
    let _ = accepted.end.set(end);
    accepted.state.lock().phase = Phase::Connected;
    Ok(())
}

/// The socket `node` is, if it is one.
pub fn socket_of(node: &NodeRef) -> Option<&Socket> {
    radix_vfs::downcast::<Socket>(&**node)
}
