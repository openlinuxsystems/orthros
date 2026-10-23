// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Signals: their numbers and sets, the action a process takes for each,
//! and the signals a thread or process has pending (radix-signal(2)).
//!
//! Signals are numbered as on Linux, from 1 to [`SIGNALS`]; a set of them
//! is a 64-bit word with bit `n - 1` for signal `n`. Signals from SIGRTMIN
//! on are real-time: each one sent is delivered, with what it carries (up
//! to [`QUEUE_MAX`] pending at once), where another signal sent while it
//! is pending is delivered once.

use alloc::vec::Vec;
use core::alloc::Allocator;

use radix_abi::signal::{
    ActionFlags, DEFAULT, IGNORE, SIGABRT, SIGBUS, SIGCHLD, SIGCONT, SIGFPE, SIGILL, SIGKILL,
    SIGRTMIN, SIGSEGV, SIGSTOP, SIGSYS, SIGTRAP, SIGTSTP, SIGTTIN, SIGTTOU, SIGURG, SIGWINCH,
};

pub use radix_abi::signal::SIGNALS;

/// The most real-time signals a thread or process may have pending at
/// once; sending another then fails, as `sigqueue` fails with EAGAIN.
pub const QUEUE_MAX: usize = 1024;

/// The bit of signal `signal` in a set, or 0 for a number that is not a
/// signal.
pub const fn bit(signal: u32) -> u64 {
    if signal == 0 || signal > SIGNALS { 0 } else { 1 << (signal - 1) }
}

/// Whether `signal` is a signal's number.
pub const fn valid(signal: u32) -> bool {
    signal >= 1 && signal <= SIGNALS
}

/// The signals that can be neither caught, blocked nor ignored.
pub const UNBLOCKABLE: u64 = bit(SIGKILL) | bit(SIGSTOP);

/// The signals that stop a process by default.
pub const STOPPING: u64 = bit(SIGSTOP) | bit(SIGTSTP) | bit(SIGTTIN) | bit(SIGTTOU);

/// The signals a fault raises, which are taken before any other pending
/// signal so that the handler sees the fault's own.
pub const SYNCHRONOUS: u64 =
    bit(SIGSEGV) | bit(SIGBUS) | bit(SIGILL) | bit(SIGTRAP) | bit(SIGFPE) | bit(SIGSYS);

/// The real-time signals.
pub const REAL_TIME: u64 = !(bit(SIGRTMIN) - 1);

/// What a signal does when its action is the default.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DefaultAction {
    /// Ends the process.
    Terminate,
    /// Nothing.
    Ignore,
    /// Stops the process until SIGCONT.
    Stop,
    /// Continues the process if it was stopped (which sending does).
    Continue,
}

/// The default action of `signal`, as POSIX gives it (with no core dumps,
/// the actions that dump one end the process).
pub const fn default_action(signal: u32) -> DefaultAction {
    match signal {
        SIGCHLD | SIGURG | SIGWINCH => DefaultAction::Ignore,
        SIGCONT => DefaultAction::Continue,
        SIGSTOP | SIGTSTP | SIGTTIN | SIGTTOU => DefaultAction::Stop,
        _ => DefaultAction::Terminate,
    }
}

/// What a process does when a signal comes: Linux's `struct sigaction`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Action {
    /// [`DEFAULT`], [`IGNORE`], or the address of the handler.
    pub handler: u64,
    /// ActionFlags bits.
    pub flags: u64,
    /// The signals blocked while the handler runs, besides the signal
    /// itself (unless NODEFER).
    pub mask: u64,
    /// Where the handler returns to: code that calls `signal_return`.
    pub restorer: u64,
}

impl Action {
    /// Whether it has a flag.
    pub const fn has(&self, flag: ActionFlags) -> bool {
        self.flags & flag.0 != 0
    }
}

/// What delivering a signal comes to, given its action.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Disposition {
    /// It is dropped.
    Ignore,
    /// The process ends, killed by it.
    Terminate,
    /// The process stops.
    Stop,
    /// The handler runs.
    Handle(Action),
}

