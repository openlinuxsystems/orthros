// SPDX-FileCopyrightText: 2026 The OrthrOS contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! The cost of the cheapest system calls, for `ob bench`.

use libradix::abi::startup::SLOT_RANDOM;
use libradix::abi::{capability, random, syscalls};
use libradix::time::{Nanos, cycles};
use libradix::{SYS, println, sys};

use crate::{Argument, checks};

/// Runs `body` `rounds` times and prints its cost.
fn measure(name: &str, rounds: u64, mut body: impl FnMut() -> bool) {
    let start = cycles();
    for _ in 0..rounds {
        if !body() {
            checks::that(false, format_args!("{name} failed"));
            return;
        }
    }
    let ns = Nanos::per(cycles() - start, rounds);
    println!("BENCH name={name} ns={ns} iterations={rounds}");
}

pub fn syscalls(argument: Argument) -> u64 {
    let rounds = argument.rounds;
    // A number the kernel does not know: entry and exit, and nothing else.
    measure("syscall_null", rounds, || sys::syscall(99, [0; 10]).0 != 0);
    measure("syscall_yield", rounds, || syscalls::r#yield(&SYS).is_ok());
    measure("cap_info", rounds, || capability::info(&SYS, SLOT_RANDOM).is_ok());
    measure("random_words", rounds / 4, || random::words(&SYS, SLOT_RANDOM).is_ok());
    checks::failures()
}
