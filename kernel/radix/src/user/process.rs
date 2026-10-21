// SPDX-FileCopyrightText: 2026 The OrthrOS contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Processes (Phase 6, radix-process(2)): an address space, a capability
//! space and the threads that run in them, which end together.
//!
//! A process is built by whoever holds its capability with the control
//! right: the kernel for Spark and its test programs (`init`, `launch`),
//! a loader in user space for every other program. It has two capability
//! spaces: the one its threads name capabilities in, and a hidden one
//! that keeps its address space's original capability (slot 0) and up to
//! `KEEP_SLOTS` capabilities its maker keeps for it, such as the frames
//! of its image, which its threads cannot reach.
//!
//! A process ends ([`ProcessObject::end`]) when its last thread exits,
//! when a fault stops one of its threads, when a holder of the control
//! right calls `exit`, or when its last capability is deleted. Its threads
//! are asked to stop, and whoever sees the last of them go finishes it
//! ([`ProcessObject::finish`]): both its capability spaces are destroyed,
//! which frees everything it held, its address space is let go, and the
//! notification `notify_exit` named is signalled. What is left is its
//! number and exit code until its last capability is deleted.
//!
//! A process made with `process.create` belongs to the process it was made
//! for, and ends when that one does ([`ProcessObject::finish`] ends the
//! processes it made). So whatever a program makes goes when it ends, even
//! a process that holds the only capability to itself, which nothing could
//! reach or end otherwise.
//!
//! Finishing needs the capability store, so a process that ends because
//! the store released it (its last capability went, under the store's
//! lock) is finished when the store's current change is done, before the
//! lock goes ([`finish_pending`], from `caps::with`). Processes waiting to
//! be finished are linked through themselves, so ending one never needs
//! memory.
//!
//! Lock order: the capability store, then a process's state, then a
//! thread's scheduler state (`sched::wake`).

use alloc::sync::{Arc, Weak};
use alloc::vec::Vec;
use core::mem::size_of;
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use radix_abi::address_space::USER_TOP;
use radix_abi::capability::ObjectKind as AbiKind;
use radix_abi::errors::Kernel;
use radix_abi::process::{
    self, EXIT_FAULT, EXIT_STOPPED, KEEP_SLOTS, MAX_SLOTS, MIN_SLOTS, NAME_MAX, ProcessState,
};
use radix_cap::{Derivation, ObjectKind, Rights, SLOT_BYTES, SlotRef, SpaceId};
use radix_hal::UserMode;
use radix_klog::info;
use radix_mm::{Flags, HEAP, Kmalloc, PAGE_SIZE};
use radix_sync::SpinLock;

use super::account::{Account, AccountRef, Charge};
use super::ipc::{self, Endpoint};
use super::memory::{SpaceObject, SpaceRef};
use super::methods::{Answer, answer};
use super::notify::{Notification, NotificationRef};
use super::objects::{self, Object};
use super::{
    Result, UserRef, UserThread, cap_error, default_window, error, set_ready, thread_main,
};
use crate::Arch;
use crate::caps::{self, Store};
use crate::sched::{self, Name, Options, SpawnError, ThreadRef, wait};

/// A process, shared.
pub type ProcessRef = Arc<ProcessObject, Kmalloc>;

/// How a thread of a process ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Ending {
    /// It exited (`thread.exit`), with a code.
    Exit,
    /// A fault ended it: the process ends with `EXIT_FAULT`.
    Fault,
    /// It was stopped.
    Stop,
}

/// A process.
pub struct ProcessObject {
    /// Its number, unique since boot.
    number: u64,
    /// Its name, which its threads take when they start.
    name: SpinLock<Name>,
    /// The capability space its threads use.
    cspace: SpaceId,
    /// The hidden capability space: its address space's original
    /// capability in slot 0, and what its maker keeps for it after.
    keep: SpaceId,
    /// Its address space, until it has ended.
    space: SpinLock<Option<SpaceRef>>,
    /// What objects it makes are charged to, and its threads run in.
    account: AccountRef,
    state: SpinLock<State>,
    /// Called once it has exited: Spark's end stops the kernel.
    on_exit: Option<fn(&ProcessObject, u64)>,
    /// The next process waiting to be finished, while it waits.
    pending: SpinLock<Option<ProcessRef>>,
    /// The processes made for it, which end when it does; `None` once it
    /// has ended, when no more can be made for it.
    made: SpinLock<Option<Vec<Weak<ProcessObject, Kmalloc>, Kmalloc>>>,
}

