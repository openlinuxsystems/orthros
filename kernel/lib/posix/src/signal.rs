// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Signals: their numbers and sets, the action a process takes for each,
//! and the signals a thread or process has pending (radix-signal(2)).
//!
//! Signals are numbered as on Linux, from 1 to [`SIGNALS`]; a set of them
//! is a 64-bit word with bit `n - 1` for signal `n`. Signals from SIGRTMIN
//! on are real-time: each one sent is delivered (up to [`QUEUE_MAX`]
//! pending at once), where another signal sent while it is pending is
//! delivered once.

use radix_abi::signal::{
    ActionFlags, DEFAULT, IGNORE, SIGABRT, SIGBUS, SIGCHLD, SIGCONT, SIGFPE, SIGILL, SIGKILL,
    SIGRTMIN, SIGSEGV, SIGSTOP, SIGSYS, SIGTRAP, SIGTSTP, SIGTTIN, SIGTTOU, SIGURG, SIGWINCH,
};

pub use radix_abi::signal::SIGNALS;

/// The most times one real-time signal may be pending at once; sending it
/// again then fails, as `sigqueue` fails with EAGAIN.
pub const QUEUE_MAX: u16 = 1024;

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

/// The action of every signal of a process.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Actions([Action; SIGNALS as usize]);

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

    /// The action of `signal`, a valid signal.
    pub fn get(&self, signal: u32) -> Action {
        self.0[(signal - 1) as usize]
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
        Ok(core::mem::replace(&mut self.0[(signal - 1) as usize], action))
    }

    /// What `exec` keeps: ignored signals stay ignored, handled ones go
    /// back to their default, since the handlers are gone with the
    /// program.
    pub fn reset_for_exec(&mut self) {
        for action in &mut self.0 {
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
        let action = &mut self.0[(signal - 1) as usize];
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
    /// The sender's process number, or the address a fault was on.
    pub pid: u64,
    /// For SIGCHLD the child's code or signal; for a real-time signal its
    /// value.
    pub status: u64,
}

/// `si_code` of a signal sent with `kill`.
pub const SI_USER: i32 = 0;
/// `si_code` of a signal the kernel sent.
pub const SI_KERNEL: i32 = 0x80;
/// `si_code` of a timer's signal.
pub const SI_TIMER: i32 = -2;
/// `si_code` of a signal sent to a thread.
pub const SI_TKILL: i32 = -6;

/// The signals pending for a thread, or for a process (taken by whichever
/// of its threads does not block them first).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Pending {
    /// The signals pending.
    set: u64,
    /// What each pending signal carries: for a real-time signal sent more
    /// than once, the last one's.
    info: [Info; SIGNALS as usize],
    /// How many times each real-time signal is pending.
    queued: [u16; (SIGNALS - SIGRTMIN + 1) as usize],
}

impl Default for Pending {
    fn default() -> Pending {
        Pending::new()
    }
}

impl Pending {
    /// Nothing pending.
    pub const fn new() -> Pending {
        Pending {
            set: 0,
            info: [Info { code: 0, pid: 0, status: 0 }; SIGNALS as usize],
            queued: [0; (SIGNALS - SIGRTMIN + 1) as usize],
        }
    }

    /// The signals pending.
    pub const fn set(&self) -> u64 {
        self.set
    }

    /// Makes `signal` pending with `info`. Returns `false` if it was
    /// pending already and is not real-time, or a real-time signal is
    /// pending [`QUEUE_MAX`] times: then nothing changes.
    pub fn add(&mut self, signal: u32, info: Info) -> bool {
        let bit = bit(signal);
        if bit == 0 {
            return false;
        }
        if bit & REAL_TIME != 0 {
            let queued = &mut self.queued[(signal - SIGRTMIN) as usize];
            if *queued >= QUEUE_MAX {
                return false;
            }
            *queued += 1;
        } else if self.set & bit != 0 {
            return false;
        }
        self.set |= bit;
        self.info[(signal - 1) as usize] = info;
        true
    }

    /// Takes the signal to deliver next among those in `allowed`: a fault's
    /// first, then the lowest-numbered, as Linux does.
    pub fn take(&mut self, allowed: u64) -> Option<(u32, Info)> {
        let ready = self.set & allowed;
        if ready == 0 {
            return None;
        }
        let synchronous = ready & SYNCHRONOUS;
        let from = if synchronous != 0 { synchronous } else { ready };
        let signal = from.trailing_zeros() + 1;
        let info = self.info[(signal - 1) as usize];
        let bit = bit(signal);
        if bit & REAL_TIME != 0 {
            let queued = &mut self.queued[(signal - SIGRTMIN) as usize];
            *queued = queued.saturating_sub(1);
            if *queued == 0 {
                self.set &= !bit;
            }
        } else {
            self.set &= !bit;
        }
        Some((signal, info))
    }

    /// Drops every signal of `set` that is pending.
    pub fn discard(&mut self, set: u64) {
        let gone = self.set & set;
        self.set &= !gone;
        for signal in SIGRTMIN..=SIGNALS {
            if gone & bit(signal) != 0 {
                self.queued[(signal - SIGRTMIN) as usize] = 0;
            }
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
