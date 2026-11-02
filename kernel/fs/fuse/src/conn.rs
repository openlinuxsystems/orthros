// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! A connection: the requests between a mount and the program serving it.
//!
//! The program opens `/devices/fuse`, which makes a [`Connection`], and
//! mounts it; then it reads requests from the description and writes
//! answers to it. A call of the mount's nodes queues a request and waits
//! for its answer; the program's `read` takes the oldest request and
//! keeps it, by its number, until the program's `write` answers it, which
//! wakes the call. The program may answer in any order, from any number
//! of threads.
//!
//! The first request is `INIT`, queued at the mount; calls wait for its
//! answer, which says the protocol version and how big a write may be.
//!
//! A signal that comes while a call waits sends `INTERRUPT` for its
//! request, at once if the program has read it, or else as soon as it
//! does, and the call waits on for the answer, which the program may make
//! EINTR: only then does the call fail as interrupted. A thread that is
//! being stopped never waits for a program (a process that is killed, or
//! that a signal ends): its request is taken back if the program has not
//! read it, and abandoned if it has, its answer thrown away when it
//! comes; an answer that names a node the kernel never took is forgotten
//! at once, so that the program's count of the node's names stays right.
//! This is how Linux's FUSE treats signals.
//!
//! When the program's last description goes, the connection is lost:
//! every waiting call fails as not connected (ENOTCONN), and so does each
//! later one. When the mount goes, the program's reads fail with no
//! device (ENODEV), which tells it to end, as on Linux.
//!
//! Waits use [`crate::Hooks`]; their conditions read only atomics, as the
//! kernel's wait table asks.

use core::sync::atomic::{AtomicBool, AtomicU8, AtomicU32, AtomicUsize, Ordering};

use radix_sync::Mutex;
use radix_vfs::mem::{self, KArc, KVec};
use radix_vfs::{Error, In, Out, Result};

use crate::abi::{self, InHeader, InitOut, OutHeader, Reader, Writer, op};
use crate::{Wake, hooks};

/// The smallest buffer a program may read requests into, as on Linux.
pub const MIN_READ_BUFFER: usize = 8192;

/// The most bytes of data one `READ` or `WRITE` carries: 256 pages.
pub const MAX_DATA: u32 = 1 << 20;

/// The bytes a request has room for besides its data: its header, the
/// operation's structure and two names.
const REQUEST_ROOM: usize = abi::IN_HEADER + 64 + 2 * 4096;

/// What may go to the program in one request at most, data included:
/// what it must be able to read.
const MAX_REQUEST: usize = REQUEST_ROOM + MAX_DATA as usize;

/// The longest answer taken: a read's data, a symbolic link's target or
/// an extended attribute's value, and the header.
const MAX_ANSWER: usize = abi::OUT_HEADER + MAX_DATA as usize + 4096;

/// What answering an operation's request makes of the answer.
enum Answer {
    /// Nothing: no answer comes (`FORGET`, `INTERRUPT`).
    None,
    /// It is thrown away (`RELEASE`, sent as a node goes).
    Dropped,
    /// It sets the connection up ([`op::INIT`]).
    Init,
    /// It goes to the call waiting in the slot.
    To(KArc<Slot>),
}

/// Where a call's answer goes.
struct Slot {
    /// The operation, which says what an abandoned answer holds.
    opcode: u32,
    /// Whether a signal asked for `INTERRUPT` ([`Interrupt`]).
    interrupt: AtomicU8,
    /// Whether the answer has come (or the connection was lost): what
    /// the call waits for.
    done: AtomicBool,
    /// Whether the call no longer waits: its answer is thrown away.
    abandoned: AtomicBool,
    /// The answer's data, after its header, or the error it makes.
    answer: Mutex<Option<Result<KVec<u8>>>>,
}

impl Slot {
    fn key(&self) -> usize {
        core::ptr::from_ref(&self.done).addr()
    }

    /// Puts the answer in, unless one is there, and wakes the call.
    fn answer(&self, answer: Result<KVec<u8>>) {
        {
            let mut held = self.answer.lock();
            if held.is_some() {
                return;
            }
            *held = Some(answer);
        }
        self.done.store(true, Ordering::Release);
        (hooks().wake)(self.key());
    }
}

