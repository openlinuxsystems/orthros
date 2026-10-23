// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Terminals (Phase 8, radix-terminal(2)): pseudo-terminals, and the
//! controlling terminal of a session.
//!
//! A pseudo-terminal ([`Tty`]) is a line discipline (`radix_tty::Ldisc`)
//! between two sides. What is written to the master is typed: edited into
//! lines, echoed, and turned into signals for the foreground process
//! group. What programs write to the terminal (`/dev/pts/N`) goes through
//! output processing to the master's reads. Opening `/dev/ptmx` makes one
//! ([`Ptmx`]) and gives the opener its master; `/dev/pts/N` appears until
//! the master goes, which hangs the terminal up.
//!
//! Each open of either side is a node of its own ([`Handle`]), so the
//! terminal counts how many descriptions each side has: a master read
//! fails with io once the terminal is no longer open, and a terminal read
//! finds the end of the file once the master has gone.
//!
//! A terminal may be the controlling terminal of one session: the one
//! whose leader opened it first without `no_terminal` while it had none,
//! or asked for it (SET_CONTROLLING). The terminal keeps its session and
//! foreground process group; each process keeps the terminal its session
//! had when it looked (`PosixState::terminal`), which forks inherit. A
//! process of the session in a background process group that reads from
//! it gets SIGTTIN, and one that writes to it with TOSTOP, or changes its
//! settings, SIGTTOU, as `radix-posix(7)` describes; `/dev/tty` opens the
//! caller's controlling terminal. When the session leader ends, or the
//! master goes, the foreground process group gets SIGHUP (and SIGCONT)
//! and the terminal belongs to no session.
//!
//! Lock order: a terminal's state, then the process tree. Signals are sent
//! with neither held.

use alloc::sync::Weak;
use alloc::vec::Vec;
use core::any::Any;
use core::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicUsize, Ordering};

use radix_abi::Status;
use radix_abi::errors::{File as FileError, Kernel};
use radix_abi::signal::{SIGCONT, SIGHUP, SIGINT, SIGQUIT, SIGTSTP, SIGTTIN, SIGTTOU, SIGWINCH};
use radix_abi::terminal::{self as abi, TOSTOP, WindowSize};
use radix_hal::UserMode;
use radix_mm::Kmalloc;
use radix_sync::{Mutex, Once};
use radix_tty::{Ldisc, Ring, Settings, TERMIOS_SIZE};
use radix_vfs::synthetic::FixedDir;
use radix_vfs::{In, KArc, Kind, Node, NodeRef, Out, Stat, mem, next_id};

use crate::Arch;
use crate::sched::{self, wait};
use crate::user::UserThread;
use crate::user::posix::signal::{Restart, interrupted};
use crate::user::posix::{self, poll};
use crate::user::process::ProcessRef;

/// A terminal, shared.
pub type TtyRef = KArc<Tty>;

/// The input a terminal holds, typed and not read: Linux's 4 KiB.
const INPUT: usize = 4096;

/// The output a terminal holds for its master.
const OUTPUT: usize = 16384;

/// The room in the output that makes the terminal side writable for
/// `poll`: Linux's WAKEUP_CHARS.
const WRITE_ROOM: usize = 256;

/// The most pseudo-terminals at once.
const MAX_PTYS: usize = 256;

/// A pseudo-terminal.
pub struct Tty {
    /// N, of `/dev/pts/N`.
    number: u32,
    /// The node number `/dev/pts/N` shows.
    id: u64,
    state: Mutex<State>,
    /// Whether a terminal read would find something, as the line
    /// discipline says, for the waits.
    input_ready: AtomicBool,
    /// How many bytes of output wait for the master.
    output_len: AtomicUsize,
    /// How many descriptions of the master and of the terminal are open,
    /// and whether the terminal ever was.
    masters: AtomicU32,
    slaves: AtomicU32,
    opened: AtomicBool,
    /// The session it controls (0: none) and that session's foreground
    /// process group.
    session: AtomicU64,
    foreground: AtomicU64,
}

/// A terminal's state.
struct State {
    ldisc: Ldisc<Vec<u8, Kmalloc>>,
    output: Ring<Vec<u8, Kmalloc>>,
    window: [u16; 4],
    /// `/dev/pts/N` may not be opened (SET_PTY_LOCK).
    locked: bool,
}

