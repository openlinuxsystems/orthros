// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Signals (Phase 8, radix-signal(7)): sending them, and delivering them
//! on a thread's way back to user mode.
//!
//! Sending ([`send_process`], [`send_thread`]) does what the signal does at
//! once (SIGCONT continues a stopped process, SIGKILL and a signal whose
//! default ends the process end it), then makes it pending, for the
//! process or for one thread, and picks a thread that does not block it:
//! that thread is marked `signalled` and woken, so that a wait it is in
//! ends as interrupted. On its way back to user mode ([`deliver`], from
//! `on_return`) the thread takes its signals one at a time: a stop stops
//! the whole process (each of its threads waits in [`deliver`] until
//! SIGCONT), a handler gets a frame on the thread's stack
//! (`UserMode::push_signal_frame`), and a system call the signal
//! interrupted runs again if it may ([`Restart`]).
//!
//! A thread's `signalled` flag is set and cleared with its process's POSIX
//! state and the thread's signals both locked, so a signal sent while the
//! thread decides it has nothing to do is never missed.

use core::sync::atomic::Ordering;
use radix_abi::Status;
use radix_abi::errors::File as FileError;
use radix_abi::errors::Kernel;
use radix_abi::process::{self, ProcessState};
use radix_abi::signal::{
    ActionFlags, MaskHow, SIGBUS, SIGCHLD, SIGCONT, SIGFPE, SIGILL, SIGKILL, SIGNALS, SIGSEGV,
    SIGSTOP, SIGTRAP, SIGTTIN, SignalInfo, StackFlags,
};
use radix_hal::{SignalFrame, SyscallRegs, UserFault, UserFaultKind, UserMode};
use radix_klog::warn;
use radix_posix::signal::{
    self as rules, Action, ActionError, Disposition, Info, UNBLOCKABLE, bit,
};
use radix_posix::status;

use super::{
    PendingSignals, PosixState, children_changed, new_pending, signal_key, stop_key, timer,
    with_tree,
};
use crate::Arch;
use crate::sched::{self, ThreadRef, wait};
use crate::user::methods::{Answer, answer};
use crate::user::process::{Ending, ProcessObject, ProcessRef};
use crate::user::{EXIT_FAULT, EXIT_STOPPED, Result, UserRef, UserThread, error, exit_current};

/// A thread's own signal state.
pub struct ThreadSignals {
    /// The signals it blocks.
    pub mask: u64,
    /// The signals sent to it alone.
    pub pending: PendingSignals,
    /// Its alternate signal stack: lowest address, size and StackFlags.
    pub stack: (u64, u64, u32),
    /// The mask to put back once the handler of the signal that ends a
    /// `signal_suspend` or masked `poll` has run.
    pub saved_mask: Option<u64>,
    /// The system call a signal interrupted, as it was made, and when it
    /// may run again.
    pub restart: Option<(SyscallRegs, Restart)>,
    /// What a fault that raised a signal for it left: the signal, the
    /// vector, the error code and the address.
    pub trap: Option<(u32, u64, u64, u64)>,
}

impl ThreadSignals {
    /// A thread's signal state, with `mask` blocked: the mask of the
    /// thread that made it. `None` if there is no memory for it.
    pub fn new(mask: u64) -> Option<ThreadSignals> {
        Some(ThreadSignals {
            mask: mask & !UNBLOCKABLE,
            pending: new_pending()?,
            stack: (0, 0, StackFlags::DISABLED.0),
            saved_mask: None,
            restart: None,
            trap: None,
        })
    }
}

/// When a system call a signal interrupted runs again, as Linux's
/// ERESTARTSYS, ERESTARTNOHAND and EINTR: the detail of its `interrupted`
/// status.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Restart {
    /// Unless a handler without RESTART runs.
    Always = 0,
    /// Only if no handler runs (a stop and a continue, an ignored signal).
    NoHandler = 1,
    /// Never: the call fails with interrupted.
    Never = 2,
}