struct State {
    phase: ProcessState,
    /// The exit code, from when it began to end.
    code: u64,
    /// Its threads that have not exited.
    threads: Vec<ThreadRef, Kmalloc>,
    /// Signalled when it exits: a notification and the bits to set.
    notify: Option<(NotificationRef, u64)>,
    /// Someone has taken on finishing it.
    finishing: bool,
}

/// Numbers processes, from 1.
static NUMBERS: AtomicU64 = AtomicU64::new(1);

/// The first of the processes waiting to be finished, linked through
/// their `pending`.
static PENDING: SpinLock<Option<ProcessRef>> = SpinLock::new(None);
/// Whether any process waits to be finished, so that the store's changes
/// need not take [`PENDING`]'s lock to see that none does.
static ANY_PENDING: AtomicBool = AtomicBool::new(false);

fn kmalloc() -> Kmalloc {
    HEAP.with(Flags::MAY_SLEEP)
}

/// The kernel memory a process with `slots` slots takes, which `create`
/// charges: the object, its two capability spaces, and its address space
/// with the top page table. (The lower page tables, made as it maps, are
/// not charged yet; docs/design/processes.md lists that limit.)
pub fn bytes_of(slots: u32) -> u64 {
    let spaces = (slots as usize + KEEP_SLOTS as usize + 1) * SLOT_BYTES;
    (size_of::<ProcessObject>() + size_of::<SpaceObject>() + spaces) as u64 + PAGE_SIZE as u64
}

/// Makes a process with a capability space of `slots` slots, named
/// `name`, whose objects and threads are charged to `account`, and puts
/// its original capability, with every right, in `at`. `charge` is what
/// the process itself is charged to its maker; the registry keeps it
/// until the process goes. A process made for `maker` ends when `maker`
/// does; if `maker` has ended already, this fails with `gone`.
pub fn create(
    at: SlotRef,
    name: &str,
    slots: u32,
    account: AccountRef,
    charge: Option<Charge>,
    maker: Option<&ProcessObject>,
    on_exit: Option<fn(&ProcessObject, u64)>,
) -> Result<ProcessRef> {
    if !(MIN_SLOTS..=MAX_SLOTS).contains(&slots) {
        return Err(Kernel::InvalidArgument.status(1));
    }
    let space = SpaceObject::new().ok_or(error(Kernel::NoMemory))?;
    let mut threads = Vec::new_in(HEAP.with(Flags::ATOMIC));
    threads.try_reserve(1).map_err(|_| error(Kernel::NoMemory))?;
    caps::with(|store| -> Result<ProcessRef> {
        objects::reserve(store, 2).map_err(|_| error(Kernel::NoMemory))?;
        let cspace = store.create_space(slots).map_err(cap_error)?;
        let keep = match store.create_space(KEEP_SLOTS + 1) {
            Ok(keep) => keep,
            Err(err) => {
                let _ = store.destroy_space(cspace, &mut caps::release);
                return Err(cap_error(err));
            }
        };
        let undo = |store: &mut Store| {
            let _ = store.destroy_space(cspace, &mut caps::release);
            let _ = store.destroy_space(keep, &mut caps::release);
        };
        let made = (|| {
            let id =
                store.create(SlotRef::new(keep, 0), ObjectKind::AddressSpace).map_err(cap_error)?;
            objects::insert(store, id, Object::Space(space.clone()))
                .map_err(|_| error(Kernel::NoMemory))?;
            let process = ProcessObject {
                number: NUMBERS.fetch_add(1, Ordering::Relaxed),
                name: SpinLock::new(Name::new(name)),
                cspace,
                keep,
                space: SpinLock::new(Some(space)),
                account,
                state: SpinLock::new(State {
                    phase: ProcessState::Created,
                    code: 0,
                    threads,
                    notify: None,
                    finishing: false,
                }),
                on_exit,
                pending: SpinLock::new(None),
                made: SpinLock::new(Some(Vec::new_in(HEAP.with(Flags::ATOMIC)))),
            };
            let process: ProcessRef =
                Arc::try_new_in(process, kmalloc()).map_err(|_| error(Kernel::NoMemory))?;
            // Before its capability exists, so that a failure leaves
            // nothing to undo but the spaces.
            if let Some(maker) = maker {
                maker.made_one(&process)?;
            }
            let id = store.create(at, ObjectKind::Process).map_err(cap_error)?;
            // Room was reserved above, so this cannot fail.
            if objects::insert_charged(store, id, Object::Process(process.clone()), charge).is_err()
            {
                let _ = store.delete(at, &mut caps::release);
                return Err(error(Kernel::NoMemory));
            }
            Ok(process)
        })();
        if made.is_err() {
            undo(store);
        }
        made
    })
    .map_err(cap_error)
    .and_then(|made| made)
}