/// The pseudo-terminals, by number, made when first needed.
static PTYS: Mutex<Option<Vec<Weak<Tty, Kmalloc>, Kmalloc>>> = Mutex::new(None);

/// `/dev/pts`.
static PTS: Once<KArc<FixedDir>> = Once::new();

fn heap() -> Kmalloc {
    radix_mm::HEAP.with(radix_mm::Flags::MAY_SLEEP)
}

/// A buffer of `len` zero bytes.
fn buffer(len: usize) -> Option<Vec<u8, Kmalloc>> {
    let mut bytes = Vec::new_in(heap());
    bytes.try_reserve_exact(len).ok()?;
    while bytes.len() < len {
        // Room was reserved above.
        let _ = bytes.push_within_capacity(0);
    }
    Some(bytes)
}

/// The status of a terminal that has hung up.
fn hung_up() -> Status {
    FileError::Io.status(0)
}

impl Tty {
    fn key(&self) -> usize {
        self as *const Tty as usize
    }

    /// Something changed: the waits look again, and so does `poll`.
    fn changed(&self) {
        wait::wake_all(self.key());
        poll::changed();
    }

    /// Updates the atomics the waits read, with the state locked.
    fn mirror(&self, state: &State) {
        self.input_ready.store(state.ldisc.ready(), Ordering::Release);
        self.output_len.store(state.output.len(), Ordering::Release);
    }

    /// N, of its `/dev/pts/N`.
    pub fn number(&self) -> u32 {
        self.number
    }

    /// The session it controls, 0 for none.
    pub fn session(&self) -> u64 {
        self.session.load(Ordering::Acquire)
    }

    /// The events `poll` finds on a side of it, as Linux's: the terminal
    /// side is readable when a read would find something and writable
    /// while the master's output has room, and once the master has gone,
    /// readable, writable, in error and hung up at once. The master is
    /// readable while output waits, always writable (typing past what the
    /// terminal holds is dropped), and hung up once the terminal, opened
    /// before, is open no more.
    pub fn events(&self, master: bool) -> u16 {
        let mut events = 0;
        if master {
            events |= poll::WRITABLE;
            if self.output_len.load(Ordering::Acquire) > 0 {
                events |= poll::READABLE;
            }
            if self.opened.load(Ordering::Acquire) && self.slaves.load(Ordering::Acquire) == 0 {
                events |= poll::HANG_UP;
            }
        } else if self.masters.load(Ordering::Acquire) == 0 {
            events = poll::READABLE | poll::WRITABLE | poll::ERROR | poll::HANG_UP;
        } else {
            if self.input_ready.load(Ordering::Acquire) {
                events |= poll::READABLE;
            }
            if self.output_len.load(Ordering::Acquire) + WRITE_ROOM <= OUTPUT {
                events |= poll::WRITABLE;
            }
        }
        events
    }

    /// Sends `signal` to its foreground process group, if it has one.
    fn signal_foreground(&self, signal: u32) {
        let group = self.foreground.load(Ordering::Acquire);
        if group != 0 && self.session() != 0 {
            let info = radix_posix::signal::Info {
                code: radix_posix::signal::SI_KERNEL,
                pid: 0,
                status: 0,
            };
            posix::signal::send_group(group, signal, info);
        }
    }

    /// Hangs it up: its foreground process group gets SIGHUP and SIGCONT,
    /// and it controls no session.
    fn hang_up(&self) {
        if self.session() == 0 {
            return;
        }
        self.signal_foreground(SIGHUP);
        self.signal_foreground(SIGCONT);
        self.session.store(0, Ordering::Release);
        self.foreground.store(0, Ordering::Release);
        self.changed();
    }

    /// The leader of session `session` has ended: if it controlled this
    /// terminal, the terminal hangs up.
    pub fn leader_exited(&self, session: u64) {
        if self.session() == session {
            self.hang_up();
        }
    }

    /// `process` opened the terminal without `no_terminal`: if it leads a
    /// session with no controlling terminal and the terminal controls none,
    /// it becomes that session's.
    pub fn opened_by(this: &TtyRef, process: &ProcessRef) {
        let _ = Self::make_controlling(this, process);
    }