/// The action of every signal of a process, kept in `S`: an array, or in
/// the kernel memory from its heap, so that a process's actions are never
/// built on a stack (they take 2 KiB).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Actions<S = [Action; SIGNALS as usize]>(S);

/// Why an action could not be set.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ActionError {
    /// The number is not a signal's.
    NoSuchSignal,
    /// SIGKILL and SIGSTOP keep their default action.
    Unchangeable,
}

impl Default for Actions {
    fn default() -> Actions {
        Actions::new()
    }
}

impl Actions {
    /// Every signal at its default action.
    pub const fn new() -> Actions {
        Actions([Action { handler: DEFAULT, flags: 0, mask: 0, restorer: 0 }; SIGNALS as usize])
    }
}

impl<S: AsRef<[Action]> + AsMut<[Action]>> Actions<S> {
    /// Every signal at its default action, kept in `storage`, which must
    /// hold [`SIGNALS`] actions (`None` otherwise).
    pub fn in_storage(mut storage: S) -> Option<Actions<S>> {
        if storage.as_ref().len() != SIGNALS as usize {
            return None;
        }
        storage.as_mut().fill(Action::default());
        Some(Actions(storage))
    }

    /// Makes every action `other`'s, as a fork's child gets them.
    pub fn copy_from<T: AsRef<[Action]>>(&mut self, other: &Actions<T>) {
        self.0.as_mut().copy_from_slice(other.0.as_ref());
    }

    /// The action of `signal`, a valid signal.
    pub fn get(&self, signal: u32) -> Action {
        self.0.as_ref()[(signal - 1) as usize]
    }

    /// Makes `action` the action of `signal`, and returns the one before.
    /// The handler's mask never holds SIGKILL or SIGSTOP.
    pub fn set(&mut self, signal: u32, mut action: Action) -> Result<Action, ActionError> {
        if !valid(signal) {
            return Err(ActionError::NoSuchSignal);
        }
        if bit(signal) & UNBLOCKABLE != 0 && action.handler != DEFAULT {
            return Err(ActionError::Unchangeable);
        }
        action.mask &= !UNBLOCKABLE;
        Ok(core::mem::replace(&mut self.0.as_mut()[(signal - 1) as usize], action))
    }

    /// What `exec` keeps: ignored signals stay ignored, handled ones go
    /// back to their default, since the handlers are gone with the
    /// program.
    pub fn reset_for_exec(&mut self) {
        for action in self.0.as_mut() {
            if action.handler != IGNORE {
                *action = Action::default();
            } else {
                *action = Action { handler: IGNORE, ..Action::default() };
            }
        }
    }

    /// Whether `signal` would be dropped if it came now: it is ignored, or
    /// its default action is to do nothing. Such a signal is not even made
    /// pending, unless the thread blocks it (POSIX leaves that open; Linux
    /// drops it too unless it is blocked).
    pub fn ignores(&self, signal: u32) -> bool {
        let action = self.get(signal);
        action.handler == IGNORE
            || (action.handler == DEFAULT && default_action(signal) == DefaultAction::Ignore)
    }

    /// Whether children that end are reaped at once instead of waiting
    /// as zombies for `wait`: SIGCHLD is set to SIG_IGN, or its action has
    /// SA_NOCLDWAIT. SIGCHLD's default action does nothing too, but its
    /// children stay to be waited for (POSIX).
    pub fn reaps_children(&self) -> bool {
        let action = self.get(SIGCHLD);
        action.handler == IGNORE || action.has(ActionFlags::NO_CHILD_WAIT)
    }

    /// What delivering `signal` comes to now.
    pub fn disposition(&self, signal: u32) -> Disposition {
        let action = self.get(signal);
        match action.handler {
            IGNORE => Disposition::Ignore,
            DEFAULT => match default_action(signal) {
                DefaultAction::Terminate => Disposition::Terminate,
                DefaultAction::Stop => Disposition::Stop,
                DefaultAction::Ignore | DefaultAction::Continue => Disposition::Ignore,
            },
            _ => Disposition::Handle(action),
        }
    }

