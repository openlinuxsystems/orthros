// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! POSIX processes (Phase 8, radix-posix(7)): the process tree, signals,
//! `fork`, `exec`, `wait`, process groups and sessions, timers, clocks,
//! `poll`, scheduling and resource limits, as the methods from 11 on of
//! radix-process(2).
//!
//! The rules live in `radix-posix`, which knows nothing of the kernel and
//! is tested on the host: the tree ([`radix_posix::tree::Tree`]), signal
//! actions and pending sets ([`radix_posix::signal`]), wait statuses, and
//! the layout of a new program's stack. This module holds them for the
//! kernel's processes and carries them out:
//!
//! - [`TREE`] is the one process tree, holding each member's
//!   `ProcessRef`, so that a zombie lasts until its parent waits for it.
//!   A process joins it when it is forked, when `exec` starts a created
//!   one, or when it first calls a method that needs it, as the root of a
//!   session of its own (Spark and what it starts the Phase 6 way);
//! - each process has a [`PosixState`]: its signal actions and pending
//!   signals, its job-control stop, its timers and children's times, its
//!   controlling terminal, its resource limits and file mode mask; each
//!   thread its [`ThreadSignals`] and its [`priority::Scheduling`];
//! - `signal` sends and delivers signals, `fork` and `exec` make and
//!   replace programs, `timer` runs the timers, `poll` waits on many
//!   capabilities at once, `priority` carries POSIX's scheduling calls
//!   to the scheduler, and `limits` keeps the resource limits.
//!
//! A process is reported to its parent when it finishes
//! ([`exited`], from `ProcessObject::finish`), which may run with the
//! capability store locked; so nothing that holds [`TREE`] ever takes the
//! store, and signals are only sent with the tree's lock let go, since
//! sending one may end a process.
//!
//! Lock order: the capability store, then [`TREE`], then a process's
//! POSIX state, then its state (its thread list), then a thread's signals,
//! then a thread's scheduler state.

mod exec;
mod fork;
pub mod limits;
pub mod poll;
pub mod priority;
pub mod signal;
pub mod timer;

use alloc::boxed::Box;
use alloc::vec::Vec;
use core::sync::atomic::Ordering;

use radix_abi::errors::Kernel;
use radix_abi::process::{self, ChildWaitFlags, ClockKind};
use radix_abi::signal::{SIGCHLD, SIGNALS};
use radix_abi::startup;
use radix_cap::Rights;
use radix_hal::UserMode;
use radix_mm::{Flags, HEAP, Kmalloc};
use radix_posix::signal::{Action, Actions, Info, Pending};
use radix_posix::status::Ended;
use radix_posix::tree::{Found, Tree, TreeError, WaitFor, Which};
use radix_sync::Mutex;

pub use signal::ThreadSignals;

use super::methods::{Answer, answer};
use super::objects;
use super::process::{ProcessObject, ProcessRef};
use super::{Result, UserRef, cap_error, error};
use crate::sched::{self, Name, ThreadRef, wait};
use crate::{Arch, caps};

/// The process tree, made when first needed.
static TREE: Mutex<Option<Tree<ProcessRef, Kmalloc>>> = Mutex::new(None);

fn kmalloc() -> Kmalloc {
    HEAP.with(Flags::MAY_SLEEP)
}

/// Runs `f` on the tree, with its lock held.
fn with_tree<R>(f: impl FnOnce(&mut Tree<ProcessRef, Kmalloc>) -> R) -> R {
    let mut tree = TREE.lock();
    f(tree.get_or_insert_with(|| Tree::new(kmalloc())))
}

/// Signal actions, kept on the kernel's heap: the kernel's stack frames
/// are too small for them.
pub type SignalActions = Actions<Vec<Action, Kmalloc>>;

/// Pending signals, kept on the kernel's heap like [`SignalActions`]. The
/// real-time ones are queued as they are sent, often with a lock held
/// that may not sleep: their memory is taken without sleeping.
pub type PendingSignals = Pending<Vec<Info, Kmalloc>, Kmalloc>;

