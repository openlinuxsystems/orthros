// SPDX-FileCopyrightText: 2026 The OrthrOS contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! The user-mode test and benchmark programs.
//!
//! One image holds them all; the kernel's `user`, `ipc` and `bench`
//! self-tests (`kernel/radix/src/usertest.rs`, which numbers the roles the
//! same way) start a program for each role they need, and check its exit
//! code: 0 when every check passed, otherwise the number that failed.
//!
//! - `probe`: every system call and method, with good and bad arguments;
//! - `fault`: one fault of the kind its argument names, which must end it;
//! - pairs that ping-pong over an endpoint, pass capabilities, signal
//!   notifications, share a channel, and post to each other's rings;
//! - `syscall-bench`: the cost of the cheapest system calls.
//!
//! A pair's programs get the same endpoint and two notifications: the
//! first notification is the one each waits on, the second the other's.

#![no_std]
#![no_main]

mod bench;
mod checks;
mod fault;
mod pairs;
mod probe;

use libradix::abi::startup::Startup;
use libradix::println;

/// The roles, as the kernel numbers them.
mod role {
    pub const PROBE: u64 = 1;
    pub const FAULT: u64 = 2;
    pub const ECHO: u64 = 3;
    pub const CALL: u64 = 4;
    pub const CAP_SEND: u64 = 5;
    pub const CAP_RECEIVE: u64 = 6;
    pub const NOTIFY_PING: u64 = 7;
    pub const NOTIFY_PONG: u64 = 8;
    pub const CHANNEL_A: u64 = 9;
    pub const CHANNEL_B: u64 = 10;
    pub const RING_A: u64 = 11;
    pub const RING_B: u64 = 12;
    pub const SYSCALL_BENCH: u64 = 13;
}

/// The flag in the argument that asks for timings.
pub const BENCH: u64 = 1 << 63;
/// The flag that says the pair runs on two CPUs.
pub const ACROSS: u64 = 1 << 62;

/// What a role's argument says.
#[derive(Clone, Copy)]
pub struct Argument {
    /// How many rounds to run.
    pub rounds: u64,
    /// Whether to print timings.
    pub bench: bool,
    /// Whether the other program of the pair runs on another CPU.
    pub across: bool,
}

impl Argument {
    fn of(word: u64) -> Argument {
        Argument {
            rounds: (word & 0xffff_ffff).max(1),
            bench: word & BENCH != 0,
            across: word & ACROSS != 0,
        }
    }

    /// The suffix of a benchmark's name.
    pub fn place(&self) -> &'static str {
        if self.across { "cross_cpu" } else { "same_cpu" }
    }
}

fn main(start: &Startup) -> u64 {
    let argument = Argument::of(start.argument);
    let failures = match start.role {
        role::PROBE => probe::run(),
        role::FAULT => fault::run(start.argument),
        role::ECHO => pairs::echo(),
        role::CALL => pairs::call(argument),
        role::CAP_SEND => pairs::cap_send(),
        role::CAP_RECEIVE => pairs::cap_receive(),
        role::NOTIFY_PING => pairs::notify_ping(argument),
        role::NOTIFY_PONG => pairs::notify_pong(argument),
        role::CHANNEL_A => pairs::channel_a(argument),
        role::CHANNEL_B => pairs::channel_b(),
        role::RING_A => pairs::ring_a(argument),
        role::RING_B => pairs::ring_b(),
        role::SYSCALL_BENCH => bench::syscalls(argument),
        other => {
            println!("no role {other}");
            1
        }
    };
    if failures > 0 {
        println!("{failures} checks failed");
    }
    failures
}

libradix::entry!(main);
