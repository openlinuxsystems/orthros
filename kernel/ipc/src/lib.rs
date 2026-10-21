// SPDX-FileCopyrightText: 2026 The OrthrOS contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! The arithmetic of Radix IPC, kept apart from the kernel so that it runs
//! and is tested on the host and proved with Kani (ADR 0032, ADR 0034):
//!
//! - [`info`]: the info word that describes a message, and the checks a
//!   sender's info word must pass;
//! - [`range`]: whether an address range a program passes lies in its half
//!   of the address space;
//! - [`ring`]: where a ring's queues lie in its shared memory, and the
//!   accounting that keeps the kernel from ever writing a completion the
//!   program has no room for, whatever the program writes in the shared
//!   indexes;
//! - [`notify`]: what signalling does to a notification's word.
//!
//! Everything here is plain arithmetic on integers, with no locks, memory
//! or hardware; the kernel's `ipc` module is the mechanism around it.

#![cfg_attr(not(any(test, kani)), no_std)]
#![forbid(unsafe_code)]

pub mod info;
pub mod notify;
pub mod range;
pub mod ring;

#[cfg(kani)]
mod proofs;
#[cfg(test)]
mod tests;