/// A request waiting for the program to read it.
struct Queued {
    unique: u64,
    bytes: KVec<u8>,
    answer: Answer,
}

/// A request the program has read, waiting for its answer.
struct Sent {
    unique: u64,
    answer: Answer,
}

/// What the lock guards.
struct State {
    /// The requests to read, oldest first.
    queue: KVec<Queued>,
    /// The requests read, waiting for answers.
    sent: KVec<Sent>,
    /// The next request's number.
    next_unique: u64,
}

/// Why a connection ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
enum Ended {
    No = 0,
    /// The program's last description went.
    Lost = 1,
    /// The mount went.
    Unmounted = 2,
}

/// Where a call's `INTERRUPT` stands: changed under the state's lock.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
enum Interrupt {
    No = 0,
    /// A signal came before the program read the request: whoever reads
    /// it sends `INTERRUPT` after it.
    Wanted = 1,
    /// Sent.
    Sent = 2,
}

/// Whether `INIT` has been answered.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
enum Init {
    Waiting = 0,
    Done = 1,
    Refused = 2,
}

/// Why the program's `read` or `write` of a connection failed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DeviceError {
    /// Nothing to read, and the description does not wait (EAGAIN).
    WouldBlock,
    /// A signal came while it waited (EINTR).
    Interrupted,
    /// The mount has gone, or never was and the connection is lost
    /// (ENODEV): the program ends.
    NoDevice,
    /// The buffer is too small, or the answer is not one (EINVAL).
    Invalid,
    /// No request waits for that answer (ENOENT).
    NotFound,
    /// The buffer is not the program's memory (EFAULT).
    BadAddress,
    /// The kernel has no memory for the answer (ENOMEM).
    NoMemory,
}

/// A connection.
pub struct Connection {
    state: Mutex<State>,
    /// How many requests wait to be read, as the queue holds them.
    queued: AtomicUsize,
    /// Whether, and why, it has ended ([`Ended`]).
    ended: AtomicU8,
    /// Whether `INIT` has been answered ([`Init`]).
    init: AtomicU8,
    /// Whether it has been mounted: once only.
    mounted: AtomicBool,
    /// The protocol's minor version agreed.
    minor: AtomicU32,
    /// The most data one `WRITE` or `READ` carries.
    max_data: AtomicU32,
}

impl Connection {
    /// A connection, which nothing has mounted yet.
    pub fn new() -> Result<KArc<Connection>> {
        Ok(mem::arc(Connection {
            state: Mutex::new(State { queue: mem::vec(), sent: mem::vec(), next_unique: 1 }),
            queued: AtomicUsize::new(0),
            ended: AtomicU8::new(Ended::No as u8),
            init: AtomicU8::new(Init::Waiting as u8),
            mounted: AtomicBool::new(false),
            minor: AtomicU32::new(abi::MINOR),
            max_data: AtomicU32::new(4096),
        })?)
    }

    /// The key the program's reads wait on, and a poll looks at.
    pub fn key(&self) -> usize {
        core::ptr::from_ref(&self.queued).addr()
    }

    fn init_key(&self) -> usize {
        core::ptr::from_ref(&self.init).addr()
    }

    fn ended(&self) -> Ended {
        match self.ended.load(Ordering::Acquire) {
            0 => Ended::No,
            1 => Ended::Lost,
            _ => Ended::Unmounted,
        }
    }

    /// Whether a read would take a request now, or fail at once.
    pub fn readable(&self) -> bool {
        self.queued.load(Ordering::Acquire) > 0 || self.ended() != Ended::No
    }

    /// Whether it has ended: its program lost, or its mount gone.
    pub fn gone(&self) -> bool {
        self.ended() != Ended::No
    }

    /// The protocol's minor version agreed.
    pub fn minor(&self) -> u32 {
        self.minor.load(Ordering::Acquire)
    }

    /// The most data one `READ` or `WRITE` carries.
    pub fn max_data(&self) -> u32 {
        self.max_data.load(Ordering::Acquire)
    }