    /// Makes the terminal the controlling terminal of `process`'s session,
    /// which `process` must lead, with no controlling terminal yet; if it
    /// already is that, nothing changes, as on Linux.
    fn make_controlling(this: &TtyRef, process: &ProcessRef) -> Result<(), Status> {
        let (pid, group, session) = posix::ids_of(process)?;
        if pid != session {
            return Err(Kernel::NotPermitted.status(0));
        }
        let mut posix = process.posix.lock();
        let current = posix.terminal.as_ref().filter(|tty| tty.session() == session);
        if let Some(tty) = current {
            return if KArc::ptr_eq(tty, this) {
                Ok(())
            } else {
                Err(Kernel::NotPermitted.status(0))
            };
        }
        // Claimed in one step, so that of two session leaders asking for
        // the same terminal at once, one fails.
        if this.session.compare_exchange(0, session, Ordering::AcqRel, Ordering::Acquire).is_err() {
            return Err(Kernel::NotPermitted.status(0));
        }
        this.foreground.store(group, Ordering::Release);
        posix.terminal = Some(this.clone());
        Ok(())
    }

    /// The caller's relation to it: `None` if it is not the caller's
    /// controlling terminal; otherwise whether the caller is in its
    /// foreground process group, and its own group.
    fn relation(&self, process: &ProcessRef) -> Option<(bool, u64)> {
        let session = self.session();
        if session == 0 {
            return None;
        }
        let (_, group, mine) = posix::ids_of(process).ok()?;
        (mine == session).then(|| (self.foreground.load(Ordering::Acquire) == group, group))
    }

    /// Job control: `user`'s thread may read (`signal` SIGTTIN) or write
    /// or change it (SIGTTOU) only from the foreground; from the
    /// background its process group gets the signal, and the call runs
    /// again once it continues.
    fn job_control(&self, user: &UserThread, signal: u32) -> Result<(), Status> {
        match self.relation(&user.process) {
            None | Some((true, _)) => Ok(()),
            Some((false, group)) => posix::signal::job_control(user, group, signal),
        }
    }

    /// The terminal side's read: a line, or in non-canonical mode what
    /// VMIN asks for.
    fn read(
        &self,
        user: &UserThread,
        out: &mut dyn Out,
        non_blocking: bool,
    ) -> Result<usize, Status> {
        self.job_control(user, SIGTTIN)?;
        let want = out.len();
        let mut deadline = None;
        loop {
            {
                let mut state = self.state.lock();
                let (min, time) = state.ldisc.settings.min_time();
                let canonical = state.ldisc.settings.canonical();
                let now =
                    state.ldisc.ready() || (!canonical && min == 0 && state.ldisc.pending() > 0);
                if now {
                    let mut at = 0;
                    let read = state.ldisc.read_with(want, &mut |run| {
                        out.put(at, run)?;
                        at += run.len();
                        Ok::<(), radix_vfs::Error>(())
                    });
                    self.mirror(&state);
                    drop(state);
                    self.changed();
                    return read.map_err(super::status);
                }
                if self.masters.load(Ordering::Acquire) == 0 {
                    return Ok(0);
                }
                if !canonical && min == 0 {
                    // VTIME alone: wait that long for a byte.
                    if time == 0 || deadline.is_some_and(|d| sched::now_ns() >= d) {
                        return Ok(0);
                    }
                    deadline.get_or_insert(sched::now_ns() + u64::from(time) * 100_000_000);
                }
            }
            if non_blocking {
                return Err(Kernel::WouldBlock.status(0));
            }
            let ready = || {
                self.input_ready.load(Ordering::Acquire)
                    || self.masters.load(Ordering::Acquire) == 0
                    || sched::interrupted()
            };
            match deadline {
                Some(deadline) => {
                    wait::wait_until_or_timeout(self.key(), "terminal read", deadline, ready);
                }
                None => wait::wait_until(self.key(), "terminal read", ready),
            }
            if sched::interrupted() && !self.input_ready.load(Ordering::Acquire) {
                return Err(interrupted(Restart::Always));
            }
        }
    }

