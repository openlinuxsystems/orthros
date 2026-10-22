// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! The runtime of Radix programs, on top of libradix (Phases 6 and 7).
//!
//! - [`heap`]: the allocator behind `alloc`, so programs have `Box`, `Vec`
//!   and `String`;
//! - [`slots`]: the program's free capability slots;
//! - [`vm`]: free addresses in the program's own part of its address
//!   space;
//! - [`args`]: the program's arguments;
//! - [`spawn`]: starting a program in a new process and reaping it;
//! - [`fs`]: files and directories, through the capabilities that reach
//!   them;
//! - [`bootfs`]: the boot archive's files, read-only, for Spark.
//!
//! A program built with it starts as every Radix program does
//! (`libradix::entry!`), and links its allocator by using the crate.
//! docs/design/processes.md describes how programs are started.

#![no_std]

extern crate alloc;

pub mod args;
pub mod bootfs;
pub mod fs;
pub mod heap;
mod lock;
pub mod slots;
pub mod spawn;
pub mod vm;

pub use args::args;
pub use libradix::{SYS, entry, exit, println, startup};
