// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Terminals' line discipline and the byte rings pipes and terminals
//! buffer through (Phase 8, radix-terminal(4)).
//!
//! [`Ring`] is a fixed ring of bytes: a pipe's 64 KiB, a terminal's input
//! line and its output. [`Ldisc`] is POSIX's general terminal interface
//! for one terminal: given each byte typed, it edits lines (ICANON),
//! echoes (ECHO), and says which signal a signal character asks for
//! (ISIG); given what programs write, it makes what the screen needs
//! (OPOST). Its settings are a [`Settings`], which reads and writes the C
//! library's `struct termios` (radix-terminal(4)'s Termios).
//!
//! The crate knows nothing of processes, locks or user memory: the kernel
//! (`radix::fs::tty`) holds an `Ldisc` behind a lock, feeds it, sends the
//! signals it names to the foreground process group, and copies between
//! its rings and programs. It is safe code and its tests run on the host.

#![cfg_attr(not(test), no_std)]
#![forbid(unsafe_code)]

mod ldisc;
mod ring;
#[cfg(test)]
mod tests;

pub use ldisc::{Ldisc, Settings, TERMIOS_SIZE};
pub use ring::Ring;