/// The status of a call a signal interrupted, which runs again as
/// `restart` says.
pub fn interrupted(restart: Restart) -> Status {
    Kernel::Interrupted.status(restart as u32)
}

/// The system call `number` with `args`, made by `user`'s thread, failed
/// with `status`: if a signal interrupted it, it is kept to run again
/// once the signal has been dealt with on the way back.
pub fn call_failed(
    me: &ThreadRef,
    user: &UserThread,
    number: u64,
    args: &[u64; 10],
    status: Status,
) {
    let interrupted = Kernel::Interrupted.status(0);
    if status.domain() != interrupted.domain() || status.code() != interrupted.code() {
        return;
    }
    let restart = match status.detail() {
        0 => Restart::Always,
        1 => Restart::NoHandler,
        _ => return,
    };
    user.signals.lock().restart = Some((SyscallRegs { number, args: *args }, restart));
    // So that the way back looks, even if another thread took the signal.
    me.set_signalled(true);
}

/// Whether `user`'s thread has something to do on its way back: with its
/// process's POSIX state and its own signals locked.
fn has_work(posix: &PosixState, signals: &ThreadSignals) -> bool {
    posix.stopped.is_some()
        || (signals.pending.set() | posix.pending.set()) & !signals.mask != 0
        || signals.restart.is_some()
}

/// Marks `thread` as having something to do on its way back, and wakes
/// it.
fn kick(thread: &ThreadRef) {
    thread.set_signalled(true);
    sched::wake(thread);
}

/// Whether `process` is past taking signals.
fn ending(process: &ProcessObject) -> bool {
    !matches!(process.state().0, ProcessState::Created | ProcessState::Running)
}

/// Ends `process`, killed by `signal`: its threads stop, as for `exit`.
fn kill_process(process: &ProcessObject, signal: u32) {
    process.posix.lock().ended_by.get_or_insert(signal);
    let code = if rules::is_crash(signal) { EXIT_FAULT } else { EXIT_STOPPED };
    process.exit(code);
}

/// What became of a signal sent.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Sent {
    /// It is pending with what it carries, or it ended the process.
    Queued,
    /// Nothing of it is pending: it is ignored, or the same standard
    /// signal was pending already, or the process is ending.
    Dropped,
    /// A real-time signal for which there was no room (QUEUE_MAX).
    Full,
}

/// What [`Pending::add`] answering `added` for `signal` means.
fn sent(signal: u32, added: bool) -> Sent {
    match (added, bit(signal) & rules::REAL_TIME != 0) {
        (true, _) => Sent::Queued,
        (false, true) => Sent::Full,
        (false, false) => Sent::Dropped,
    }
}

/// Sends `signal` (1 to SIGNALS) to `process`, from `info`.
pub fn send_process(process: &ProcessRef, signal: u32, info: Info) -> Sent {
    if signal == 0 || signal > SIGNALS || ending(process) {
        return Sent::Dropped;
    }
    if signal == SIGKILL {
        continue_process(process);
        kill_process(process, signal);
        return Sent::Queued;
    }
    let on = rules::on_send(signal);
    // A stop signal to a process group no shell can continue is dropped.
    if bit(signal) & rules::STOPPING != 0 && signal != SIGSTOP && orphaned(process) {
        return Sent::Dropped;
    }
    if on.resume {
        continue_process(process);
    }
    let mut fatal = false;
    let result;
    {
        let mut posix = process.posix.lock();
        posix.pending.discard(on.discard);
        process.for_each_thread(&mut |_, user| user.signals.lock().pending.discard(on.discard));
        timer::discarded(process, &mut posix, on.discard);
        let disposition = posix.actions.disposition(signal);
        let ignored = posix.actions.ignores(signal);
        // An ignored signal is dropped, unless every thread blocks it (its
        // action may change before it is taken).
        let mut target = None;
        process.for_each_thread(&mut |thread, user| {
            if target.is_none() && user.signals.lock().mask & bit(signal) == 0 {
                target = Some(thread.clone());
            }
        });
        if ignored && target.is_some() {
            return Sent::Dropped;
        }
        if disposition == Disposition::Terminate && target.is_some() {
            // Nothing would run before it ends the process: end it now,
            // even if its threads are in waits a signal cannot cut short.
            fatal = true;
            result = Sent::Queued;
        } else {
            result = sent(signal, posix.pending.add(signal, info));
            if result == Sent::Queued
                && let Some(thread) = target
            {
                kick(&thread);
            }
        }
    }
    if fatal {
        kill_process(process, signal);
        return result;
    }
    if result == Sent::Queued {
        process.signal_events.fetch_add(1, Ordering::AcqRel);
        wait::wake_all(signal_key(process));
    }
    result
}