impl ProcessObject {
    /// Its number.
    pub fn number(&self) -> u64 {
        self.number
    }

    /// The capability space its threads use.
    pub fn cspace(&self) -> SpaceId {
        self.cspace
    }

    /// Its hidden capability space.
    pub fn keep_space(&self) -> SpaceId {
        self.keep
    }

    /// Records `process` as made for it, forgetting those that have gone.
    /// Fails with `gone` once it has ended.
    fn made_one(&self, process: &ProcessRef) -> Result<()> {
        let mut made = self.made.lock();
        let list = made.as_mut().ok_or(error(Kernel::Gone))?;
        // Only the record of one that has gone is let go of here: never the
        // last reference to a process, whose end may need to sleep.
        list.retain(|made| made.strong_count() > 0);
        list.try_reserve(1).map_err(|_| error(Kernel::NoMemory))?;
        // Room was reserved above.
        let _ = list.push_within_capacity(Arc::downgrade(process));
        Ok(())
    }

    /// Its address space, unless it has ended.
    pub fn space(&self) -> Option<SpaceRef> {
        self.space.lock().clone()
    }

    /// Where it is in its life, and its exit code once it has exited.
    pub fn state(&self) -> (ProcessState, Option<u64>) {
        let state = self.state.lock();
        (state.phase, (state.phase == ProcessState::Exited).then_some(state.code))
    }

    /// What waiters for its end wait on.
    pub fn key(&self) -> usize {
        self as *const ProcessObject as usize
    }

    /// Waits until it has exited, or the kernel's clock reaches
    /// `deadline_ns`. Returns whether it exited.
    pub fn wait_until(&self, deadline_ns: u64) -> bool {
        wait::wait_until_or_timeout(self.key(), "process", deadline_ns, || {
            self.state.lock().phase == ProcessState::Exited
        })
    }

    /// Starts a thread at `entry`, with its stack pointer at `stack` and
    /// `argument` in its first argument register, on CPU `cpu` only if
    /// there is one, and puts its capability in slot `to` of the process's
    /// capability space. `prepare` sees the thread before it may run.
    pub fn start(
        process: &ProcessRef,
        [entry, stack, argument]: [u64; 3],
        to: u32,
        cpu: Option<usize>,
        prepare: &mut dyn FnMut(&ThreadRef) -> Result<()>,
    ) -> Result<ThreadRef> {
        if entry >= USER_TOP || stack > USER_TOP {
            return Err(Kernel::InvalidArgument.status(0));
        }
        if !matches!(process.state.lock().phase, ProcessState::Created | ProcessState::Running) {
            return Err(error(Kernel::Gone));
        }
        let space = process.space().ok_or(error(Kernel::Gone))?;
        let user = UserThread {
            cspace: process.cspace,
            space,
            start: [entry, stack, argument],
            ready: AtomicBool::new(false),
            ipc: SpinLock::new(ipc::IpcState::Idle),
            reply: SpinLock::new(None),
            window: AtomicU64::new(default_window()),
            process: process.clone(),
        };
        let user: UserRef =
            Arc::try_new_in(user, kmalloc()).map_err(|_| error(Kernel::NoMemory))?;
        let mut options =
            Options::new().domain(process.account.domain().clone()).user(user.clone());
        if let Some(cpu) = cpu {
            options = options.pinned(cpu);
        }
        let name = *process.name.lock();
        let thread =
            sched::spawn(name.as_str(), thread_main, 0, options).map_err(|err| match err {
                SpawnError::Stopping => error(Kernel::Gone),
                SpawnError::NoCpu | SpawnError::InvalidPolicy => Kernel::InvalidArgument.status(0),
                _ => error(Kernel::NoMemory),
            })?;
        // From here on the thread waits in the kernel: on a failure it is
        // let go to stop at once.
        let fail = |err| {
            thread.request_stop();
            set_ready(&user);
            err
        };
        prepare(&thread).map_err(fail)?;
        install(
            SlotRef::new(process.cspace, to),
            ObjectKind::Thread,
            Object::Thread(thread.clone()),
        )
        .map_err(fail)?;
        {
            let mut state = process.state.lock();
            if !matches!(state.phase, ProcessState::Created | ProcessState::Running) {
                drop(state);
                return Err(fail(error(Kernel::Gone)));
            }
            if state.threads.try_reserve(1).is_err() {
                drop(state);
                return Err(fail(error(Kernel::NoMemory)));
            }
            // Room was reserved above.
            let _ = state.threads.push_within_capacity(thread.clone());
            state.phase = ProcessState::Running;
        }
        set_ready(&user);
        Ok(thread)
    }

