// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Programs in user mode (Phases 5 and 6): their processes and threads,
//! the twelve system calls, and the kernel objects they work with.
//!
//! A program runs as a process (`process`): an address space, a
//! capability space and threads, which end together. A program's thread
//! is a scheduler thread with a [`UserThread`] attached: its process, the
//! capability space it names capabilities in, the address space it runs
//! in, its IPC state and its signals. The architecture takes it into user
//! mode once ([`UserMode::enter_user`]) and back into the kernel through
//! system calls (`syscall`), interrupts and faults. A fault in user mode
//! that no page the kernel makes on demand resolves raises a signal
//! (Phase 8), which ends the process unless a handler takes it; nothing a
//! program does stops the kernel.
//!
//! Kernel objects live in [`objects`]'s registry, keyed by the name the
//! capability store gives them, so a capability lookup leads to the object
//! itself. The store decides what a program may do; the objects carry it
//! out:
//!
//! - `ipc`: endpoints, synchronous messages of up to eight words and four
//!   capabilities, with a fast path for register-only messages;
//! - `notify`: notifications, words of bits that are signalled and waited
//!   for;
//! - `memory`: address spaces and frames, mapped and unmapped;
//! - `ring`: rings shared with programs (radix-ring);
//! - `process`: processes, and the kernel memory their objects take,
//!   charged to resource domains (`account`);
//! - `files`: files and directories (Phase 7, `crate::fs`);
//! - `posix`: the POSIX side of processes (Phase 8): the process tree,
//!   signals, `fork`, `exec`, timers and `poll`;
//! - `methods`: the methods called on capabilities with `cap_call`.
//!
//! The kernel itself starts one program, Spark, from the boot archive
//! (`init`), and the test programs of the `usertests` boot module
//! (`launch`) for the `user`, `ipc`, `fuzz` and `bench` self-tests
//! (`usertest`); Spark and its runtime start every other program.
//! `docs/design/ipc.md` and `docs/design/processes.md` describe the
//! design.
//!
//! Lock order: the capability store, the object registry, a process's
//! state, a frame's state, an address space's regions; an endpoint, then
//! a thread's IPC state; a ring's queues. None is held across a block.

mod account;
pub(crate) mod files;
pub mod init;
mod ipc;
pub mod launch;
mod loader;
mod memory;
mod methods;
mod notify;
pub mod objects;
pub(crate) mod posix;
pub mod process;
pub mod random;
mod ring;
mod syscall;

use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};

pub use radix_abi::process::{EXIT_FAULT, EXIT_STOPPED};

use radix_abi::Status;
use radix_abi::errors::Kernel;
use radix_abi::startup;
use radix_cap::{CapError, SpaceId};
use radix_hal::{Cpu, UserFault, UserFaultKind, UserMode};
use radix_klog::{info, warn};
use radix_mm::{Flags, HEAP, Kmalloc};
use radix_sync::SpinLock;

use crate::Arch;
use crate::sched::{self, ThreadRef, wait};
use memory::{Fault, SpaceObject};
use posix::ThreadSignals;
use process::{Ending, ProcessRef, Start};

/// A program thread's side, shared.
pub type UserRef = Arc<UserThread, Kmalloc>;

/// What a thread of a program has besides its scheduler side.
pub struct UserThread {
    /// The capability space its slot numbers name.
    cspace: SpaceId,
    /// The address space it runs in.
    space: memory::SpaceRef,
    /// Where it starts in user mode.
    start: Start,
    /// Set once whoever started it has finished setting it up.
    ready: AtomicBool,
    /// Its IPC state.
    ipc: SpinLock<ipc::IpcState>,
    /// The caller it owes a reply, after receiving a call.
    reply: SpinLock<Option<ipc::Caller>>,
    /// Its receive window: first slot (high half) and count (low half).
    window: AtomicU64,
    /// The process it is a thread of.
    pub(crate) process: ProcessRef,
    /// Its signal mask, the signals sent to it alone, and the rest of its
    /// signal state (Phase 8).
    pub(crate) signals: SpinLock<ThreadSignals>,
    /// Set by the `futex_wake` that takes it off a futex's waiters.
    pub(crate) futex_woken: AtomicBool,
}

impl UserThread {
    /// The address space.
    pub fn space(&self) -> &SpaceObject {
        &self.space
    }

    /// The receive window: first slot and count.
    fn window(&self) -> (u32, u32) {
        let word = self.window.load(Ordering::Relaxed);
        ((word >> 32) as u32, word as u32)
    }
}

/// A system call's result: its result words, or an error status.
type Result<T> = core::result::Result<T, Status>;

/// An error status with no detail.
fn error(kind: Kernel) -> Status {
    kind.status(0)
}