/// Sends `signal` to `thread`, `user`'s, alone: a fault's, `signal_thread`'s
/// or a timer's.
pub fn send_thread(thread: &ThreadRef, user: &UserThread, signal: u32, info: Info) -> Sent {
    let process = &user.process;
    if signal == 0 || signal > SIGNALS || ending(process) {
        return Sent::Dropped;
    }
    if signal == SIGKILL {
        continue_process(process);
        kill_process(process, signal);
        return Sent::Queued;
    }
    let on = rules::on_send(signal);
    if on.resume {
        continue_process(process);
    }
    let result = {
        let mut posix = process.posix.lock();
        let mut signals = user.signals.lock();
        signals.pending.discard(on.discard);
        let blocked = signals.mask & bit(signal) != 0;
        if posix.actions.ignores(signal) && !blocked {
            drop(signals);
            timer::discarded(process, &mut posix, on.discard);
            return Sent::Dropped;
        }
        if posix.actions.disposition(signal) == Disposition::Terminate && !blocked {
            None
        } else {
            let result = sent(signal, signals.pending.add(signal, info));
            if result == Sent::Queued && !blocked {
                kick(thread);
            }
            drop(signals);
            timer::discarded(process, &mut posix, on.discard);
            Some(result)
        }
    };
    let Some(result) = result else {
        kill_process(process, signal);
        return Sent::Queued;
    };
    if result == Sent::Queued {
        process.signal_events.fetch_add(1, Ordering::AcqRel);
        wait::wake_all(signal_key(process));
    }
    result
}

/// Raises `signal` for the running thread, as the kernel does for a
/// write to a pipe no end reads (SIGPIPE).
pub fn raise_current(signal: u32) {
    let Some(me) = sched::current() else { return };
    let Some(user) = me.user() else { return };
    let info = Info { code: rules::SI_USER, pid: user.process.number(), status: 0 };
    send_thread(&me, user, signal, info);
}

/// Job control on a terminal (radix-terminal(4)): `user`'s thread, of
/// background process group `group`, reads its controlling terminal
/// (`signal` SIGTTIN) or writes to it or changes it (SIGTTOU). If the
/// thread ignores or blocks the signal, a read fails with io and a write
/// or change goes ahead; if the process group is orphaned, both fail with
/// io; otherwise the process group gets the signal, and the call runs
/// again once it continues.
pub fn job_control(user: &UserThread, group: u64, signal: u32) -> core::result::Result<(), Status> {
    let (ignored, blocked) = {
        let posix = user.process.posix.lock();
        let blocked = user.signals.lock().mask & bit(signal) != 0;
        (posix.actions.ignores(signal), blocked)
    };
    if ignored || blocked {
        return if signal == SIGTTIN { Err(FileError::Io.status(0)) } else { Ok(()) };
    }
    if with_tree(|tree| tree.orphaned(group)) {
        return Err(FileError::Io.status(0));
    }
    send_group(group, signal, Info { code: rules::SI_KERNEL, pid: 0, status: 0 });
    Err(interrupted(Restart::Always))
}