    /// Ends it with exit code `code`, unless it is ending already, as
    /// `process.exit` does: its threads are asked to stop, and it is
    /// finished now if none is left to.
    pub fn exit(&self, code: u64) {
        if self.end(code) {
            self.finish(None);
        }
    }

    /// Ends it with exit code `code`, unless it is ending already: its
    /// threads are asked to stop. Returns whether the caller is to finish
    /// it ([`ProcessObject::finish`]), because no thread is left to.
    fn end(&self, code: u64) -> bool {
        let mut state = self.state.lock();
        if matches!(state.phase, ProcessState::Created | ProcessState::Running) {
            state.phase = ProcessState::Ending;
            state.code = code;
            for thread in &state.threads {
                thread.request_stop();
                sched::wake(thread);
            }
        }
        Self::take_finish(&mut state)
    }

    /// Whether the caller is to finish it: it is ending, with no thread
    /// left, and nobody else has taken that on.
    fn take_finish(state: &mut State) -> bool {
        let finish =
            state.phase == ProcessState::Ending && state.threads.is_empty() && !state.finishing;
        if finish {
            state.finishing = true;
        }
        finish
    }

    /// Thread `me` of the process has ended, with exit status `status`.
    /// The last thread to go ends the process with its status, and a
    /// fault ends it at once; the last thread of an ending process then
    /// finishes it. Called on the thread itself, which may sleep.
    pub fn thread_exited(&self, me: &ThreadRef, status: u64, how: Ending) {
        let (finish, gone) = {
            let mut state = self.state.lock();
            let Some(index) = state.threads.iter().position(|t| ThreadRef::ptr_eq(t, me)) else {
                // It was never added: its start failed.
                return;
            };
            let gone = state.threads.swap_remove(index);
            let running = state.phase == ProcessState::Running;
            if running && how == Ending::Fault {
                state.phase = ProcessState::Ending;
                state.code = EXIT_FAULT;
                for thread in &state.threads {
                    thread.request_stop();
                    sched::wake(thread);
                }
            } else if running && state.threads.is_empty() {
                state.phase = ProcessState::Ending;
                state.code = status;
            }
            (Self::take_finish(&mut state), gone)
        };
        drop(gone);
        if finish {
            self.finish(None);
        }
    }

    /// Finishes ending it: ends the processes made for it, empties both
    /// its capability spaces, lets go of its address space, and tells
    /// whoever asked. With `store`, the caller holds the store's lock
    /// (`finish_pending`).
    fn finish(&self, store: Option<&mut Store>) {
        // The processes made for it end too. Those with no thread left are
        // finished through the pending list, as released ones are, so that
        // a long line of processes made for each other is finished one
        // after another and never in a deepening recursion: the store's
        // next change finishes them (here `caps::destroy_space` below, or
        // with the store's lock held, the rest of `finish_pending`).
        let made = self.made.lock().take();
        for process in made.iter().flatten().filter_map(Weak::upgrade) {
            if process.end(EXIT_STOPPED) {
                push_pending(process);
            }
        }
        drop(made);
        match store {
            Some(store) => {
                let _ = store.destroy_space(self.cspace, &mut caps::release);
                let _ = store.destroy_space(self.keep, &mut caps::release);
            }
            None => {
                let _ = caps::destroy_space(self.cspace);
                let _ = caps::destroy_space(self.keep);
            }
        }
        // Its threads' own references keep it alive while they finish
        // leaving it.
        let space = self.space.lock().take();
        drop(space);
        let (code, notify) = {
            let mut state = self.state.lock();
            state.phase = ProcessState::Exited;
            (state.code, state.notify.take())
        };
        if let Some((notification, badge)) = notify {
            notification.notify(badge);
        }
        if code != 0 {
            let name = *self.name.lock();
            info!("User: process {} ({name}) exited with code {code:#x}", self.number);
        }
        wait::wake_all(self.key());
        if let Some(on_exit) = self.on_exit {
            on_exit(self, code);
        }
    }
}

