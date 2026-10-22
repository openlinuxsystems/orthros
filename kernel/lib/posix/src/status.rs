// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Wait statuses: what `wait` says about a child, encoded as Linux encodes
//! them, so that a C library's `WIFEXITED` and friends read them as they
//! are (radix-process(2)).

/// A child that exited with `code`: its low byte, shifted left by 8.
pub const fn exited(code: u64) -> u32 {
    ((code & 0xff) as u32) << 8
}

/// A child that signal `signal` ended: the signal, in the low 7 bits.
pub const fn killed(signal: u32) -> u32 {
    signal & 0x7f
}

/// A child that signal `signal` stopped: 0x7f, with the signal shifted
/// left by 8.
pub const fn stopped(signal: u32) -> u32 {
    0x7f | ((signal & 0xff) << 8)
}

/// A child that continued.
pub const CONTINUED: u32 = 0xffff;

/// How a process ended, for its wait status.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Ended {
    /// It exited with this code.
    Exited(u64),
    /// This signal ended it.
    Killed(u32),
}

impl Ended {
    /// Its wait status.
    pub const fn status(self) -> u32 {
        match self {
            Ended::Exited(code) => exited(code),
            Ended::Killed(signal) => killed(signal),
        }
    }

    /// SIGCHLD's `si_code` for it: 1 (exited) or 2 (killed).
    pub const fn code(self) -> i32 {
        match self {
            Ended::Exited(_) => 1,
            Ended::Killed(_) => 2,
        }
    }

    /// SIGCHLD's `si_status` for it: the exit code or the signal.
    pub const fn value(self) -> u64 {
        match self {
            Ended::Exited(code) => code & 0xff,
            Ended::Killed(signal) => signal as u64,
        }
    }
}