    /// The terminal side's write: processed for the screen, into the
    /// master's output.
    fn write(&self, user: &UserThread, data: &dyn In, non_blocking: bool) -> Result<usize, Status> {
        if self.state.lock().ldisc.settings.lflag & TOSTOP != 0 {
            self.job_control(user, SIGTTOU)?;
        }
        let total = data.len();
        let mut done = 0;
        let mut chunk = [0u8; 256];
        while done < total {
            if self.masters.load(Ordering::Acquire) == 0 {
                return if done > 0 { Ok(done) } else { Err(hung_up()) };
            }
            let n = (total - done).min(chunk.len());
            data.get(done, &mut chunk[..n]).map_err(super::status)?;
            let taken = {
                let mut state = self.state.lock();
                let State { ldisc, output, .. } = &mut *state;
                let room = output.free();
                let taken = if ldisc.output_stopped {
                    0
                } else {
                    ldisc.output(&chunk[..n], room, &mut |run| {
                        output.push(run);
                    })
                };
                self.mirror(&state);
                taken
            };
            if taken > 0 {
                done += taken;
                self.changed();
                continue;
            }
            if non_blocking {
                return if done > 0 { Ok(done) } else { Err(Kernel::WouldBlock.status(0)) };
            }
            wait::wait_until(self.key(), "terminal write", || {
                self.output_len.load(Ordering::Acquire) + 2 <= OUTPUT
                    || self.masters.load(Ordering::Acquire) == 0
                    || sched::interrupted()
            });
            if sched::interrupted() {
                return if done > 0 { Ok(done) } else { Err(interrupted(Restart::Always)) };
            }
        }
        Ok(done)
    }

    /// The master's read: the terminal's output.
    fn master_read(&self, out: &mut dyn Out, non_blocking: bool) -> Result<usize, Status> {
        let want = out.len();
        loop {
            {
                let mut state = self.state.lock();
                if !state.output.is_empty() {
                    let mut at = 0;
                    let read = state.output.read_with(want, &mut |run| {
                        out.put(at, run)?;
                        at += run.len();
                        Ok::<(), radix_vfs::Error>(())
                    });
                    self.mirror(&state);
                    drop(state);
                    self.changed();
                    return read.map_err(super::status);
                }
            }
            if self.opened.load(Ordering::Acquire) && self.slaves.load(Ordering::Acquire) == 0 {
                return Err(hung_up());
            }
            if non_blocking {
                return Err(Kernel::WouldBlock.status(0));
            }
            wait::wait_until(self.key(), "terminal output", || {
                self.output_len.load(Ordering::Acquire) > 0
                    || (self.opened.load(Ordering::Acquire)
                        && self.slaves.load(Ordering::Acquire) == 0)
                    || sched::interrupted()
            });
            if sched::interrupted() && self.output_len.load(Ordering::Acquire) == 0 {
                return Err(interrupted(Restart::Always));
            }
        }
    }

    /// The master's write: typing. Signal characters send their signals
    /// to the foreground process group; input past what the terminal holds
    /// is dropped, as a terminal's typing is.
    fn master_write(&self, data: &dyn In) -> Result<usize, Status> {
        let total = data.len();
        let mut done = 0;
        let mut chunk = [0u8; 256];
        while done < total {
            let n = (total - done).min(chunk.len());
            data.get(done, &mut chunk[..n]).map_err(super::status)?;
            let mut signals = [0u32; 4];
            let mut count = 0;
            {
                let mut state = self.state.lock();
                let State { ldisc, output, .. } = &mut *state;
                for &byte in &chunk[..n] {
                    let (signal, _) = ldisc.receive(byte, &mut |echo| {
                        output.push(echo);
                    });
                    if let Some(signal) = signal
                        && count < signals.len()
                    {
                        signals[count] = signal;
                        count += 1;
                    }
                }
                self.mirror(&state);
            }
            self.changed();
            for &signal in &signals[..count] {
                debug_assert!(matches!(signal, SIGINT | SIGQUIT | SIGTSTP));
                self.signal_foreground(signal);
            }
            done += n;
        }
        Ok(done)
    }