/// `count` copies of `value` on the kernel's heap.
fn filled<T: Copy>(value: T, count: usize) -> Option<Vec<T, Kmalloc>> {
    let mut storage = Vec::new_in(kmalloc());
    storage.try_reserve_exact(count).ok()?;
    while storage.len() < count {
        // Room was reserved above.
        let _ = storage.push_within_capacity(value);
    }
    Some(storage)
}

/// Every signal at its default action. `None` if there is no memory.
pub fn new_actions() -> Option<SignalActions> {
    Actions::in_storage(filled(Action::default(), SIGNALS as usize)?)
}

/// Nothing pending. `None` if there is no memory.
pub fn new_pending() -> Option<PendingSignals> {
    Pending::in_storage(filled(Info::default(), SIGNALS as usize)?, HEAP.with(Flags::ATOMIC))
}

/// A process's POSIX side.
pub struct PosixState {
    /// What each signal does.
    pub actions: SignalActions,
    /// The signals sent to the process, taken by whichever of its threads
    /// does not block them first.
    pub pending: PendingSignals,
    /// The signal that stopped it (job control), until it continues.
    pub stopped: Option<u32>,
    /// The signal that ended it, if one did.
    pub ended_by: Option<u32>,
    /// Whether it is in the tree.
    pub in_tree: bool,
    /// The CPU time of the children it waited for, and theirs (user mode
    /// and in all).
    pub children_ns: (u64, u64),
    /// Its interval timers: real, user and profiling time.
    pub timers: [timer::Timer; 3],
    /// Its POSIX timers (`timer_create`), by number.
    pub posix_timers: Vec<Option<timer::PosixTimer>, Kmalloc>,
    /// Its session's controlling terminal, as it was when it last looked:
    /// the terminal says whose it is now (`crate::fs::tty`).
    pub terminal: Option<crate::fs::tty::TtyRef>,
    /// Its resource limits, by LimitKind (on the heap: the kernel's stack
    /// frames are too small for them).
    pub limits: Box<limits::Limits, Kmalloc>,
    /// Its file mode creation mask (`umask`).
    pub file_mask: u32,
    /// Its working directory as the C library last noted it (`exec`,
    /// `set_directory`): an absolute path, for `/proc`. Shared, so that
    /// `fork` copies it under the lock without taking memory.
    pub directory: Option<Directory>,
}

/// A working directory's path, as [`PosixState::directory`] keeps it.
pub type Directory = alloc::sync::Arc<Vec<u8, Kmalloc>, Kmalloc>;

/// Notes `path` as `process`'s working directory, if there is memory for
/// it (`exec`, and radix-process(2)'s `set_directory`).
pub fn set_directory(process: &ProcessObject, path: &[u8]) -> bool {
    exec::copied(path).is_ok_and(|copy| keep_directory(process, copy))
}

/// [`set_directory`] with the path copied already.
fn keep_directory(process: &ProcessObject, path: Vec<u8, Kmalloc>) -> bool {
    let Ok(directory) = alloc::sync::Arc::try_new_in(path, kmalloc()) else { return false };
    let old = process.posix.lock().directory.replace(directory);
    drop(old);
    true
}

impl PosixState {
    /// A process's POSIX side before it has done anything POSIX: every
    /// signal's default action, nothing pending. `None` if there is no
    /// memory for it.
    pub fn new() -> Option<PosixState> {
        Some(PosixState {
            actions: new_actions()?,
            pending: new_pending()?,
            stopped: None,
            ended_by: None,
            in_tree: false,
            children_ns: (0, 0),
            timers: [timer::Timer::OFF; 3],
            posix_timers: Vec::new_in(kmalloc()),
            terminal: None,
            limits: Box::try_new_in(limits::Limits::DEFAULT, kmalloc()).ok()?,
            file_mask: 0o022,
            directory: None,
        })
    }

    /// Takes what a child has of its parent's `parent` (`fork`, or `exec`
    /// on a created process): its resource limits, file mode mask and,
    /// unless `exec` noted its own, working directory.
    pub fn inherit(&mut self, parent: &PosixState) {
        *self.limits = *parent.limits;
        self.file_mask = parent.file_mask;
        if self.directory.is_none() {
            self.directory.clone_from(&parent.directory);
        }
    }
}