    /// The handler of `signal` has run once: with RESETHAND, its action
    /// goes back to the default.
    pub fn handled(&mut self, signal: u32) {
        let action = &mut self.0.as_mut()[(signal - 1) as usize];
        if action.has(ActionFlags::RESET_HAND) {
            *action = Action::default();
        }
    }
}

/// What a signal's receiver learns about it: the parts of `siginfo_t` the
/// kernel fills (radix-signal(2)'s SignalInfo).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Info {
    /// Where it came from: `si_code`.
    pub code: i32,
    /// The sender's process number, or the address a fault was on; for a
    /// timer's signal, the timer's number, and in the high half how many
    /// times it expired again before the signal was taken (its overrun).
    pub pid: u64,
    /// For SIGCHLD the child's code or signal; for `sigqueue`'s signals
    /// and timers' the value they carry.
    pub status: u64,
}

/// `si_code` of a signal sent with `kill`.
pub const SI_USER: i32 = 0;
/// `si_code` of a signal sent with `sigqueue`.
pub const SI_QUEUE: i32 = -1;
/// `si_code` of a signal the kernel sent.
pub const SI_KERNEL: i32 = 0x80;
/// `si_code` of a POSIX timer's signal (`timer_create`).
pub const SI_TIMER: i32 = -2;
/// `si_code` of a signal sent to a thread.
pub const SI_TKILL: i32 = -6;
/// SIGCHLD's `si_code`: the child exited.
pub const CLD_EXITED: i32 = 1;
/// SIGCHLD's `si_code`: a signal ended the child.
pub const CLD_KILLED: i32 = 2;
/// SIGCHLD's `si_code`: the child stopped.
pub const CLD_STOPPED: i32 = 5;
/// SIGCHLD's `si_code`: the child continued.
pub const CLD_CONTINUED: i32 = 6;
/// SIGSEGV's `si_code`: nothing is mapped at the address.
pub const SEGV_MAPERR: i32 = 1;
/// SIGSEGV's `si_code`: the mapping does not allow the access.
pub const SEGV_ACCERR: i32 = 2;
/// SIGILL's `si_code`: an illegal opcode.
pub const ILL_ILLOPN: i32 = 2;
/// SIGFPE's `si_code`: an integer divided by zero.
pub const FPE_INTDIV: i32 = 1;
/// SIGBUS's `si_code`: a misaligned address.
pub const BUS_ADRALN: i32 = 1;
/// SIGBUS's `si_code`: an address with nothing behind it (a file mapping
/// past its file's end, or one that could not be read).
pub const BUS_ADRERR: i32 = 2;
/// SIGTRAP's `si_code`: a breakpoint.
pub const TRAP_BRKPT: i32 = 1;

/// The signals pending for a thread, or for a process (taken by whichever
/// of its threads does not block them first): what each standard signal
/// carries is kept in `S`, as [`Actions`] keeps its actions, and the
/// real-time signals in a queue in `A`'s memory, each with its own.
#[derive(Debug)]
pub struct Pending<S, A: Allocator> {
    /// The signals pending.
    set: u64,
    /// What each pending standard signal carries.
    info: S,
    /// The real-time signals pending, in the order they were sent, each
    /// with what it carries.
    queue: Vec<(u32, Info), A>,
}

impl<S: AsRef<[Info]> + AsMut<[Info]>, A: Allocator> Pending<S, A> {
    /// Nothing pending, with what standard signals carry kept in
    /// `storage`, which must hold [`SIGNALS`] entries (`None` otherwise),
    /// and real-time signals queued in `alloc`'s memory.
    pub fn in_storage(storage: S, alloc: A) -> Option<Pending<S, A>> {
        (storage.as_ref().len() == SIGNALS as usize).then_some(Pending {
            set: 0,
            info: storage,
            queue: Vec::new_in(alloc),
        })
    }

    /// The signals pending.
    pub const fn set(&self) -> u64 {
        self.set
    }