    /// A request of radix-file(2)'s `control`, from `user`'s thread, on the
    /// master side or the terminal side.
    fn control(
        this: &TtyRef,
        master: bool,
        user: &UserThread,
        request: u32,
        argument: u64,
        buffer: u64,
        size: u64,
    ) -> Result<u64, Status> {
        let word_in = || -> Result<u32, Status> {
            let mut bytes = [0u8; 4];
            if size < 4 || !Arch::copy_from_user(&mut bytes, buffer) {
                return Err(Kernel::BadAddress.status(0));
            }
            Ok(u32::from_le_bytes(bytes))
        };
        let put = |bytes: &[u8]| -> Result<u64, Status> {
            if size < bytes.len() as u64 || !Arch::copy_to_user(buffer, bytes) {
                return Err(Kernel::BadAddress.status(0));
            }
            Ok(0)
        };
        let process = &user.process;
        match request {
            abi::GET_ATTRIBUTES => {
                let bytes = this.state.lock().ldisc.settings.to_bytes();
                put(&bytes)
            }
            abi::SET_ATTRIBUTES | abi::SET_ATTRIBUTES_DRAIN | abi::SET_ATTRIBUTES_FLUSH => {
                let mut bytes = [0u8; TERMIOS_SIZE];
                if size < TERMIOS_SIZE as u64 || !Arch::copy_from_user(&mut bytes, buffer) {
                    return Err(Kernel::BadAddress.status(0));
                }
                if !master {
                    this.job_control(user, SIGTTOU)?;
                }
                let mut state = this.state.lock();
                state.ldisc.set(Settings::from_bytes(&bytes));
                if request == abi::SET_ATTRIBUTES_FLUSH {
                    state.ldisc.flush_input();
                }
                this.mirror(&state);
                drop(state);
                this.changed();
                Ok(0)
            }
            abi::FLOW => {
                let mut state = this.state.lock();
                match argument {
                    0 => state.ldisc.output_stopped = true,
                    1 => state.ldisc.output_stopped = false,
                    2 | 3 => {}
                    _ => return Err(Kernel::InvalidArgument.status(1)),
                }
                drop(state);
                this.changed();
                Ok(0)
            }
            abi::FLUSH => {
                let mut state = this.state.lock();
                match argument {
                    0 => state.ldisc.flush_input(),
                    1 => state.output.clear(),
                    2 => {
                        state.ldisc.flush_input();
                        state.output.clear();
                    }
                    _ => return Err(Kernel::InvalidArgument.status(1)),
                }
                this.mirror(&state);
                drop(state);
                this.changed();
                Ok(0)
            }
            abi::SET_CONTROLLING => Tty::make_controlling(this, process).map(|()| 0),
            abi::GET_FOREGROUND => {
                if this.relation(process).is_none() && !master {
                    return Err(Kernel::NotTerminal.status(0));
                }
                put(&(this.foreground.load(Ordering::Acquire) as u32).to_le_bytes())
            }
            abi::SET_FOREGROUND => {
                let group = u64::from(word_in()?);
                let Some((_, _)) = this.relation(process) else {
                    return Err(Kernel::NotTerminal.status(0));
                };
                this.job_control(user, SIGTTOU)?;
                if !posix::group_in_session(group, this.session()) {
                    return Err(Kernel::NotPermitted.status(0));
                }
                this.foreground.store(group, Ordering::Release);
                Ok(0)
            }
            abi::GET_WINDOW_SIZE => {
                let window = this.state.lock().window;
                let mut bytes = [0u8; WindowSize::SIZE];
                for (i, value) in window.iter().enumerate() {
                    bytes[2 * i..2 * i + 2].copy_from_slice(&value.to_le_bytes());
                }
                put(&bytes)
            }
            abi::SET_WINDOW_SIZE => {
                let mut bytes = [0u8; WindowSize::SIZE];
                if size < WindowSize::SIZE as u64 || !Arch::copy_from_user(&mut bytes, buffer) {
                    return Err(Kernel::BadAddress.status(0));
                }
                let window: [u16; 4] =
                    core::array::from_fn(|i| u16::from_le_bytes([bytes[2 * i], bytes[2 * i + 1]]));
                let changed = {
                    let mut state = this.state.lock();
                    core::mem::replace(&mut state.window, window) != window
                };
                if changed {
                    this.signal_foreground(SIGWINCH);
                }
                Ok(0)
            }
            abi::READABLE => {
                let state = this.state.lock();
                let pending = if master { state.output.len() } else { state.ldisc.pending() };
                drop(state);
                put(&(pending as u32).to_le_bytes())
            }
            abi::DETACH => {
                if this.relation(process).is_none() {
                    return Err(Kernel::NotTerminal.status(0));
                }
                let (pid, _, session) = posix::ids_of(process)?;
                process.posix.lock().terminal = None;
                if pid == session {
                    this.hang_up();
                }
                Ok(0)
            }
            abi::GET_SESSION => {
                if this.relation(process).is_none() && !master {
                    return Err(Kernel::NotTerminal.status(0));
                }
                put(&(this.session() as u32).to_le_bytes())
            }
            // Both sides tell, the terminal's for `ttyname`.
            abi::GET_PTY_NUMBER => put(&this.number.to_le_bytes()),
            abi::SET_PTY_LOCK if master => {
                let lock = word_in()? != 0;
                this.state.lock().locked = lock;
                Ok(0)
            }
            _ => Err(Kernel::NotTerminal.status(0)),
        }
    }
}