/// What waiters for a change in `process`'s children wait on.
fn child_key(process: &ProcessObject) -> usize {
    process.key() + 1
}

/// What `process`'s stopped threads wait on, to continue.
pub(super) fn stop_key(process: &ProcessObject) -> usize {
    process.key() + 2
}

/// What `signal_wait` waits on, for a signal sent to `process` or one of
/// its threads.
pub(super) fn signal_key(process: &ProcessObject) -> usize {
    process.key() + 3
}

/// The status an error of the tree becomes.
fn tree_error(err: TreeError) -> radix_abi::Status {
    error(match err {
        TreeError::NoSuchProcess => Kernel::NoSuchProcess,
        TreeError::NotPermitted => Kernel::NotPermitted,
        TreeError::Execed => Kernel::Execed,
        TreeError::Invalid => Kernel::InvalidArgument,
        TreeError::NoMemory => Kernel::NoMemory,
        TreeError::Exists => Kernel::Occupied,
    })
}

/// Puts `process` in the tree as the root of a session of its own, unless
/// it is in it already. With the tree locked.
fn join(tree: &mut Tree<ProcessRef, Kmalloc>, process: &ProcessRef) -> Result<()> {
    if tree.get(process.number()).is_some() {
        return Ok(());
    }
    tree.insert_root(process.number(), process.clone()).map_err(tree_error)?;
    process.posix.lock().in_tree = true;
    Ok(())
}

/// `process`'s number, process group and session, putting it in the tree
/// if it is not there.
pub fn ids_of(process: &ProcessRef) -> Result<(u64, u64, u64)> {
    with_tree(|tree| {
        join(tree, process)?;
        let member = tree.get(process.number()).ok_or(error(Kernel::NoSuchProcess))?;
        Ok((member.pid, member.group, member.session))
    })
}

/// Where process `pid` is in the tree, for `/proc`: its parent, process
/// group and session, and whether it has ended unreaped (a zombie); none
/// if it is not in the tree.
pub fn tree_place(pid: u64) -> Option<(u64, u64, u64, bool)> {
    with_tree(|tree| {
        let member = tree.get(pid)?;
        let zombie = matches!(member.life, radix_posix::tree::Life::Zombie { .. });
        Some((member.parent, member.group, member.session, zombie))
    })
}

/// The process numbered `pid` if it has ended and its parent has not
/// waited for it (a zombie), for `/proc`: the registry holds a process
/// only while a capability names it, which a zombie's seldom does.
pub fn zombie(pid: u64) -> Option<ProcessRef> {
    with_tree(|tree| {
        let member = tree.get(pid)?;
        matches!(member.life, radix_posix::tree::Life::Zombie { .. }).then(|| member.handle.clone())
    })
}

/// Adds the numbers of the zombies to `numbers`, as many as it has room
/// for after room for those there are now was asked for.
pub fn zombie_numbers(numbers: &mut Vec<u64, Kmalloc>) {
    let is_zombie = |m: &radix_posix::tree::Member<ProcessRef>| {
        matches!(m.life, radix_posix::tree::Life::Zombie { .. })
    };
    let count = with_tree(|tree| tree.members().filter(|m| is_zombie(m)).count());
    if numbers.try_reserve(count).is_err() {
        return;
    }
    with_tree(|tree| {
        for member in tree.members().filter(|m| is_zombie(m)) {
            if numbers.push_within_capacity(member.pid).is_err() {
                break;
            }
        }
    });
}

/// Whether process group `group` has a live member in session `session`.
pub fn group_in_session(group: u64, session: u64) -> bool {
    with_tree(|tree| {
        tree.members().any(|m| {
            m.group == group
                && m.session == session
                && !matches!(m.life, radix_posix::tree::Life::Zombie { .. })
        })
    })
}

/// The process numbered `pid` in the tree, if it is alive.
fn find(tree: &Tree<ProcessRef, Kmalloc>, pid: u64) -> Option<ProcessRef> {
    let member = tree.get(pid)?;
    (!matches!(member.life, radix_posix::tree::Life::Zombie { .. })).then(|| member.handle.clone())
}