    /// Makes `signal` pending with `info`. Returns `false` if it was
    /// pending already and is not real-time, or [`QUEUE_MAX`] real-time
    /// signals are pending, or there is no memory to queue one: then
    /// nothing changes.
    pub fn add(&mut self, signal: u32, info: Info) -> bool {
        let bit = bit(signal);
        if bit == 0 {
            return false;
        }
        if bit & REAL_TIME != 0 {
            if self.queue.len() >= QUEUE_MAX || self.queue.try_reserve(1).is_err() {
                return false;
            }
            // Room was reserved above.
            let _ = self.queue.push_within_capacity((signal, info));
        } else if self.set & bit != 0 {
            return false;
        } else {
            self.info.as_mut()[(signal - 1) as usize] = info;
        }
        self.set |= bit;
        true
    }

    /// Takes the signal to deliver next among those in `allowed`: a fault's
    /// first, then the lowest-numbered, as Linux does; of a real-time
    /// signal pending more than once, the one sent first.
    pub fn take(&mut self, allowed: u64) -> Option<(u32, Info)> {
        let ready = self.set & allowed;
        if ready == 0 {
            return None;
        }
        let synchronous = ready & SYNCHRONOUS;
        let from = if synchronous != 0 { synchronous } else { ready };
        let signal = from.trailing_zeros() + 1;
        let bit = bit(signal);
        if bit & REAL_TIME == 0 {
            self.set &= !bit;
            return Some((signal, self.info.as_ref()[(signal - 1) as usize]));
        }
        let mut found = self.queue.iter().enumerate().filter(|(_, (s, _))| *s == signal);
        let first = found.next().map(|(at, _)| at);
        if found.next().is_none() {
            self.set &= !bit;
        }
        // The set says one is queued; if none were, it is gone now.
        first.map(|at| (signal, self.queue.remove(at).1))
    }

    /// Whether `signal` is pending with `info` (for a real-time signal,
    /// one of those sent).
    pub fn holds(&self, signal: u32, info: &Info) -> bool {
        let bit = bit(signal);
        if self.set & bit == 0 {
            false
        } else if bit & REAL_TIME == 0 {
            self.info.as_ref()[(signal - 1) as usize] == *info
        } else {
            self.queue.contains(&(signal, *info))
        }
    }

    /// Drops `signal` if it is pending with `info` (for a real-time
    /// signal, the first one sent with it). Returns whether it was.
    pub fn remove(&mut self, signal: u32, info: &Info) -> bool {
        let bit = bit(signal);
        if self.set & bit == 0 {
            return false;
        }
        if bit & REAL_TIME == 0 {
            if self.info.as_ref()[(signal - 1) as usize] != *info {
                return false;
            }
            self.set &= !bit;
            return true;
        }
        let Some(at) = self.queue.iter().position(|entry| *entry == (signal, *info)) else {
            return false;
        };
        self.queue.remove(at);
        if !self.queue.iter().any(|(s, _)| *s == signal) {
            self.set &= !bit;
        }
        true
    }

    /// Drops every signal of `set` that is pending.
    pub fn discard(&mut self, set: u64) {
        let gone = self.set & set;
        self.set &= !gone;
        if gone & REAL_TIME != 0 {
            self.queue.retain(|(signal, _)| gone & bit(*signal) == 0);
        }
    }

    /// Drops everything.
    pub fn clear(&mut self) {
        self.discard(u64::MAX);
    }
}

/// What sending `signal` does to a process before it is delivered: SIGCONT
/// continues a stopped process and drops its pending stop signals; a stop
/// signal drops a pending SIGCONT; SIGKILL continues it so that it can end.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct OnSend {
    /// The pending signals to drop.
    pub discard: u64,
    /// Whether the process continues, if it was stopped.
    pub resume: bool,
}

/// What sending `signal` does at once ([`OnSend`]).
pub const fn on_send(signal: u32) -> OnSend {
    match signal {
        SIGCONT | SIGKILL => OnSend { discard: STOPPING, resume: true },
        _ if bit(signal) & STOPPING != 0 => OnSend { discard: bit(SIGCONT), resume: false },
        _ => OnSend { discard: 0, resume: false },
    }
}

/// Whether a signal ends a process by default the way a fault does, so
/// that the kernel logs it: SIGSEGV and friends, and SIGABRT.
pub const fn is_crash(signal: u32) -> bool {
    bit(signal) & (SYNCHRONOUS | bit(SIGABRT)) != 0
}