/// Puts `mask` in place of the calling thread's signal mask for a wait
/// (`signal_suspend`, a masked `poll`): the old mask comes back once the
/// handler of the signal that ends the wait has run, or at
/// [`end_masked_wait`] if the wait ends otherwise.
pub fn begin_masked_wait(user: &UserThread, mask: u64) {
    let posix = user.process.posix.lock();
    let mut signals = user.signals.lock();
    if signals.saved_mask.is_none() {
        signals.saved_mask = Some(signals.mask);
    }
    signals.mask = mask & !UNBLOCKABLE;
    if let Some(me) = sched::running() {
        me.set_signalled(has_work(&posix, &signals));
    }
}

/// A wait [`begin_masked_wait`] began ended without a signal: the old
/// mask is back.
pub fn end_masked_wait(user: &UserThread) {
    let posix = user.process.posix.lock();
    let mut signals = user.signals.lock();
    if let Some(mask) = signals.saved_mask.take() {
        signals.mask = mask;
    }
    if let Some(me) = sched::running() {
        me.set_signalled(has_work(&posix, &signals));
    }
}

/// Sends `signal` to every process of process group `group`.
pub fn send_group(group: u64, signal: u32, info: Info) {
    let mut targets = alloc::vec::Vec::new_in(super::kmalloc());
    with_tree(|tree| {
        tree.targets(0, group, -(group as i64), &mut |member| {
            if !matches!(member.life, radix_posix::tree::Life::Zombie { .. })
                && targets.try_reserve(1).is_ok()
            {
                let _ = targets.push_within_capacity(member.handle.clone());
            }
        });
    });
    for target in &targets {
        send_process(target, signal, info);
    }
}

/// Whether `process`'s process group is orphaned.
fn orphaned(process: &ProcessObject) -> bool {
    with_tree(|tree| tree.get(process.number()).is_some_and(|me| tree.orphaned(me.group)))
}

/// Continues `process` if a signal stopped it: its threads go on, and its
/// parent hears of it.
fn continue_process(process: &ProcessObject) {
    let was = process.posix.lock().stopped.take();
    if was.is_none() {
        return;
    }
    process.stopped.store(false, Ordering::Release);
    wait::wake_all(stop_key(process));
    let parent = with_tree(|tree| {
        let parent = tree.continued(process.number())?;
        super::find(tree, parent)
    });
    if let Some(parent) = parent {
        let wants = !parent.posix.lock().actions.get(SIGCHLD).has(ActionFlags::NO_CHILD_STOP);
        if wants {
            let info = Info {
                code: rules::CLD_CONTINUED,
                pid: process.number(),
                status: u64::from(SIGCONT),
            };
            send_process(&parent, SIGCHLD, info);
        }
        children_changed(&parent);
    }
}

/// Stops `process` with `signal`, unless it is stopped already: each of
/// its threads stops on its way back, and its parent hears of it.
fn stop_process(process: &ProcessObject, signal: u32) {
    {
        let mut posix = process.posix.lock();
        if posix.stopped.is_some() {
            return;
        }
        posix.stopped = Some(signal);
        process.stopped.store(true, Ordering::Release);
        process.for_each_thread(&mut |thread, _| kick(thread));
    }
    let parent = with_tree(|tree| {
        let parent = tree.stopped(process.number(), signal)?;
        super::find(tree, parent)
    });
    if let Some(parent) = parent {
        let wants = !parent.posix.lock().actions.get(SIGCHLD).has(ActionFlags::NO_CHILD_STOP);
        if wants {
            let info =
                Info { code: rules::CLD_STOPPED, pid: process.number(), status: u64::from(signal) };
            send_process(&parent, SIGCHLD, info);
        }
        children_changed(&parent);
    }
}

/// The signal a fault in user mode raises, and its `si_code`.
fn fault_signal(fault: &UserFault) -> (u32, i32, u64) {
    match fault.kind {
        UserFaultKind::Page { address, present, .. } => {
            (SIGSEGV, if present { rules::SEGV_ACCERR } else { rules::SEGV_MAPERR }, address)
        }
        UserFaultKind::Protection => (SIGSEGV, rules::SI_KERNEL, 0),
        UserFaultKind::InvalidOpcode => (SIGILL, rules::ILL_ILLOPN, fault.pc),
        UserFaultKind::Breakpoint => (SIGTRAP, rules::TRAP_BRKPT, fault.pc),
        UserFaultKind::Divide => (SIGFPE, rules::FPE_INTDIV, fault.pc),
        UserFaultKind::FloatingPoint => (SIGFPE, rules::SI_KERNEL, fault.pc),
        UserFaultKind::Alignment => (SIGBUS, rules::BUS_ADRALN, fault.pc),
        UserFaultKind::Other(_) => (SIGSEGV, rules::SI_KERNEL, 0),
    }
}