/// The thread of `process` numbered `number`, and its program side. The
/// last match: a thread that ran exec, ending, has the number of the one
/// that runs the new program.
pub(super) fn find_thread(process: &ProcessObject, number: u64) -> Option<(ThreadRef, UserRef)> {
    let mut found = None;
    process.for_each_thread(&mut |thread, user| {
        if thread.id() == number {
            found = Some((thread.clone(), user.clone()));
        }
    });
    found
}

/// Thread `number` of `process` (0: the calling one): ESRCH for none.
fn own_thread(process: &ProcessObject, number: u64) -> Result<ThreadRef> {
    if number == 0 {
        return sched::current().ok_or(error(Kernel::Gone));
    }
    find_thread(process, number).map(|(thread, _)| thread).ok_or(error(Kernel::NoSuchProcess))
}

/// `process`'s CPU time so far: its threads', live and exited.
pub fn cpu_ns(process: &ProcessObject) -> u64 {
    process.cpu_ns()
}

/// `process` has finished: its parent hears of it (SIGCHLD, and its
/// `wait`), its children lose it, and if it led a session with a
/// controlling terminal, the terminal's foreground process group gets
/// SIGHUP. Called from `ProcessObject::finish`, perhaps with the store
/// locked.
pub fn exited(process: &ProcessObject) {
    let (in_tree, ended, terminal) = {
        let mut posix = process.posix.lock();
        posix.timers = [timer::Timer::OFF; 3];
        posix.posix_timers.clear();
        let (_, code) = process.state();
        let code = code.unwrap_or(0);
        let ended = match posix.ended_by {
            Some(signal) => Ended::Killed(signal),
            None => Ended::Exited(code),
        };
        (posix.in_tree, ended, posix.terminal.take())
    };
    crate::fs::trace::exited(process, ended.status());
    let pid = process.number();
    if let Some(terminal) = terminal {
        // A session leader's end hangs its terminal up.
        terminal.leader_exited(pid);
    }
    if !in_tree {
        return;
    }
    let times = {
        let (own_user, own_system) = process.cpu_times();
        let (user, all) = process.posix.lock().children_ns;
        (own_user + user, own_user + own_system + all)
    };
    // The handles of members that left: dropped once the tree is let go.
    let mut gone: Vec<ProcessRef, Kmalloc> = Vec::new_in(kmalloc());
    let mut leftover = None;
    let (parent, hangups) = with_tree(|tree| {
        let Some(me) = tree.get(pid) else { return (None, Vec::new_in(kmalloc())) };
        let session = me.session;
        let parent = find(tree, me.parent);
        let auto_reap =
            parent.as_ref().is_some_and(|parent| parent.posix.lock().actions.reaps_children());
        // Groups this end may leave orphaned with stopped members: its
        // own, and its children's, each with whether it is orphaned now.
        // Only a group there is no memory to note is left out.
        let mut groups: Vec<(u64, bool), Kmalloc> = Vec::new_in(kmalloc());
        for member in tree.members() {
            if (member.pid == pid || member.parent == pid)
                && member.session == session
                && !groups.iter().any(|&(group, _)| group == member.group)
                && groups.try_reserve(1).is_ok()
            {
                let _ = groups.push_within_capacity((member.group, tree.orphaned(member.group)));
            }
        }
        tree.exit(pid, ended, times, auto_reap, &mut |handle| {
            if gone.try_reserve(1).is_ok() {
                let _ = gone.push_within_capacity(handle);
            } else {
                // No room to put it aside: it goes now, which only frees
                // memory (a zombie has let go of everything else).
                leftover = Some(handle);
            }
        });
        // POSIX: a process group newly orphaned with a stopped member gets
        // SIGHUP and SIGCONT.
        groups.retain(|&(group, was)| !was && tree.orphaned(group) && tree.has_stopped(group));
        (parent, groups)
    });
    drop(leftover);
    drop(gone);
    if let Some(parent) = parent {
        let (code, status) = match ended {
            Ended::Exited(code) => (radix_posix::signal::CLD_EXITED, code & 0xff),
            Ended::Killed(signal) => (radix_posix::signal::CLD_KILLED, u64::from(signal)),
        };
        let info = radix_posix::signal::Info { code, pid, status };
        signal::send_process(&parent, SIGCHLD, info);
        children_changed(&parent);
    }
    for &(group, _) in &hangups {
        let info =
            radix_posix::signal::Info { code: radix_posix::signal::SI_KERNEL, pid: 0, status: 0 };
        signal::send_group(group, radix_abi::signal::SIGHUP, info);
        signal::send_group(group, radix_abi::signal::SIGCONT, info);
    }
}