    /// Takes the connection for a mount, and queues `INIT`: fails with
    /// [`Error::InUse`] if it was mounted before, and with
    /// [`Error::NotConnected`] if its program has gone.
    pub fn mount(&self) -> Result<()> {
        if self.mounted.swap(true, Ordering::AcqRel) {
            return Err(Error::InUse);
        }
        let mut w = Writer::with_capacity(abi::IN_HEADER + 16)?;
        header(&mut w, op::INIT, 0, 16, (0, 0, 0))?;
        w.u32(abi::MAJOR)?.u32(abi::MINOR)?;
        // Read-ahead: none is made, but a program may want a number.
        w.u32(128 * 1024)?
            .u32(abi::init::BIG_WRITES | abi::init::DONT_MASK | abi::init::MAX_PAGES)?;
        self.queue(w.0, Answer::Init, false).map(drop)
    }

    /// The mount went: the program's reads now fail with no device, and
    /// any call still waiting fails as not connected.
    pub fn unmount(&self) {
        self.end(Ended::Unmounted);
    }

    /// The program's last description went: every call fails as not
    /// connected from now on.
    pub fn lose(&self) {
        self.end(Ended::Lost);
    }

    fn end(&self, how: Ended) {
        if self
            .ended
            .compare_exchange(Ended::No as u8, how as u8, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            return;
        }
        let (queue, sent) = {
            let mut state = self.state.lock();
            let queue = core::mem::replace(&mut state.queue, mem::vec());
            let sent = core::mem::replace(&mut state.sent, mem::vec());
            if how == Ended::Lost {
                self.queued.store(0, Ordering::Release);
                (queue, sent)
            } else {
                // Unmounted: what nobody waits for is still the program's
                // to read and answer (its files to close, its names to
                // forget), before its reads fail; a call still waiting
                // fails now.
                state.queue = queue;
                state.sent = sent;
                state.queue.retain(|q| kept(&q.answer));
                state.sent.retain(|s| kept(&s.answer));
                self.queued.store(state.queue.len(), Ordering::Release);
                (mem::vec(), mem::vec())
            }
        };
        let answers = queue.into_iter().map(|q| q.answer).chain(sent.into_iter().map(|s| s.answer));
        for answer in answers {
            if let Answer::To(slot) = answer {
                slot.answer(Err(Error::NotConnected));
            }
        }
        let _ = self.init.compare_exchange(
            Init::Waiting as u8,
            Init::Refused as u8,
            Ordering::AcqRel,
            Ordering::Acquire,
        );
        (hooks().wake)(self.init_key());
        (hooks().wake)(self.key());
    }

    /// Queues a request, before the others if `urgent` (`INTERRUPT`, as
    /// Linux reads them first): returns the number it was given, or fails
    /// as not connected once the connection has ended.
    fn queue(&self, mut bytes: KVec<u8>, answer: Answer, urgent: bool) -> Result<u64> {
        let unique = {
            let mut state = self.state.lock();
            if self.gone() {
                return Err(Error::NotConnected);
            }
            state.queue.try_reserve(1).map_err(|_| Error::NoMemory)?;
            let unique = state.next_unique;
            state.next_unique += 1;
            bytes[8..16].copy_from_slice(&unique.to_le_bytes());
            let _ = state.queue.push_within_capacity(Queued { unique, bytes, answer });
            if urgent {
                state.queue.rotate_right(1);
            }
            self.queued.store(state.queue.len(), Ordering::Release);
            unique
        };
        (hooks().wake)(self.key());
        Ok(unique)
    }

    /// Waits until `INIT` has been answered: fails as not connected if it
    /// was refused or the connection ended first, and as interrupted if
    /// the thread is being stopped; other signals wait on, as on Linux.
    fn wait_ready(&self) -> Result<()> {
        loop {
            match self.init.load(Ordering::Acquire) {
                1 => return Ok(()),
                2 => return Err(Error::NotConnected),
                _ => {}
            }
            let init = &self.init;
            match (hooks().wait)(self.init_key(), &|| init.load(Ordering::Acquire) != 0, false) {
                Wake::Ready => {}
                Wake::Signal | Wake::Stop => return Err(Error::Interrupted),
            }
        }
    }