/// The x86-64 vector a fault came with, for the signal frame.
fn vector_of(fault: &UserFault) -> u64 {
    match fault.kind {
        UserFaultKind::Divide => 0,
        UserFaultKind::Breakpoint => 3,
        UserFaultKind::InvalidOpcode => 6,
        UserFaultKind::Protection => 13,
        UserFaultKind::Page { .. } => 14,
        UserFaultKind::FloatingPoint => 16,
        UserFaultKind::Alignment => 17,
        UserFaultKind::Other(vector) => u64::from(vector),
    }
}

/// A fault in user mode that no page resolves, by `user`'s thread:
/// raises its signal for the thread, if a handler of its will run;
/// otherwise the process ends, killed by it. Returns whether the thread
/// goes on (to its handler).
pub fn fault(me: &ThreadRef, user: &UserThread, fault: &UserFault, bus: bool) -> bool {
    let (signal, code, address) = if bus {
        let address = match fault.kind {
            UserFaultKind::Page { address, .. } => address,
            _ => 0,
        };
        (SIGBUS, rules::BUS_ADRERR, address)
    } else {
        fault_signal(fault)
    };
    let process = &user.process;
    let handled = {
        let posix = process.posix.lock();
        let mut signals = user.signals.lock();
        let blocked = signals.mask & bit(signal) != 0;
        let handler = matches!(posix.actions.disposition(signal), Disposition::Handle(_));
        if handler && !blocked {
            signals.pending.add(signal, Info { code, pid: address, status: 0 });
            signals.trap = Some((signal, vector_of(fault), fault.code, address));
            me.set_signalled(true);
            true
        } else {
            false
        }
    };
    if !handled {
        warn!("User: thread {} ({}) ended: {fault}", me.id(), me.name());
        process.posix.lock().ended_by.get_or_insert(signal);
    }
    handled
}

/// The bytes of a SignalInfo for `signal` from `info`.
fn info_bytes(signal: u32, info: &Info) -> [u8; SignalInfo::SIZE] {
    let mut bytes = [0u8; SignalInfo::SIZE];
    bytes[SignalInfo::OFFSET_SIGNAL..][..4].copy_from_slice(&(signal as i32).to_le_bytes());
    bytes[SignalInfo::OFFSET_CODE..][..4].copy_from_slice(&info.code.to_le_bytes());
    bytes[SignalInfo::OFFSET_PID..][..8].copy_from_slice(&info.pid.to_le_bytes());
    bytes[SignalInfo::OFFSET_STATUS..][..8].copy_from_slice(&info.status.to_le_bytes());
    bytes
}

/// What the way back does for one signal, decided with the locks held.
enum Step {
    /// The process is stopped: wait until it continues.
    Park,
    /// Stop the process with this signal.
    Stop(u32),
    /// End the process, killed by this signal.
    Kill(u32),
    /// Run a handler.
    Handle { signal: u32, info: Info, action: Action },
}

