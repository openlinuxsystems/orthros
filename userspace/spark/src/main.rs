// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Spark, the first program (Phase 6, spark(8)).
//!
//! The kernel starts Spark from the boot archive with the archive mapped
//! read-only and a memory object for the programs it starts
//! (`SLOT_FIRST_EXTRA`). Spark v0 starts one program from the archive,
//! the one its arguments name (`/system/tests/hello` by default, with any
//! arguments after it), logs over the kernel log, waits for the program
//! to exit, reaps it and checks that all of its memory came back. Then it
//! waits. Spark must not end: the kernel stops if it does. Later phases
//! make it the supervisor of the system's services.

#![no_std]
#![no_main]

use libradix::abi::capability::{self, ObjectKind};
use libradix::abi::notification::WaitFlags;
use libradix::abi::startup::{SLOT_FIRST_EXTRA, SLOT_PROCESS, Startup};
use libradix::abi::{memory, process, syscalls};
use radix_rt::fs::BootFs;
use radix_rt::spawn::{self, Spawn};
use radix_rt::{SYS, args, entry, println, slots};

entry!(main);

/// The program Spark starts when its arguments name none.
const DEFAULT_PROGRAM: &str = "/system/tests/hello";

/// The memory the program it starts gets: 2^order pages.
const PROGRAM_MEMORY_ORDER: u32 = 9;

/// The most arguments it passes on.
const ARGS_MAX: usize = 16;

fn main(start: &Startup) -> u64 {
    let number = process::info(&SYS, SLOT_PROCESS).map_or(0, |info| info.number);
    println!("spark: Spark v0 is process {number}, moved by {:#x}", start.image_bias);
    run();
    // Spark never exits: it waits for good on a notification nothing
    // signals.
    println!("spark: nothing more to do; waiting");
    idle()
}

/// Starts the program and reaps it.
fn run() {
    let fs = match BootFs::mount() {
        Ok(fs) => fs,
        Err(err) => {
            println!("spark: {err}");
            return;
        }
    };
    println!(
        "spark: boot archive of {} KiB with {} files at {:#x}",
        fs.size().div_ceil(1024),
        fs.files(),
        libradix::startup().bootfs_address
    );
    let mut words = args().skip(1);
    let path = words.next().unwrap_or(DEFAULT_PROGRAM);
    let mut rest = [""; ARGS_MAX];
    let mut count = 0;
    for word in words.take(ARGS_MAX) {
        rest[count] = word;
        count += 1;
    }
    let Some(image) = fs.read(path) else {
        println!("spark: {path} is not in the boot archive");
        return;
    };
    let pool = SLOT_FIRST_EXTRA;
    let is_memory = capability::info(&SYS, pool).is_ok_and(|info| info.kind == ObjectKind::Memory);
    if !is_memory {
        println!("spark: the kernel gave no memory for programs");
        return;
    }
    let spawn = Spawn {
        path,
        image,
        args: &rest[..count],
        pool,
        memory_order: PROGRAM_MEMORY_ORDER,
        share: &[],
    };
    let child = match spawn::spawn(&spawn) {
        Ok(child) => child,
        Err(err) => {
            println!("spark: could not start {path}: {err}");
            return;
        }
    };
    let number = child.number();
    println!("spark: started {path} as process {number}");
    match child.wait() {
        Ok(code) => println!("spark: process {number} ({path}) exited with code {code:#x}"),
        Err(err) => println!("spark: waiting for process {number} failed: {err}"),
    }
    child.reap();
    match memory::info(&SYS, pool) {
        Ok(info) if info.used == 0 => {
            println!("spark: reaped process {number}; all of its memory came back")
        }
        Ok(info) => {
            println!("spark: reaped process {number}, but {} pages are still used", info.used)
        }
        Err(status) => {
            println!("spark: reaped process {number}; memory info failed: {:#x}", status.0)
        }
    }
}

/// Waits for good.
fn idle() -> ! {
    let made = slots::take().filter(|&slot| {
        process::create(&SYS, SLOT_PROCESS, ObjectKind::Notification, 0, slot).is_ok()
    });
    loop {
        match made {
            Some(slot) => {
                let _ = syscalls::wait(&SYS, slot, WaitFlags::NONE, 0);
            }
            None => {
                let _ = syscalls::r#yield(&SYS);
            }
        }
    }
}