/// One of `process`'s children changed: its `wait` looks again, and its
/// `notify_children` notification is signalled.
pub(super) fn children_changed(process: &ProcessObject) {
    process.child_events.fetch_add(1, Ordering::AcqRel);
    wait::wake_all(child_key(process));
    let notify = process.child_notify.lock().clone();
    if let Some((notification, badge)) = notify {
        notification.notify(badge);
    }
}

/// The methods of radix-process(2) from `fork` on, called by `user`'s
/// thread on `target`, whose capability is in its slot `slot`: its own
/// process, or for `exec` one `create` made.
pub fn method(
    user: &UserRef,
    slot: u32,
    target: &ProcessRef,
    label: u32,
    args: &[u64; 8],
) -> Result<Answer> {
    let me = &user.process;
    // These act on the caller's own process; `exec` may also start a
    // created one.
    if !ProcessRef::ptr_eq(target, me) && label != process::EXEC {
        return Err(error(Kernel::InvalidArgument));
    }
    match label {
        process::FORK => {
            process::ForkArgs::decode(args)?;
            let child = fork::fork(user, label)?;
            answer(label, process::ForkResults::WORDS, process::ForkResults { child }.encode())
        }
        process::EXEC => {
            let a = process::ExecArgs::decode(args)?;
            exec::exec(user, target, slot, label, a.file, a.block)
        }
        process::WAIT => {
            let a = process::WaitArgs::decode(args)?;
            let (pid, status, user_ns, system_ns) = wait_child(me, a.pid, a.flags)?;
            let results = process::WaitResults { pid, status, user_ns, system_ns };
            answer(label, process::WaitResults::WORDS, results.encode())
        }
        process::KILL => {
            let a = process::KillArgs::decode(args)?;
            let info = Info { code: radix_posix::signal::SI_USER, pid: me.number(), status: 0 };
            kill(me, a.pid, a.signal, info)?;
            answer(label, 0, [0; 8])
        }
        process::SIGNAL_QUEUE => {
            let a = process::SignalQueueArgs::decode(args)?;
            // One process, as `sigqueue` names it.
            let pid = i64::try_from(a.pid).ok().filter(|&pid| pid > 0);
            let pid = pid.ok_or(error(Kernel::NoSuchProcess))?;
            let info =
                Info { code: radix_posix::signal::SI_QUEUE, pid: me.number(), status: a.value };
            kill(me, pid, a.signal, info)?;
            answer(label, 0, [0; 8])
        }
        process::IDS => {
            let a = process::IdsArgs::decode(args)?;
            let results = with_tree(|tree| -> Result<process::IdsResults> {
                join(tree, me)?;
                let pid = if a.pid == 0 { me.number() } else { a.pid };
                let member = tree.get(pid).ok_or(error(Kernel::NoSuchProcess))?;
                Ok(process::IdsResults {
                    pid,
                    parent: member.parent,
                    group: member.group,
                    session: member.session,
                })
            })?;
            answer(label, process::IdsResults::WORDS, results.encode())
        }
        process::SET_GROUP => {
            let a = process::SetGroupArgs::decode(args)?;
            with_tree(|tree| -> Result<()> {
                join(tree, me)?;
                tree.set_group(me.number(), a.pid, a.group).map_err(tree_error)
            })?;
            answer(label, 0, [0; 8])
        }
        process::NEW_SESSION => {
            process::NewSessionArgs::decode(args)?;
            let session = with_tree(|tree| -> Result<u64> {
                join(tree, me)?;
                tree.new_session(me.number()).map_err(tree_error)
            })?;
            // A new session has no controlling terminal.
            let old = me.posix.lock().terminal.take();
            drop(old);
            answer(
                label,
                process::NewSessionResults::WORDS,
                process::NewSessionResults { session }.encode(),
            )
        }
        process::SIGNAL_ACTION
        | process::SIGNAL_RETURN
        | process::SIGNAL_MASK
        | process::SIGNAL_PENDING
        | process::SIGNAL_STACK
        | process::SIGNAL_SUSPEND
        | process::SIGNAL_WAIT
        | process::SIGNAL_THREAD => signal::method(user, label, args),
        process::TIMER_CREATE | process::TIMER_SET | process::TIMER_GET | process::TIMER_DELETE => {
            timer::method(user, label, args)
        }
        process::PRIORITY
        | process::SCHEDULING
        | process::SET_AFFINITY
        | process::AFFINITY
        | process::IO_PRIORITY => priority::method(user, label, args),
        process::SET_DIRECTORY => {
            let a = process::SetDirectoryArgs::decode(args)?;
            if a.path.len > radix_vfs::PATH_MAX as u64 + 1 {
                return Err(Kernel::InvalidArgument.status(0));
            }
            if !radix_ipc::range::user_bytes(a.path.address, a.path.len) {
                return Err(error(Kernel::BadAddress));
            }
            let mut path = exec::zeroed(a.path.len as usize)?;
            if !Arch::copy_from_user(&mut path, a.path.address) {
                return Err(error(Kernel::BadAddress));
            }
            if path.first() != Some(&b'/') || path.contains(&0) {
                return Err(Kernel::InvalidArgument.status(0));
            }
            if !keep_directory(me, path) {
                return Err(error(Kernel::NoMemory));
            }
            answer(label, 0, [0; 8])
        }
        process::NOTIFY_CHILDREN => {
            let a = process::NotifyChildrenArgs::decode(args)?;
            let (cap, notification) =
                objects::notification(user.cspace, a.notification, Rights::SEND)?;
            *target.child_notify.lock() = Some((notification, cap.badge()));
            answer(label, 0, [0; 8])
        }
        process::TRACE => {
            let a = process::TraceArgs::decode(args)?;
            let target = with_tree(|tree| -> Result<(ProcessRef, bool)> {
                join(tree, me)?;
                let target = find(tree, a.pid).ok_or(error(Kernel::NoSuchProcess))?;
                Ok((target, tree.may_signal(me.number(), a.pid)))
            })?;
            let (target, permitted) = target;
            if !permitted {
                return Err(error(Kernel::NotPermitted));
            }
            crate::fs::trace::start(user, &target, a.flags, a.to)?;
            answer(label, 0, [0; 8])
        }
        process::LIMITS | process::FILE_MASK => limits::method(user, label, args),
        process::TIMES => {
            process::TimesArgs::decode(args)?;
            let (user_ns, system_ns) = me.cpu_times();
            let children = me.posix.lock().children_ns;
            let results = process::TimesResults {
                user_ns,
                system_ns,
                children_user_ns: children.0,
                children_system_ns: children.1.saturating_sub(children.0),
            };
            answer(label, process::TimesResults::WORDS, results.encode())
        }
        process::SET_TIMER => {
            let a = process::SetTimerArgs::decode(args)?;
            let (value_ns, interval_ns) = timer::set(me, a.timer, a.value_ns, a.interval_ns)?;
            let results = process::SetTimerResults { value_ns, interval_ns };
            answer(label, process::SetTimerResults::WORDS, results.encode())
        }
        process::PIPE => {
            let a = process::PipeArgs::decode(args)?;
            crate::fs::pipe::make(user, a.read, a.write, a.flags)?;
            answer(label, 0, [0; 8])
        }
        process::SOCKET_PAIR => {
            let a = process::SocketPairArgs::decode(args)?;
            crate::fs::socket::make_pair(user, a.first, a.second, a.flags)?;
            answer(label, 0, [0; 8])
        }
        process::SOCKET => {
            let a = process::SocketArgs::decode(args)?;
            crate::fs::socket::make(user, a.to, a.flags)?;
            answer(label, 0, [0; 8])
        }
        process::POLL => {
            let a = process::PollArgs::decode(args)?;
            let ready = poll::poll(user, a)?;
            answer(label, process::PollResults::WORDS, process::PollResults { ready }.encode())
        }
        process::CLOCK => {
            let a = process::ClockArgs::decode(args)?;
            let ns = clock(user, a.clock)?;
            answer(label, process::ClockResults::WORDS, process::ClockResults { ns }.encode())
        }
        process::SLOTS_USED => {
            let a = process::SlotsUsedArgs::decode(args)?;
            let cspace = me.cspace();
            let (used, slots) =
                caps::with(|store| -> core::result::Result<_, radix_cap::CapError> {
                    Ok((store.used_slots(cspace, a.first)?, store.slots(cspace)?))
                })
                .and_then(|found| found)
                .map_err(cap_error)?;
            let results = process::SlotsUsedResults { used, slots };
            answer(label, process::SlotsUsedResults::WORDS, results.encode())
        }
        process::GROW => {
            let a = process::GrowArgs::decode(args)?;
            let slots = me.grow(a.slots)?;
            answer(label, process::GrowResults::WORDS, process::GrowResults { slots }.encode())
        }
        process::EXIT_THREAD => {
            let a = process::ExitThreadArgs::decode(args)?;
            Ok(Answer::Exit(a.code))
        }
        process::SET_THREAD_POINTER => {
            let a = process::SetThreadPointerArgs::decode(args)?;
            if a.base >= radix_abi::address_space::USER_TOP {
                return Err(Kernel::InvalidArgument.status(0));
            }
            sched::current().ok_or(error(Kernel::Gone))?.set_thread_pointer(a.base);
            answer(label, 0, [0; 8])
        }
        process::SET_THREAD_NAME => {
            let a = process::SetThreadNameArgs::decode(args)?;
            let thread = own_thread(me, a.thread)?;
            if a.name.len > process::NAME_MAX {
                return Err(error(Kernel::OutOfRange));
            }
            let len = a.name.len as usize;
            let mut bytes = [0u8; process::NAME_MAX as usize];
            if !radix_ipc::range::user_bytes(a.name.address, a.name.len)
                || !Arch::copy_from_user(&mut bytes[..len], a.name.address)
            {
                return Err(error(Kernel::BadAddress));
            }
            let name = core::str::from_utf8(&bytes[..len])
                .map_err(|_| Kernel::InvalidArgument.status(0))?;
            thread.set_name(Name::new(name));
            answer(label, 0, [0; 8])
        }
        process::THREAD_NAME => {
            let a = process::ThreadNameArgs::decode(args)?;
            let name = own_thread(me, a.thread)?.name();
            let name = name.as_str().as_bytes();
            let length = name.len() as u64;
            if length > a.buffer.len {
                return Err(error(Kernel::OutOfRange));
            }
            if !radix_ipc::range::user_bytes(a.buffer.address, length)
                || !Arch::copy_to_user(a.buffer.address, name)
            {
                return Err(error(Kernel::BadAddress));
            }
            answer(
                label,
                process::ThreadNameResults::WORDS,
                process::ThreadNameResults { length }.encode(),
            )
        }
        process::THREAD_ID => {
            process::ThreadIdArgs::decode(args)?;
            let thread = sched::current().map_or(0, |me| me.id());
            answer(
                label,
                process::ThreadIdResults::WORDS,
                process::ThreadIdResults { thread }.encode(),
            )
        }
        _ => Err(error(Kernel::NoSuchMethod)),
    }
}