/// Whether a session other than `session` already has `tty`.
/// One open of a side of a terminal: what a file description of it
/// holds.
pub struct Handle {
    tty: TtyRef,
    /// Whether it is the master side.
    pub master: bool,
}

impl Handle {
    /// The terminal.
    pub fn tty(&self) -> &TtyRef {
        &self.tty
    }

    /// Reads, as `user`'s thread.
    pub fn read(
        &self,
        user: &UserThread,
        out: &mut dyn Out,
        non_blocking: bool,
    ) -> Result<usize, Status> {
        if self.master {
            self.tty.master_read(out, non_blocking)
        } else {
            self.tty.read(user, out, non_blocking)
        }
    }

    /// Writes, as `user`'s thread.
    pub fn write(
        &self,
        user: &UserThread,
        data: &dyn In,
        non_blocking: bool,
    ) -> Result<usize, Status> {
        if self.master {
            self.tty.master_write(data)
        } else {
            self.tty.write(user, data, non_blocking)
        }
    }

    /// A `control` request.
    pub fn control(
        &self,
        user: &UserThread,
        request: u32,
        argument: u64,
        buffer: u64,
        size: u64,
    ) -> Result<u64, Status> {
        Tty::control(&self.tty, self.master, user, request, argument, buffer, size)
    }

    /// The events `poll` finds.
    pub fn events(&self) -> u16 {
        self.tty.events(self.master)
    }
}

impl Drop for Handle {
    fn drop(&mut self) {
        if self.master {
            if self.tty.masters.fetch_sub(1, Ordering::AcqRel) == 1 {
                // The terminal goes with its master.
                self.tty.hang_up();
                if let Some(pts) = PTS.get() {
                    let mut name = radix_klog::TextBuf::<12>::new();
                    let _ =
                        core::fmt::Write::write_fmt(&mut name, format_args!("{}", self.tty.number));
                    pts.remove_entry(name.as_str());
                }
            }
        } else {
            self.tty.slaves.fetch_sub(1, Ordering::AcqRel);
        }
        self.tty.changed();
    }
}

fn char_device(id: u64) -> Stat {
    Stat { kind: Kind::CharDevice, size: 0, allocated: 0, id, created: 0, modified: 0, accessed: 0 }
}

impl Node for Handle {
    fn stat(&self) -> Stat {
        char_device(self.tty.id)
    }

    fn kind(&self) -> Kind {
        Kind::CharDevice
    }

    // The kernel's own reads and writes (a ring's) go through
    // `user::files`, which knows the caller; through the node they fail.
    fn read(&self, _offset: u64, _out: &mut dyn Out) -> radix_vfs::Result<usize> {
        Err(radix_vfs::Error::NotSupported)
    }

    fn write(&self, _offset: u64, _data: &dyn In) -> radix_vfs::Result<usize> {
        Err(radix_vfs::Error::NotSupported)
    }