    /// Sends a request for `opcode` about node `nodeid`, with `parts`
    /// after its header and then `data`'s `len` bytes from `at`, made by
    /// `caller`'s user, group and process (or the running thread's), and
    /// waits for the answer: its data after the header, or the error it
    /// makes.
    pub fn call(
        &self,
        opcode: u32,
        nodeid: u64,
        caller: Option<(u32, u32)>,
        parts: &[&[u8]],
        data: Option<(&dyn In, usize, usize)>,
    ) -> Result<KVec<u8>> {
        self.wait_ready()?;
        let bytes = request(opcode, nodeid, caller, parts, data)?;
        let slot = mem::arc(Slot {
            opcode,
            interrupt: AtomicU8::new(Interrupt::No as u8),
            done: AtomicBool::new(false),
            abandoned: AtomicBool::new(false),
            answer: Mutex::new(None),
        })?;
        let unique = self.queue(bytes, Answer::To(slot.clone()), false)?;
        let done = &slot.done;
        let ready = || done.load(Ordering::Acquire);
        let mut signals = true;
        loop {
            match (hooks().wait)(slot.key(), &ready, signals) {
                Wake::Ready => break,
                Wake::Signal => {
                    // Ask the program to stop, now if it has the request
                    // or once it reads it, and wait on for its answer,
                    // unless the thread is stopped.
                    if self.interrupt_wanted(&slot) {
                        self.interrupt(unique);
                    }
                    signals = false;
                }
                Wake::Stop => {
                    if !self.take_back(&slot) {
                        slot.abandoned.store(true, Ordering::Release);
                        // An answer that came meanwhile is the call's to
                        // throw away, its nodes the kernel's to forget.
                        if slot.done.load(Ordering::Acquire) {
                            self.dropped(&slot);
                        }
                    }
                    return Err(Error::Interrupted);
                }
            }
        }
        let answer = slot.answer.lock().take();
        answer.unwrap_or(Err(Error::Io))
    }

    /// Takes the request of `slot` out of the queue, if the program has
    /// not read it yet.
    fn take_back(&self, slot: &KArc<Slot>) -> bool {
        let mut state = self.state.lock();
        let Some(at) = state
            .queue
            .iter()
            .position(|q| matches!(&q.answer, Answer::To(s) if KArc::ptr_eq(s, slot)))
        else {
            return false;
        };
        state.queue.remove(at);
        self.queued.store(state.queue.len(), Ordering::Release);
        true
    }

    /// A signal came for the call of `slot`: whether the program has read
    /// its request, and `INTERRUPT` is to be sent now; if not, the read
    /// that takes it sends it.
    fn interrupt_wanted(&self, slot: &KArc<Slot>) -> bool {
        let state = self.state.lock();
        let queued =
            state.queue.iter().any(|q| matches!(&q.answer, Answer::To(s) if KArc::ptr_eq(s, slot)));
        let now = if queued { Interrupt::Wanted } else { Interrupt::Sent };
        slot.interrupt.store(now as u8, Ordering::Release);
        !queued
    }

    /// Asks the program to stop working on request `unique`.
    fn interrupt(&self, unique: u64) {
        let bytes = request(op::INTERRUPT, 0, Some((0, 0)), &[&unique.to_le_bytes()], None);
        if let Ok(bytes) = bytes {
            let _ = self.queue(bytes, Answer::None, true);
        }
    }

    /// An abandoned call's answer came: forgets the node it names, if it
    /// names one the kernel took no reference to.
    fn dropped(&self, slot: &Slot) {
        let names_node = matches!(
            slot.opcode,
            op::LOOKUP | op::MKNOD | op::MKDIR | op::SYMLINK | op::LINK | op::CREATE
        );
        let answer = slot.answer.lock().take();
        if let (true, Some(Ok(data))) = (names_node, answer)
            && let Ok(entry) = abi::Entry::read(&mut Reader::new(&data))
            && entry.nodeid != 0
        {
            let _ = self.forget(entry.nodeid, 1);
            if slot.opcode == op::CREATE {
                // The file it opened, closed again.
                let mut r = Reader::new(&data[abi::Entry::SIZE..]);
                if let Ok(fh) = r.u64() {
                    let _ = self.release(op::RELEASE, entry.nodeid, fh, 1);
                }
            }
        }
    }

    /// Sends a request whose answer nobody waits for.
    fn post(&self, opcode: u32, nodeid: u64, parts: &[&[u8]], answer: Answer) -> Result<()> {
        let bytes = request(opcode, nodeid, Some((0, 0)), parts, None)?;
        self.queue(bytes, answer, false).map(drop)
    }