/// `wait`: waits for a child of `me` that `pid` names to change as
/// `flags` asks.
#[inline(never)]
fn wait_child(me: &ProcessRef, pid: i64, flags: ChildWaitFlags) -> Result<(u64, u32, u64, u64)> {
    let for_ = WaitFor {
        stopped: flags.contains(ChildWaitFlags::STOPPED),
        continued: flags.contains(ChildWaitFlags::CONTINUED),
        no_reap: flags.contains(ChildWaitFlags::NO_REAP),
        skip_ended: flags.contains(ChildWaitFlags::SKIP_ENDED),
    };
    if pid == i64::MIN {
        return Err(Kernel::InvalidArgument.status(0));
    }
    loop {
        let seen = me.child_events.load(Ordering::Acquire);
        let found = with_tree(|tree| -> Result<Found<ProcessRef>> {
            join(tree, me)?;
            let group = tree.get(me.number()).map_or(0, |m| m.group);
            Ok(tree.wait(me.number(), Which::from_wait(pid, group), for_))
        })?;
        match found {
            Found::Child { pid, status, times, reaped } => {
                if reaped.is_some() {
                    let mut posix = me.posix.lock();
                    posix.children_ns.0 += times.0;
                    posix.children_ns.1 += times.1;
                }
                drop(reaped);
                return Ok((pid, status, times.0, times.1.saturating_sub(times.0)));
            }
            Found::NoChild => return Err(error(Kernel::NoChild)),
            Found::NotYet if flags.contains(ChildWaitFlags::NO_HANG) => return Ok((0, 0, 0, 0)),
            Found::NotYet => {}
        }
        wait::wait_until(child_key(me), "wait", || {
            me.child_events.load(Ordering::Acquire) != seen || sched::interrupted()
        });
        if sched::interrupted() {
            return Err(signal::interrupted(signal::Restart::Always));
        }
    }
}

