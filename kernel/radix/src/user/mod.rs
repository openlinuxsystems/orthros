// SPDX-FileCopyrightText: 2026 The OrthrOS contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Programs in user mode (Phase 5): their threads, the twelve system
//! calls, and the kernel objects they work with.
//!
//! A program's thread is a scheduler thread with a [`UserThread`] attached:
//! the capability space it names capabilities in, the address space it
//! runs in, and its IPC state. The architecture takes it into user mode
//! once ([`UserMode::enter_user`]) and back into the kernel through system
//! calls (`syscall`), interrupts and faults. A fault in user mode ends the
//! thread; nothing a program does stops the kernel.
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
//! - `methods`: the methods called on capabilities with `cap_call`.
//!
//! Until Phase 6 brings processes and init, the kernel itself starts
//! programs from the `usertests` boot module (`launch`), for the
//! `user`, `ipc` and `bench` self-tests (`usertest`). `docs/design/ipc.md`
//! describes the design.
//!
//! Lock order: the capability store, the object registry, a frame's state,
//! an address space's regions; an endpoint, then a thread's IPC state; a
//! ring's queues. None is held across a block.

mod ipc;
pub mod launch;
mod memory;
mod methods;
mod notify;
pub mod objects;
mod ring;
mod syscall;

use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use radix_abi::Status;
use radix_abi::errors::Kernel;
use radix_abi::startup;
use radix_cap::{CapError, SpaceId};
use radix_hal::{Cpu, UserFault, UserMode};
use radix_klog::{info, warn};
use radix_mm::{Flags, HEAP, Kmalloc};
use radix_sync::SpinLock;

use crate::Arch;
use crate::sched::{self, ThreadRef};
use memory::SpaceObject;

/// A program thread's side, shared.
pub type UserRef = Arc<UserThread, Kmalloc>;

/// The exit status of a thread a fault ended.
pub const EXIT_FAULT: u64 = 0xdead_0000;
/// The exit status of a thread the kernel stopped.
pub const EXIT_STOPPED: u64 = 0xdead_0001;

/// What a thread of a program has besides its scheduler side.
pub struct UserThread {
    /// The capability space its slot numbers name.
    cspace: SpaceId,
    /// The address space it runs in.
    space: memory::SpaceRef,
    /// Where it starts in user mode: entry point, stack top, argument.
    start: [u64; 3],
    /// Set once the launcher has finished setting it up.
    ready: AtomicBool,
    /// Its IPC state.
    ipc: SpinLock<ipc::IpcState>,
    /// The caller it owes a reply, after receiving a call.
    reply: SpinLock<Option<ipc::Caller>>,
    /// Its receive window: first slot (high half) and count (low half).
    window: AtomicU64,
    /// Its exit status, once it has exited.
    exit: AtomicU64,
    exited: AtomicBool,
    /// Woken when it exits.
    program: Arc<launch::Program, Kmalloc>,
}

impl UserThread {
    /// The address space.
    pub fn space(&self) -> &SpaceObject {
        &self.space
    }

    /// Its exit status, if it has exited.
    pub fn exit_status(&self) -> Option<u64> {
        self.exited.load(Ordering::Acquire).then(|| self.exit.load(Ordering::Relaxed))
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

/// Where a program's thread starts: waits until the launcher is done with
/// it, then leaves for user mode.
fn thread_main(_: usize) {
    let Some((thread, user)) = current() else { return };
    let key = Arc::as_ptr(&user) as usize;
    sched::wait::wait_until(key, "starting", || {
        user.ready.load(Ordering::Acquire) || thread.should_stop()
    });
    if thread.should_stop() {
        drop(thread);
        exit_current(user, EXIT_STOPPED);
    }
    let [entry, stack, arg] = user.start;
    // Nothing may stay on this stack: user mode abandons it.
    drop(thread);
    drop(user);
    Arch::enter_user(entry, stack, arg)
}

/// Ends the running program thread with exit status `status`.
fn exit_current(user: UserRef, status: u64) -> ! {
    ipc::exiting(&user);
    user.exit.store(status, Ordering::Relaxed);
    user.exited.store(true, Ordering::Release);
    let program = user.program.clone();
    drop(user);
    program.thread_exited();
    drop(program);
    sched::exit()
}

/// A fault in user mode: the thread ends.
fn on_fault(fault: &UserFault) -> ! {
    Arch::enable_interrupts();
    let Some((thread, user)) = current() else {
        panic!("User mode: {fault} in a thread that is not a program's");
    };
    warn!("User: thread {} ({}) ended by a {fault}", thread.id(), thread.name());
    drop(thread);
    exit_current(user, EXIT_FAULT)
}

/// On the way back to user mode from an interrupt: a thread asked to stop
/// ends here.
fn on_return() {
    if !sched::should_stop() {
        return;
    }
    Arch::enable_interrupts();
    if let Some((thread, user)) = current() {
        info!("User: thread {} ({}) stopped", thread.id(), thread.name());
        drop(thread);
        exit_current(user, EXIT_STOPPED);
    }
}

/// The first slot of the receive window programs start with, and its size.
const fn default_window() -> u64 {
    ((startup::SLOT_WINDOW as u64) << 32) | startup::WINDOW_SLOTS as u64
}

/// A first-in, first-out queue of waiters, which grows under spin locks
/// (with atomic allocations) and only ever says so when it cannot.
pub(crate) struct Fifo<T>(Vec<T, Kmalloc>);

impl<T> Fifo<T> {
    pub(crate) fn new() -> Fifo<T> {
        Fifo(Vec::new_in(HEAP.with(Flags::ATOMIC)))
    }

    /// Adds `item` at the back; gives it back if there is no memory.
    pub(crate) fn push_back(&mut self, item: T) -> core::result::Result<(), T> {
        if self.0.try_reserve(1).is_err() {
            return Err(item);
        }
        self.0.push_within_capacity(item).map(|_| ())
    }

    /// Puts `item` back at the front.
    pub(crate) fn push_front(&mut self, item: T) -> core::result::Result<(), T> {
        self.push_back(item)?;
        self.0.rotate_right(1);
        Ok(())
    }

    /// Takes the item at the front.
    pub(crate) fn pop_front(&mut self) -> Option<T> {
        if self.0.is_empty() { None } else { Some(self.0.remove(0)) }
    }

    /// Keeps only the items `keep` says to.
    pub(crate) fn retain(&mut self, keep: impl FnMut(&T) -> bool) {
        self.0.retain(keep);
    }

    pub(crate) fn len(&self) -> usize {
        self.0.len()
    }

    /// Takes every item, leaving it empty.
    pub(crate) fn take(&mut self) -> Fifo<T> {
        core::mem::replace(self, Fifo::new())
    }
}

impl<T> IntoIterator for Fifo<T> {
    type Item = T;
    type IntoIter = alloc::vec::IntoIter<T, Kmalloc>;

    fn into_iter(self) -> Self::IntoIter {
        self.0.into_iter()
    }
}