    /// Tells the program that the kernel has let go of `count` of the
    /// names it was given for `nodeid`.
    pub fn forget(&self, nodeid: u64, count: u64) -> Result<()> {
        self.post(op::FORGET, nodeid, &[&count.to_le_bytes()], Answer::None)
    }

    /// Closes the file or directory `fh` of node `nodeid` (`RELEASE` or
    /// `RELEASEDIR`), opened with `flags`, without waiting for the answer.
    pub fn release(&self, opcode: u32, nodeid: u64, fh: u64, flags: u32) -> Result<()> {
        let mut body = [0u8; 24];
        body[..8].copy_from_slice(&fh.to_le_bytes());
        body[8..12].copy_from_slice(&flags.to_le_bytes());
        self.post(opcode, nodeid, &[&body], Answer::Dropped)
    }

    /// The program reads the oldest request into `out`: waits for one
    /// unless `non_blocking`; returns its length.
    pub fn read(
        &self,
        out: &mut dyn Out,
        non_blocking: bool,
    ) -> core::result::Result<usize, DeviceError> {
        if out.len() < MIN_READ_BUFFER {
            return Err(DeviceError::Invalid);
        }
        loop {
            let mut interrupt = false;
            let taken = {
                let mut state = self.state.lock();
                if state.queue.is_empty() {
                    None
                } else {
                    let queued = state.queue.remove(0);
                    self.queued.store(state.queue.len(), Ordering::Release);
                    // A signal came for it while it waited to be read.
                    if let Answer::To(slot) = &queued.answer
                        && slot.interrupt.load(Ordering::Acquire) == Interrupt::Wanted as u8
                    {
                        slot.interrupt.store(Interrupt::Sent as u8, Ordering::Release);
                        interrupt = true;
                    }
                    Some(queued)
                }
            };
            let Some(queued) = taken else {
                match self.ended() {
                    Ended::No => {}
                    Ended::Lost | Ended::Unmounted => return Err(DeviceError::NoDevice),
                }
                if non_blocking {
                    return Err(DeviceError::WouldBlock);
                }
                let wait = (hooks().wait)(self.key(), &|| self.readable(), true);
                if wait != Wake::Ready && !self.readable() {
                    return Err(DeviceError::Interrupted);
                }
                continue;
            };
            let Queued { unique, bytes, answer } = queued;
            if bytes.len() > out.len() {
                // Too big for this program's buffer: the call fails.
                if let Answer::To(slot) = answer {
                    slot.answer(Err(Error::Io));
                }
                continue;
            }
            if let Err(err) = out.put(0, &bytes) {
                if let Answer::To(slot) = answer {
                    slot.answer(Err(Error::Io));
                }
                return Err(match err {
                    Error::NoMemory => DeviceError::NoMemory,
                    _ => DeviceError::BadAddress,
                });
            }
            if !matches!(answer, Answer::None) {
                let mut state = self.state.lock();
                if self.gone() && matches!(answer, Answer::To(_)) {
                    drop(state);
                    if let Answer::To(slot) = answer {
                        slot.answer(Err(Error::NotConnected));
                    }
                } else if state.sent.try_reserve(1).is_ok() {
                    let _ = state.sent.push_within_capacity(Sent { unique, answer });
                } else if let Answer::To(slot) = answer {
                    drop(state);
                    slot.answer(Err(Error::NoMemory));
                }
            }
            if interrupt {
                self.interrupt(unique);
            }
            return Ok(bytes.len());
        }
    }