/// `kill`: sends `signal` to the processes `pid` names, as `me` may,
/// carrying `info`. A `sigqueue` signal (SI_QUEUE) for which there is no
/// room fails with would_block.
#[inline(never)]
fn kill(me: &ProcessRef, pid: i64, sig: u32, info: Info) -> Result<()> {
    if sig > SIGNALS || pid == i64::MIN {
        return Err(Kernel::InvalidArgument.status(1));
    }
    // The first target is kept here, the others in the heap: most kills
    // name one process.
    let mut first: Option<ProcessRef> = None;
    let mut others: Vec<ProcessRef, Kmalloc> = Vec::new_in(kmalloc());
    let (count, allowed) = with_tree(|tree| -> Result<(usize, usize)> {
        join(tree, me)?;
        let tree = &*tree;
        let sender = me.number();
        let group = tree.get(sender).map_or(0, |m| m.group);
        let mut allowed = 0;
        let mut failed = false;
        let count = tree.targets(sender, group, pid, &mut |member| {
            if !tree.may_signal(sender, member.pid) {
                return;
            }
            allowed += 1;
            // A zombie counts as signalled, and nothing happens to it.
            if matches!(member.life, radix_posix::tree::Life::Zombie { .. }) {
                return;
            }
            if first.is_none() {
                first = Some(member.handle.clone());
                return;
            }
            if others.try_reserve(1).is_err() {
                failed = true;
                return;
            }
            // Room was reserved above.
            let _ = others.push_within_capacity(member.handle.clone());
        });
        if failed {
            return Err(error(Kernel::NoMemory));
        }
        Ok((count, allowed))
    })?;
    if count == 0 {
        return Err(error(Kernel::NoSuchProcess));
    }
    if allowed == 0 {
        return Err(error(Kernel::NotPermitted));
    }
    if sig == 0 {
        return Ok(());
    }
    let mut full = false;
    for target in first.iter().chain(&others) {
        full |= signal::send_process(target, sig, info) == signal::Sent::Full;
    }
    if full && info.code == radix_posix::signal::SI_QUEUE {
        return Err(error(Kernel::WouldBlock));
    }
    Ok(())
}

/// `clock`: reads `clock`, for `user`'s thread.
fn clock(user: &UserRef, clock: ClockKind) -> Result<u64> {
    Ok(match clock {
        ClockKind::Realtime | ClockKind::RealtimeCoarse => {
            <Arch as radix_hal::Timer>::wall_clock_ns().unwrap_or(0)
        }
        ClockKind::Monotonic
        | ClockKind::MonotonicRaw
        | ClockKind::MonotonicCoarse
        | ClockKind::Boottime => sched::now_ns(),
        ClockKind::Process => cpu_ns(&user.process),
        ClockKind::Thread => sched::current().map_or(0, |me| me.runtime_ns()),
    })
}

/// The slots a program started by `exec` or forked finds itself in.
pub(super) const OWN_SLOTS: [u32; 3] =
    [startup::SLOT_THREAD, startup::SLOT_SPACE, startup::SLOT_PROCESS];