/// The status a capability error becomes. Missing rights put the rights
/// that were missing in the detail.
fn cap_error(err: CapError) -> Status {
    match err {
        CapError::NoSuchSpace => error(Kernel::Gone),
        CapError::NoSuchSlot => error(Kernel::NoSuchSlot),
        CapError::Empty => error(Kernel::EmptySlot),
        CapError::Occupied => error(Kernel::Occupied),
        CapError::WrongKind { found, .. } => Kernel::WrongKind.status(found as u32),
        CapError::MissingRights(rights) => Kernel::MissingRights.status(u32::from(rights.bits())),
        CapError::Sealed => error(Kernel::Sealed),
        CapError::NotSealed => error(Kernel::InvalidArgument),
        CapError::WrongKey => error(Kernel::WrongKey),
        CapError::NotBadgeable(_) | CapError::AlreadyBadged | CapError::InvalidBadge => {
            error(Kernel::BadBadge)
        }
        CapError::NotARange | CapError::OutOfRange => error(Kernel::OutOfRange),
        CapError::TooDeep => error(Kernel::TooDeep),
        CapError::Revoking => error(Kernel::Revoking),
        CapError::Exhausted => error(Kernel::Exhausted),
        CapError::NoMemory => error(Kernel::NoMemory),
        CapError::InvalidArgument => error(Kernel::InvalidArgument),
    }
}

/// The running thread and its program side, if it is a program's thread.
fn current() -> Option<(ThreadRef, UserRef)> {
    let thread = sched::current()?;
    let user = thread.user()?.clone();
    Some((thread, user))
}

/// Registers the system call, fault and return handlers, and reports user
/// mode. Called once at boot.
pub fn init() {
    Arch::set_syscall_handler(syscall::handle);
    Arch::set_user_fault_handler(on_fault);
    Arch::set_user_page_handler(on_user_page);
    Arch::set_user_return_handler(on_return);
    Arch::describe_user_mode(&mut |args| info!("{args}"));
    objects::init();
    // The ABI numbers object kinds as the store does.
    for kind in radix_cap::ObjectKind::ALL {
        let abi = radix_abi::capability::ObjectKind::from_raw(kind as u8);
        assert_eq!(abi.map(|abi| abi as u8), Some(kind as u8), "the ABI lacks the {kind} kind");
    }
    assert!(radix_abi::capability::ObjectKind::from_raw(radix_cap::KINDS as u8).is_none());
}

/// Where a program's thread starts: waits until whoever started it is
/// done with it, then leaves for user mode.
fn thread_main(_: usize) {
    let Some((thread, user)) = current() else { return };
    let key = Arc::as_ptr(&user) as usize;
    wait::wait_until(key, "starting", || {
        user.ready.load(Ordering::Acquire) || thread.should_stop()
    });
    if thread.should_stop() {
        drop(thread);
        exit_current(user, EXIT_STOPPED, Ending::Stop);
    }
    let start = user.start;
    // Nothing may stay on this stack: user mode abandons it.
    drop(thread);
    drop(user);
    match start {
        Start::Fresh([entry, stack, arg]) => Arch::enter_user(entry, stack, arg),
        Start::Registers(regs) => Arch::enter_user_registers(&regs),
    }
}

/// Lets a thread that waits to start go: it was set up, or is to stop.
fn set_ready(user: &UserThread) {
    user.ready.store(true, Ordering::Release);
    wait::wake_all(user as *const UserThread as usize);
}

/// Ends the running program thread with exit status `status`, which
/// ended `how`; its process hears of it (`ProcessObject::thread_exited`),
/// and may end with it.
fn exit_current(user: UserRef, status: u64, how: Ending) -> ! {
    ipc::exiting(&user);
    let process = user.process.clone();
    drop(user);
    if let Some(me) = sched::current() {
        process.thread_exited(&me, status, how);
    }
    drop(process);
    sched::exit()
}

/// A fault in user mode: a page the kernel makes on demand (anonymous
/// memory, a copy of a page shared since `fork`), or else a signal for the
/// thread (`posix::signal::fault`), or the end of its process.
fn on_fault(fault: &UserFault) {
    Arch::enable_interrupts();
    let Some((thread, user)) = current() else {
        panic!("User mode: {fault} in a thread that is not a program's");
    };
    let mut bus = false;
    if let UserFaultKind::Page { address, write, execute, .. } = fault.kind {
        match user.space.resolve(address, write, execute) {
            Fault::Resolved => return,
            Fault::Refused => {}
            Fault::Io => bus = true,
            Fault::NoMemory => {
                warn!(
                    "User: thread {} ({}) ended: no memory for a page at {address:#x}",
                    thread.id(),
                    thread.name()
                );
                user.process.posix.lock().ended_by.get_or_insert(radix_abi::signal::SIGKILL);
                drop(thread);
                exit_current(user, EXIT_FAULT, Ending::Fault);
            }
        }
    }
    if posix::signal::fault(&thread, &user, fault, bus) {
        // Its handler runs, from the way back to user mode.
        return;
    }
    drop(thread);
    exit_current(user, EXIT_FAULT, Ending::Fault)
}