/// The store released a process: its last capability is gone, so it
/// ends, with `EXIT_STOPPED` unless it was ending already. Called with
/// the store locked: if it is to be finished now, [`finish_pending`]
/// does that before the lock goes.
pub fn released(process: ProcessRef) {
    if process.end(EXIT_STOPPED) {
        push_pending(process);
    }
}

/// Puts `process`, which has ended with no thread left, on the list of
/// those [`finish_pending`] finishes.
fn push_pending(process: ProcessRef) {
    let mut first = PENDING.lock();
    *process.pending.lock() = first.take();
    *first = Some(process);
    ANY_PENDING.store(true, Ordering::Release);
}

/// Finishes the processes the store released, with the store locked:
/// `caps::with` calls it after every change. Finishing one may release
/// more, which are finished too.
pub fn finish_pending(store: &mut Store) {
    if !ANY_PENDING.load(Ordering::Acquire) {
        return;
    }
    loop {
        let next = {
            let mut first = PENDING.lock();
            let next = first.take();
            match &next {
                Some(process) => *first = process.pending.lock().take(),
                None => ANY_PENDING.store(false, Ordering::Release),
            }
            next
        };
        match next {
            Some(process) => process.finish(Some(store)),
            None => return,
        }
    }
}

/// Makes an object of `kind` with its original capability in `at`, and
/// what it costs charged with `charge`.
pub fn install_charged(
    at: SlotRef,
    kind: ObjectKind,
    object: Object,
    charge: Option<Charge>,
) -> Result<()> {
    caps::with(|store| -> Result<()> {
        objects::reserve(store, 1).map_err(|_| error(Kernel::NoMemory))?;
        let id = store.create(at, kind).map_err(cap_error)?;
        objects::insert_charged(store, id, object, charge).map_err(|_| error(Kernel::NoMemory))
    })
    .map_err(cap_error)
    .and_then(|made| made)
}

/// Makes an object of `kind` with its original capability in `at`.
pub fn install(at: SlotRef, kind: ObjectKind, object: Object) -> Result<()> {
    install_charged(at, kind, object, None)
}

/// The process interface.
pub fn method(user: &UserRef, slot: u32, label: u32, args: &[u64; 8]) -> Result<Answer> {
    let needs = if label == process::INFO { Rights::READ } else { Rights::CONTROL };
    let (_, target) = objects::process(user.cspace, slot, needs)?;
    let at = |slot| SlotRef::new(user.cspace, slot);
    match label {
        process::CREATE => {
            let a = process::CreateArgs::decode(args)?;
            create_for(&target, a.kind, a.size, at(a.to))?;
            answer(label, 0, [0; 8])
        }
        process::SET_NAME => {
            let a = process::SetNameArgs::decode(args)?;
            if a.name.len > NAME_MAX {
                return Err(Kernel::InvalidArgument.status(0));
            }
            let len = a.name.len as usize;
            let mut bytes = [0u8; NAME_MAX as usize];
            if !radix_ipc::range::user_bytes(a.name.address, a.name.len)
                || !Arch::copy_from_user(&mut bytes[..len], a.name.address)
            {
                return Err(error(Kernel::BadAddress));
            }
            let name = core::str::from_utf8(&bytes[..len])
                .map_err(|_| Kernel::InvalidArgument.status(0))?;
            *target.name.lock() = Name::new(name);
            answer(label, 0, [0; 8])
        }
        process::ADDRESS_SPACE => {
            let a = process::AddressSpaceArgs::decode(args)?;
            let how =
                Derivation { rights: ObjectKind::AddressSpace.rights(), badge: None, range: None };
            caps::with(|store| store.derive(SlotRef::new(target.keep, 0), at(a.to), how))
                .and_then(|done| done)
                .map_err(cap_error)?;
            answer(label, 0, [0; 8])
        }
        process::GIVE => {
            let a = process::GiveArgs::decode(args)?;
            let to = SlotRef::new(target.cspace, a.to);
            caps::with(|store| store.move_cap(at(a.from), to))
                .and_then(|done| done)
                .map_err(cap_error)?;
            answer(label, 0, [0; 8])
        }
        process::SHARE => {
            let a = process::ShareArgs::decode(args)?;
            let how = Derivation {
                rights: super::memory::cap_rights(a.rights),
                badge: (a.badge != 0).then_some(a.badge),
                range: None,
            };
            let to = SlotRef::new(target.cspace, a.to);
            caps::with(|store| store.derive(at(a.from), to, how))
                .and_then(|done| done)
                .map_err(cap_error)?;
            answer(label, 0, [0; 8])
        }
        process::KEEP => {
            let a = process::KeepArgs::decode(args)?;
            keep(&target, at(a.from))?;
            answer(label, 0, [0; 8])
        }
        process::START => {
            let a = process::StartArgs::decode(args)?;
            let thread = ProcessObject::start(
                &target,
                [a.entry, a.stack, a.argument],
                a.to,
                None,
                &mut |_| Ok(()),
            )?;
            let results = process::StartResults { thread: thread.id() };
            answer(label, process::StartResults::WORDS, results.encode())
        }
        process::EXIT => {
            let a = process::ExitArgs::decode(args)?;
            target.exit(a.code);
            if ProcessRef::ptr_eq(&target, &user.process) {
                return Ok(Answer::Exit(a.code));
            }
            answer(label, 0, [0; 8])
        }
        process::NOTIFY_EXIT => {
            let a = process::NotifyExitArgs::decode(args)?;
            let (cap, notification) =
                objects::notification(user.cspace, a.notification, Rights::SEND)?;
            let badge = cap.badge();
            let now = {
                let mut state = target.state.lock();
                if state.phase == ProcessState::Exited {
                    true
                } else {
                    state.notify = Some((notification.clone(), badge));
                    false
                }
            };
            if now {
                notification.notify(badge);
            }
            answer(label, 0, [0; 8])
        }
        process::INFO => {
            process::InfoArgs::decode(args)?;
            let state = target.state.lock();
            let results = process::InfoResults {
                state: state.phase,
                code: if state.phase == ProcessState::Exited { state.code } else { 0 },
                number: target.number,
                threads: state.threads.len() as u32,
            };
            drop(state);
            answer(label, process::InfoResults::WORDS, results.encode())
        }
        _ => Err(error(Kernel::NoSuchMethod)),
    }
}