    /// The program writes an answer: returns its length.
    pub fn write(&self, data: &dyn In) -> core::result::Result<usize, DeviceError> {
        let len = data.len();
        if !(abi::OUT_HEADER..=MAX_ANSWER).contains(&len) {
            return Err(DeviceError::Invalid);
        }
        let mut head = [0u8; abi::OUT_HEADER];
        data.get(0, &mut head).map_err(|_| DeviceError::BadAddress)?;
        let header = OutHeader::read(&head);
        if header.len as usize != len {
            return Err(DeviceError::Invalid);
        }
        if header.unique == 0 {
            // A notification: none is taken yet, and none needs to be.
            return Ok(len);
        }
        if header.error > 0 || header.error < -4095 || header.error != 0 && len != abi::OUT_HEADER {
            return Err(DeviceError::Invalid);
        }
        let mut body = mem::zeroed(len - abi::OUT_HEADER).map_err(|_| DeviceError::NoMemory)?;
        data.get(abi::OUT_HEADER, &mut body).map_err(|_| DeviceError::BadAddress)?;
        let answer = {
            let mut state = self.state.lock();
            let at = state
                .sent
                .iter()
                .position(|s| s.unique == header.unique)
                .ok_or(DeviceError::NotFound)?;
            state.sent.remove(at).answer
        };
        let result = if header.error == 0 { Ok(body) } else { Err(abi::error_of(-header.error)) };
        match answer {
            Answer::None | Answer::Dropped => {}
            Answer::Init => self.initialized(result),
            Answer::To(slot) => {
                slot.answer(result);
                if slot.abandoned.load(Ordering::Acquire) {
                    self.dropped(&slot);
                }
            }
        }
        Ok(len)
    }

    /// `INIT`'s answer came: the connection is set up, or refused (an
    /// older protocol than [`abi::MINOR_MIN`], or another major one).
    fn initialized(&self, answer: Result<KVec<u8>>) {
        let agreed = answer
            .ok()
            .and_then(|data| InitOut::read(&mut Reader::new(&data)).ok())
            .filter(|out| out.major == abi::MAJOR && out.minor >= abi::MINOR_MIN);
        let state = match agreed {
            Some(out) => {
                self.minor.store(out.minor.min(abi::MINOR), Ordering::Release);
                // As many pages as the program takes, or Linux's 32.
                let pages = if out.flags & abi::init::MAX_PAGES != 0 && out.max_pages > 0 {
                    u32::from(out.max_pages)
                } else {
                    32
                };
                let max = out.max_write.min(pages.saturating_mul(4096)).clamp(4096, MAX_DATA);
                self.max_data.store(max, Ordering::Release);
                Init::Done
            }
            None => Init::Refused,
        };
        self.init.store(state as u8, Ordering::Release);
        (hooks().wake)(self.init_key());
    }
}

/// Whether a request stays queued at an unmount: one nobody waits for
/// does; a call's fails as not connected.
fn kept(answer: &Answer) -> bool {
    match answer {
        Answer::To(slot) => {
            slot.answer(Err(Error::NotConnected));
            false
        }
        _ => true,
    }
}

/// The largest request a program may have to read, data included.
pub const fn max_request() -> usize {
    MAX_REQUEST
}

/// Writes a request's header for a body of `body` bytes: its number is
/// put in when it is queued.
fn header(
    w: &mut Writer,
    opcode: u32,
    nodeid: u64,
    body: usize,
    (uid, gid, pid): (u32, u32, u32),
) -> Result<()> {
    let len = u32::try_from(abi::IN_HEADER + body).map_err(|_| Error::TooBig)?;
    InHeader { len, opcode, unique: 0, nodeid, uid, gid, pid }.write(w)
}

/// A request: its header, `parts` and `data`'s `len` bytes from `at`.
fn request(
    opcode: u32,
    nodeid: u64,
    caller: Option<(u32, u32)>,
    parts: &[&[u8]],
    data: Option<(&dyn In, usize, usize)>,
) -> Result<KVec<u8>> {
    let fixed: usize = parts.iter().map(|p| p.len()).sum();
    if fixed > REQUEST_ROOM - abi::IN_HEADER {
        return Err(Error::NameTooLong);
    }
    let data_len = data.map_or(0, |(_, _, len)| len);
    if data_len > MAX_DATA as usize {
        return Err(Error::TooBig);
    }
    let body = fixed + data_len;
    let mut w = Writer::with_capacity(abi::IN_HEADER + fixed)?;
    let who = (hooks().caller)();
    let (uid, gid) = caller.unwrap_or((who.uid, who.gid));
    header(&mut w, opcode, nodeid, body, (uid, gid, who.pid))?;
    for part in parts {
        w.bytes(part)?;
    }
    let Some((input, at, len)) = data else { return Ok(w.0) };
    // The data straight from the caller's memory into the request.
    let mut bytes = mem::zeroed(w.0.len() + len)?;
    let (head, tail) = bytes.split_at_mut(w.0.len());
    head.copy_from_slice(&w.0);
    input.get(at, tail)?;
    Ok(bytes)
}
