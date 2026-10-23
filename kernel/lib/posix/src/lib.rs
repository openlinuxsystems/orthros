// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! The rules of POSIX processes that the Radix kernel keeps (Phase 8,
//! docs/design/posix.md).
//!
//! Radix's own model is capabilities: a process belongs to whoever holds
//! its capability. A C library also needs what POSIX promises, and some of
//! it only the kernel can keep, because it spans processes or must happen
//! between two instructions of a program: the tree of parents and children
//! that `wait` reads, process groups and sessions for job control, signals
//! and what each process does with them, and the start of a program that
//! `exec` lays out. This crate holds those rules as plain data and
//! functions, with no locks, no memory of the kernel's and no `unsafe`
//! code, so that its host tests and property tests check them:
//!
//! - [`signal`]: signal sets, the action of each signal, the default
//!   actions, and the signals a thread or process has pending;
//! - [`status`]: wait statuses, encoded as Linux does;
//! - [`tree`]: the process tree, with process groups and sessions, which
//!   `fork`, `wait`, `kill`, `setpgid` and `setsid` change and read;
//! - [`exec`]: the block of arguments and environment `exec` takes, `#!`
//!   lines, and the System V initial stack a program starts with.
//!
//! The kernel (`radix::user::posix`) holds a [`tree::Tree`] behind a lock,
//! keeps a [`signal::Actions`] in each process and a [`signal::Pending`]
//! in each process and thread, and carries out what these say.

#![cfg_attr(not(test), no_std)]
#![forbid(unsafe_code)]
#![feature(vec_push_within_capacity)]
// Kani's own, older nightly has the allocator API unstable still.
#![cfg_attr(kani, feature(allocator_api))]

extern crate alloc;

pub mod exec;
pub mod signal;
pub mod status;
pub mod tree;

#[cfg(test)]
mod tests;