/// Delivers what `me`, `user`'s thread, has pending on its way back to
/// user mode: stops for job control, ends the process for a signal that
/// ends it, or makes the thread run a handler; and makes an interrupted
/// system call run again when it may. Called with interrupts on; returns
/// with the thread ready for user mode, or never.
pub fn deliver(me: &ThreadRef, user: &UserRef) {
    let process = &user.process;
    loop {
        if me.should_stop() {
            return;
        }
        let step = {
            let mut posix = process.posix.lock();
            let mut signals = user.signals.lock();
            if posix.stopped.is_some() {
                Step::Park
            } else {
                let allowed = !signals.mask;
                let taken = signals.pending.take(allowed).or_else(|| posix.pending.take(allowed));
                match taken {
                    None => {
                        let restart = signals.restart.take();
                        if let Some(mask) = signals.saved_mask.take() {
                            signals.mask = mask;
                        }
                        me.set_signalled(has_work(&posix, &signals));
                        drop(signals);
                        drop(posix);
                        if let Some((regs, _)) = restart {
                            // Nothing handled the interruption: the call
                            // runs again.
                            Arch::restart_syscall(&regs);
                        }
                        return;
                    }
                    Some((signal, info)) => {
                        // A timer's signal: the timer is set again, with
                        // its process's lock (and not the thread's) held.
                        drop(signals);
                        let info = timer::taken(process, &mut posix, signal, info);
                        match posix.actions.disposition(signal) {
                            Disposition::Ignore => continue,
                            Disposition::Stop => Step::Stop(signal),
                            Disposition::Terminate => Step::Kill(signal),
                            Disposition::Handle(action) => {
                                posix.actions.handled(signal);
                                Step::Handle { signal, info, action }
                            }
                        }
                    }
                }
            }
        };
        match step {
            Step::Park => {
                wait::wait_until(stop_key(process), "stopped", || {
                    !process.stopped.load(Ordering::Acquire) || me.should_stop()
                });
            }
            Step::Stop(signal) => stop_process(process, signal),
            Step::Kill(signal) => {
                if rules::is_crash(signal) {
                    warn!("User: process {} ended by signal {signal}", process.number());
                }
                kill_process(process, signal);
                exit_current(user.clone(), EXIT_STOPPED, Ending::Stop);
            }
            Step::Handle { signal, info, action } => {
                if !run_handler(user, signal, &info, &action) {
                    // No room for the frame: the process cannot go on.
                    warn!(
                        "User: thread {} ({}) has no stack for signal {signal}'s handler",
                        me.id(),
                        me.name()
                    );
                    kill_process(process, SIGSEGV);
                    exit_current(user.clone(), EXIT_FAULT, Ending::Stop);
                }
                // One handler at a time: the next signal comes on the next
                // way back, inside this one's handler.
                let posix = process.posix.lock();
                let signals = user.signals.lock();
                me.set_signalled(has_work(&posix, &signals));
                return;
            }
        }
    }
}

/// Makes the running thread run `action`'s handler for `signal`: restarts
/// the system call it interrupted if the handler allows, writes the frame,
/// and blocks the handler's mask. Returns `false` if the frame could not
/// be written.
fn run_handler(user: &UserThread, signal: u32, info: &Info, action: &Action) -> bool {
    let (mask, restart, stack, trap) = {
        let mut signals = user.signals.lock();
        let mask = signals.saved_mask.take().unwrap_or(signals.mask);
        let restart = signals.restart.take();
        let trap = signals.trap.take().filter(|trap| trap.0 == signal);
        (mask, restart, signals.stack, trap)
    };
    if let Some((regs, Restart::Always)) = restart
        && action.has(ActionFlags::RESTART)
    {
        Arch::restart_syscall(&regs);
    }
    let (base, size, flags) = stack;
    let sp = Arch::user_stack_pointer();
    let enabled = flags & StackFlags::DISABLED.0 == 0 && size > 0;
    let on_it = enabled && sp > base && sp <= base + size;
    let alternate =
        (action.has(ActionFlags::ON_STACK) && enabled && !on_it).then_some((base, size));
    let state = if on_it { StackFlags::ON_STACK.0 } else { flags };
    let bytes = info_bytes(signal, info);
    let frame = SignalFrame {
        signal,
        info: &bytes,
        handler: action.handler,
        restorer: action.restorer,
        mask,
        stack: alternate,
        altstack: (base, state, size),
        trap: trap.map(|(_, vector, code, address)| (vector, code, address)),
    };
    if !Arch::push_signal_frame(&frame) {
        return false;
    }
    let mut signals = user.signals.lock();
    let mut blocked = mask | action.mask;
    if !action.has(ActionFlags::NO_DEFER) {
        blocked |= bit(signal);
    }
    signals.mask = blocked & !UNBLOCKABLE;
    true
}