/// A page fault on user memory in one of the kernel's own copies: makes
/// the page if the space makes it on demand.
fn on_user_page(address: u64, write: bool) -> bool {
    let Some(thread) = sched::running() else { return false };
    let Some(user) = thread.user() else { return false };
    user.space.resolve(address, write, false) == Fault::Resolved
}

/// On the way back to user mode, from a system call, an interrupt or a
/// fault: a thread asked to stop ends here, and one with signals to take
/// takes them (`posix::signal::deliver`).
fn on_return() {
    let Some(me) = sched::running() else { return };
    if !me.interrupted() {
        return;
    }
    Arch::enable_interrupts();
    let Some(user) = me.user() else { return };
    if me.should_stop() {
        info!("User: thread {} ({}) stopped", me.id(), me.name());
        exit_current(user.clone(), EXIT_STOPPED, Ending::Stop);
    }
    posix::signal::deliver(&me, user);
    if me.should_stop() {
        exit_current(user.clone(), EXIT_STOPPED, Ending::Stop);
    }
}

/// The first slot of the receive window programs start with, and its size.
const fn default_window() -> u64 {
    ((startup::SLOT_WINDOW as u64) << 32) | startup::WINDOW_SLOTS as u64
}

/// A first-in, first-out queue of waiters, which grows under spin locks
/// (with atomic allocations) and only ever says so when it cannot. Taking
/// from the front costs the same however many wait. (`VecDeque` is not
/// built into the kernel's `alloc`, which leaves out what allocates
/// without saying so when it cannot.)
pub(crate) struct Fifo<T> {
    /// The items from `head` on; the slots before it were taken.
    items: Vec<Option<T>, Kmalloc>,
    head: usize,
}

impl<T> Fifo<T> {
    pub(crate) fn new() -> Fifo<T> {
        Fifo { items: Vec::new_in(HEAP.with(Flags::ATOMIC)), head: 0 }
    }

    /// Adds `item` at the back; gives it back if there is no memory.
    pub(crate) fn push_back(&mut self, item: T) -> core::result::Result<(), T> {
        // Reuse the taken slots before growing: each item moves once per
        // time the queue fills, so a push costs the same on average.
        if self.items.len() == self.items.capacity() {
            self.compact();
        }
        if self.items.try_reserve(1).is_err() {
            return Err(item);
        }
        self.items
            .push_within_capacity(Some(item))
            .map(|_| ())
            .map_err(|item| item.expect("it was pushed as `Some`"))
    }

    /// Puts `item` back at the front; right after `pop_front` this needs
    /// no memory. Gives it back if there is no memory.
    pub(crate) fn push_front(&mut self, item: T) -> core::result::Result<(), T> {
        if self.head > 0 {
            self.head -= 1;
            self.items[self.head] = Some(item);
            return Ok(());
        }
        self.push_back(item)?;
        self.items.rotate_right(1);
        Ok(())
    }

    /// Takes the item at the front.
    pub(crate) fn pop_front(&mut self) -> Option<T> {
        let item = self.items.get_mut(self.head)?.take();
        self.head += 1;
        if self.head == self.items.len() {
            self.items.clear();
            self.head = 0;
        }
        item
    }

    /// Keeps only the items `keep` says to.
    pub(crate) fn retain(&mut self, mut keep: impl FnMut(&T) -> bool) {
        self.compact();
        self.items.retain(|item| item.as_ref().is_some_and(&mut keep));
    }

    pub(crate) fn len(&self) -> usize {
        self.items.len() - self.head
    }

    /// Takes every item, leaving it empty.
    pub(crate) fn take(&mut self) -> Fifo<T> {
        core::mem::replace(self, Fifo::new())
    }

    /// Drops the taken slots before `head`.
    fn compact(&mut self) {
        let live = self.len();
        self.items.rotate_left(self.head);
        self.items.truncate(live);
        self.head = 0;
    }
}

impl<T> IntoIterator for Fifo<T> {
    type Item = T;
    type IntoIter = core::iter::Flatten<alloc::vec::IntoIter<Option<T>, Kmalloc>>;

    fn into_iter(self) -> Self::IntoIter {
        self.items.into_iter().flatten()
    }
}