    fn set_size(&self, _size: u64) -> radix_vfs::Result<()> {
        Ok(())
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

/// A new open of `tty`'s side.
fn open_side(tty: &TtyRef, master: bool) -> radix_vfs::Result<NodeRef> {
    let handle: NodeRef = mem::arc(Handle { tty: tty.clone(), master })?;
    if master {
        tty.masters.fetch_add(1, Ordering::AcqRel);
    } else {
        tty.slaves.fetch_add(1, Ordering::AcqRel);
        tty.opened.store(true, Ordering::Release);
    }
    Ok(handle)
}

/// `/dev/pts/N`: the terminal side of pseudo-terminal N.
struct Entry {
    tty: TtyRef,
}

impl Node for Entry {
    fn stat(&self) -> Stat {
        char_device(self.tty.id)
    }

    fn kind(&self) -> Kind {
        Kind::CharDevice
    }

    fn open(&self, _this: &NodeRef) -> radix_vfs::Result<NodeRef> {
        if self.tty.state.lock().locked || self.tty.masters.load(Ordering::Acquire) == 0 {
            return Err(radix_vfs::Error::Io);
        }
        open_side(&self.tty, false)
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

/// `/dev/ptmx`: opening it makes a pseudo-terminal.
pub struct Ptmx(pub u64);

impl Node for Ptmx {
    fn stat(&self) -> Stat {
        char_device(self.0)
    }

    fn kind(&self) -> Kind {
        Kind::CharDevice
    }

    fn open(&self, _this: &NodeRef) -> radix_vfs::Result<NodeRef> {
        let tty = make()?;
        open_side(&tty, true)
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

/// `/dev/tty`: opening it opens the caller's controlling terminal.
pub struct Current(pub u64);

impl Node for Current {
    fn stat(&self) -> Stat {
        char_device(self.0)
    }

    fn kind(&self) -> Kind {
        Kind::CharDevice
    }

    fn open(&self, _this: &NodeRef) -> radix_vfs::Result<NodeRef> {
        let thread = sched::current().ok_or(radix_vfs::Error::NoDevice)?;
        let user = thread.user().ok_or(radix_vfs::Error::NoDevice)?;
        let tty = controlling(&user.process).ok_or(radix_vfs::Error::NoDevice)?;
        open_side(&tty, false)
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

/// `process`'s controlling terminal, if its session has one.
pub fn controlling(process: &ProcessRef) -> Option<TtyRef> {
    let tty = process.posix.lock().terminal.clone()?;
    let (_, _, session) = posix::ids_of(process).ok()?;
    (tty.session() == session).then_some(tty)
}

/// Makes a pseudo-terminal, with the lowest free number, and its
/// `/dev/pts/N`.
fn make() -> radix_vfs::Result<TtyRef> {
    let pts = PTS.get().ok_or(radix_vfs::Error::NotFound)?;
    let input = buffer(INPUT).ok_or(radix_vfs::Error::NoMemory)?;
    let output = buffer(OUTPUT).ok_or(radix_vfs::Error::NoMemory)?;
    let mut ptys = PTYS.lock();
    let list = ptys.get_or_insert_with(|| Vec::new_in(heap()));
    let number = match list.iter().position(|weak| weak.strong_count() == 0) {
        Some(free) => free,
        None if list.len() < MAX_PTYS => {
            list.try_reserve(1).map_err(|_| radix_vfs::Error::NoMemory)?;
            let _ = list.push_within_capacity(Weak::new_in(heap()));
            list.len() - 1
        }
        None => return Err(radix_vfs::Error::NoSpace),
    };
    let mut ldisc = Ldisc::new(input);
    ldisc.set(Settings::new());
    let tty = mem::arc(Tty {
        number: number as u32,
        id: next_id(),
        state: Mutex::new(State {
            ldisc,
            output: Ring::new(output),
            window: [24, 80, 0, 0],
            locked: false,
        }),
        input_ready: AtomicBool::new(false),
        output_len: AtomicUsize::new(0),
        masters: AtomicU32::new(0),
        slaves: AtomicU32::new(0),
        opened: AtomicBool::new(false),
        session: AtomicU64::new(0),
        foreground: AtomicU64::new(0),
    })?;
    list[number] = KArc::downgrade(&tty);
    drop(ptys);
    let entry: NodeRef = mem::arc(Entry { tty: tty.clone() })?;
    let mut name = radix_klog::TextBuf::<12>::new();
    let _ = core::fmt::Write::write_fmt(&mut name, format_args!("{number}"));
    pts.add(name.as_str(), entry)?;
    Ok(tty)
}

/// Makes `/dev/pts` in the device tree `root`, with `/dev/ptmx` and
/// `/dev/tty`.
pub fn init(root: &FixedDir) -> radix_vfs::Result<()> {
    let pts = root.add_dir("pts", next_id())?;
    let _ = PTS.set(pts);
    let ptmx: NodeRef = mem::arc(Ptmx(next_id()))?;
    root.add("ptmx", ptmx)?;
    let current: NodeRef = mem::arc(Current(next_id()))?;
    root.add("tty", current)
}

/// The side of a terminal `node` is, if it is one.
pub fn handle_of(node: &NodeRef) -> Option<&Handle> {
    radix_vfs::downcast::<Handle>(&**node)
}