/// The signal methods of radix-process(2), on the calling thread.
#[inline(never)]
pub fn method(user: &UserRef, label: u32, args: &[u64; 8]) -> Result<Answer> {
    let process = &user.process;
    match label {
        process::SIGNAL_ACTION => {
            let a = process::SignalActionArgs::decode(args)?;
            if !rules::valid(a.signal) {
                return Err(Kernel::InvalidArgument.status(0));
            }
            let old = {
                let mut posix = process.posix.lock();
                if a.set {
                    let action = Action {
                        handler: a.handler,
                        flags: a.flags,
                        mask: a.mask,
                        restorer: a.restorer,
                    };
                    // DEFAULT and IGNORE are below every address.
                    if action.handler >= radix_abi::address_space::USER_TOP {
                        return Err(Kernel::InvalidArgument.status(2));
                    }
                    let old = posix.actions.set(a.signal, action).map_err(|err| match err {
                        ActionError::NoSuchSignal | ActionError::Unchangeable => {
                            Kernel::InvalidArgument.status(0)
                        }
                    })?;
                    // A signal now ignored is dropped where it is pending.
                    if posix.actions.ignores(a.signal) {
                        posix.pending.discard(bit(a.signal));
                        process.for_each_thread(&mut |_, user| {
                            user.signals.lock().pending.discard(bit(a.signal));
                        });
                        timer::discarded(process, &mut posix, bit(a.signal));
                    }
                    old
                } else {
                    posix.actions.get(a.signal)
                }
            };
            let results = process::SignalActionResults {
                handler: old.handler,
                flags: old.flags,
                mask: old.mask,
                restorer: old.restorer,
            };
            answer(label, process::SignalActionResults::WORDS, results.encode())
        }
        process::SIGNAL_RETURN => {
            let a = process::SignalReturnArgs::decode(args)?;
            let mut regs = SyscallRegs::default();
            let mask =
                Arch::pop_signal_frame(a.context, &mut regs).ok_or(error(Kernel::BadAddress))?;
            let posix = process.posix.lock();
            let mut signals = user.signals.lock();
            signals.mask = mask & !UNBLOCKABLE;
            signals.restart = None;
            if let Some(me) = sched::running() {
                me.set_signalled(has_work(&posix, &signals));
            }
            Ok(Answer::Restore(regs))
        }
        process::SIGNAL_MASK => {
            let a = process::SignalMaskArgs::decode(args)?;
            let posix = process.posix.lock();
            let mut signals = user.signals.lock();
            let old = signals.mask;
            signals.mask = match a.how {
                MaskHow::Block => old | a.set,
                MaskHow::Unblock => old & !a.set,
                MaskHow::Set => a.set,
                MaskHow::Query => old,
            } & !UNBLOCKABLE;
            if let Some(me) = sched::running() {
                me.set_signalled(has_work(&posix, &signals));
            }
            answer(
                label,
                process::SignalMaskResults::WORDS,
                process::SignalMaskResults { old }.encode(),
            )
        }
        process::SIGNAL_PENDING => {
            process::SignalPendingArgs::decode(args)?;
            let posix = process.posix.lock();
            let signals = user.signals.lock();
            let set = (signals.pending.set() | posix.pending.set()) & signals.mask;
            answer(
                label,
                process::SignalPendingResults::WORDS,
                process::SignalPendingResults { set }.encode(),
            )
        }
        process::SIGNAL_STACK => {
            let a = process::SignalStackArgs::decode(args)?;
            let mut signals = user.signals.lock();
            let (base, size, flags) = signals.stack;
            let sp = Arch::user_stack_pointer();
            let enabled = flags & StackFlags::DISABLED.0 == 0 && size > 0;
            let on_it = enabled && sp > base && sp <= base + size;
            if a.set {
                if on_it {
                    return Err(error(Kernel::NotPermitted));
                }
                if a.flags.contains(StackFlags::DISABLED) {
                    signals.stack = (0, 0, StackFlags::DISABLED.0);
                } else {
                    // MINSIGSTKSZ, as Linux's.
                    if a.size < 2048
                        || a.address
                            .checked_add(a.size)
                            .is_none_or(|end| end > radix_abi::address_space::USER_TOP)
                    {
                        return Err(Kernel::InvalidArgument.status(3));
                    }
                    if a.flags.0 & !StackFlags::DISABLED.0 != 0 {
                        return Err(Kernel::InvalidArgument.status(2));
                    }
                    signals.stack = (a.address, a.size, 0);
                }
            }
            let state = if on_it { StackFlags::ON_STACK } else { StackFlags(flags) };
            let results = process::SignalStackResults { address: base, flags: state, size };
            answer(label, process::SignalStackResults::WORDS, results.encode())
        }
        process::SIGNAL_SUSPEND => {
            let a = process::SignalSuspendArgs::decode(args)?;
            begin_masked_wait(user, a.mask);
            wait::wait_until(signal_key(process), "signal", sched::interrupted);
            // A handler runs, then the old mask is back; if none runs (a
            // stop and a continue), the call waits again.
            Err(interrupted(Restart::NoHandler))
        }
        process::SIGNAL_WAIT => {
            let a = process::SignalWaitArgs::decode(args)?;
            let set = a.set & !UNBLOCKABLE;
            let deadline = (!a.forever).then(|| sched::now_ns().saturating_add(a.timeout_ns));
            loop {
                let seen = process.signal_events.load(Ordering::Acquire);
                let taken = {
                    let mut posix = process.posix.lock();
                    let mut signals = user.signals.lock();
                    let taken = signals.pending.take(set).or_else(|| posix.pending.take(set));
                    if let Some(me) = sched::running() {
                        me.set_signalled(has_work(&posix, &signals));
                    }
                    drop(signals);
                    taken.map(|(signal, info)| {
                        (signal, timer::taken(process, &mut posix, signal, info))
                    })
                };
                if let Some((signal, info)) = taken {
                    let results = process::SignalWaitResults {
                        signal,
                        code: info.code,
                        pid: info.pid,
                        status: info.status,
                    };
                    return answer(label, process::SignalWaitResults::WORDS, results.encode());
                }
                if sched::interrupted() {
                    return Err(interrupted(Restart::NoHandler));
                }
                let changed = || {
                    process.signal_events.load(Ordering::Acquire) != seen || sched::interrupted()
                };
                match deadline {
                    None => wait::wait_until(signal_key(process), "signal", changed),
                    Some(deadline) => {
                        if !wait::wait_until_or_timeout(
                            signal_key(process),
                            "signal",
                            deadline,
                            changed,
                        ) {
                            return Err(error(Kernel::TimedOut));
                        }
                    }
                }
            }
        }
        process::SIGNAL_THREAD => {
            let a = process::SignalThreadArgs::decode(args)?;
            if a.signal > SIGNALS {
                return Err(Kernel::InvalidArgument.status(1));
            }
            // The last match: a thread that ran exec, ending, has the
            // number of the thread that runs the new program.
            let mut found = None;
            process.for_each_thread(&mut |thread, user| {
                if thread.id() == a.thread {
                    found = Some((thread.clone(), user.clone()));
                }
            });
            let (thread, target) = found.ok_or(error(Kernel::NoSuchProcess))?;
            if a.signal != 0 {
                let info = Info { code: rules::SI_TKILL, pid: process.number(), status: 0 };
                send_thread(&thread, &target, a.signal, info);
            }
            answer(label, 0, [0; 8])
        }
        _ => Err(error(Kernel::NoSuchMethod)),
    }
}

/// Whether a wait status says the process stopped (for the tests).
#[allow(dead_code)]
fn is_stop(wait_status: u32) -> bool {
    wait_status & 0xff == 0x7f && wait_status != status::CONTINUED
}
