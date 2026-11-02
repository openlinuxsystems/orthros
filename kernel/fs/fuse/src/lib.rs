// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Filesystems served by programs (ADR 0226), through `/devices/fuse`, in
//! Linux's FUSE protocol: a network share's files come from the program
//! that speaks to the server (`mount -t smb`), and any program written
//! for Linux's FUSE, or with a library for it such as fuser, can serve
//! one.
//!
//! - [`abi`] is the protocol: the messages, written from Linux's
//!   description of them.
//! - [`Connection`] carries requests to the program and its answers
//!   back: what `/devices/fuse` reads and writes.
//! - [`FuseFs`] is the mounted filesystem, and [`FuseNode`] a node of
//!   it: each call becomes a request and waits for its answer.
//!
//! What a program can make the kernel do is bounded: a mount of it is
//! never a way to the kernel's devices (a device node on it opens
//! nothing, as Linux's `nodev`) or to set-ID programs (their bits are
//! never shown, as `nosuid`), it is checked against the owners and modes
//! it gives, as Linux's `default_permissions`, and a call waiting for it
//! ends when its thread is killed.
//!
//! The crate is a compartment: it has no `unsafe` code, and builds for
//! the host (feature `host`), where its tests serve a filesystem from a
//! thread.

#![cfg_attr(not(any(test, feature = "host")), no_std)]
#![forbid(unsafe_code)]
#![feature(allocator_ext)]
#![feature(vec_push_within_capacity)]

extern crate alloc;

pub mod abi;
mod conn;
mod fs;

#[cfg(test)]
mod tests;

use radix_sync::Once;

pub use conn::{Connection, DeviceError, MAX_DATA, MIN_READ_BUFFER, max_request};
pub use fs::{FuseFs, FuseNode};

/// How a wait ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Wake {
    /// What it waited for happened.
    Ready,
    /// The thread has a signal to take.
    Signal,
    /// The thread is being stopped: its process was killed, or a signal
    /// ends it.
    Stop,
}

/// Who makes a request: the running thread's user, group and process.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Caller {
    /// The user's number.
    pub uid: u32,
    /// The group's number.
    pub gid: u32,
    /// The process's number.
    pub pid: u32,
}

/// What the crate needs from the kernel.
pub struct Hooks {
    /// Sleeps until `ready` holds (it reads only atomics) and someone
    /// calls `wake` with `key`; ends early, unless `ready` holds, when
    /// the thread is being stopped, or, with `signals`, when it has a
    /// signal to take.
    pub wait: fn(key: usize, ready: &dyn Fn() -> bool, signals: bool) -> Wake,
    /// Wakes the waits on `key`, and any poll of a connection.
    pub wake: fn(key: usize),
    /// Who the running thread is.
    pub caller: fn() -> Caller,
    /// A clock that never goes back, in nanoseconds: how long a node's
    /// attributes may be kept is measured on it.
    pub now: fn() -> u64,
}

static HOOKS: Once<Hooks> = Once::new();

/// Registers the kernel's hooks, once, before any connection is made.
pub fn set_hooks(hooks: Hooks) {
    let _ = HOOKS.set(hooks);
}

fn hooks() -> &'static Hooks {
    HOOKS.get().unwrap_or(&NONE)
}

/// Before the hooks are registered: waits spin, nothing wakes, and nobody
/// calls.
static NONE: Hooks = Hooks {
    wait: |_, ready, _| {
        while !ready() {
            core::hint::spin_loop();
        }
        Wake::Ready
    },
    wake: |_| {},
    caller: Caller::default_fn,
    now: || 0,
};

impl Caller {
    fn default_fn() -> Caller {
        Caller::default()
    }
}