/// `process.create`: makes an object of `kind` charged to `target`, with
/// its capability in `to`.
fn create_for(target: &ProcessRef, kind: AbiKind, size: u32, to: SlotRef) -> Result<()> {
    if !matches!(target.state.lock().phase, ProcessState::Created | ProcessState::Running) {
        return Err(error(Kernel::Gone));
    }
    match kind {
        AbiKind::Process => {
            if !(MIN_SLOTS..=MAX_SLOTS).contains(&size) {
                return Err(Kernel::InvalidArgument.status(1));
            }
            let charge = Account::charge(&target.account, bytes_of(size))?;
            let account =
                Account::new(target.account.domain().clone()).ok_or(error(Kernel::NoMemory))?;
            create(to, "process", size, account, Some(charge), Some(&**target), None).map(|_| ())
        }
        AbiKind::Endpoint => {
            if size != 0 {
                return Err(Kernel::InvalidArgument.status(1));
            }
            let charge = Account::charge(&target.account, size_of::<Endpoint>() as u64)?;
            let endpoint = Endpoint::new().ok_or(error(Kernel::NoMemory))?;
            install_charged(to, ObjectKind::Endpoint, Object::Endpoint(endpoint), Some(charge))
        }
        AbiKind::Notification => {
            if size != 0 {
                return Err(Kernel::InvalidArgument.status(1));
            }
            let charge = Account::charge(&target.account, size_of::<Notification>() as u64)?;
            let notification = Notification::new().ok_or(error(Kernel::NoMemory))?;
            let object = Object::Notification(notification);
            install_charged(to, ObjectKind::Notification, object, Some(charge))
        }
        _ => Err(Kernel::InvalidArgument.status(0)),
    }
}

/// `process.keep`: moves the capability in `from` to the first free slot
/// among `target`'s kept ones.
pub fn keep(target: &ProcessObject, from: SlotRef) -> Result<()> {
    caps::with(|store| -> Result<()> {
        for slot in 1..=KEEP_SLOTS {
            let to = SlotRef::new(target.keep, slot);
            match store.get(to) {
                Err(radix_cap::CapError::Empty) => {
                    return store.move_cap(from, to).map_err(cap_error);
                }
                Err(radix_cap::CapError::NoSuchSpace) => return Err(error(Kernel::Gone)),
                _ => {}
            }
        }
        // The capability to keep must still be there and movable before
        // the lack of room is the answer.
        store.get(from).map_err(cap_error)?;
        Err(error(Kernel::NoRoom))
    })
    .map_err(cap_error)
    .and_then(|done| done)
}
