// SPDX-FileCopyrightText: 2026 The OrthrOS contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! hello: the test program Spark starts by default, from
//! `/system/tests/hello` in the boot archive. It prints its arguments,
//! its process number and where it was placed, uses the heap, and exits
//! with 0; the `spark` boot test checks each line.

#![no_std]
#![no_main]

extern crate alloc;

use alloc::string::String;
use alloc::vec::Vec;
use core::fmt::Write;

use libradix::abi::process;
use libradix::abi::startup::{SLOT_PROCESS, Startup};
use radix_rt::{SYS, args, entry, println};

entry!(main);

fn main(start: &Startup) -> u64 {
    let number = process::info(&SYS, SLOT_PROCESS).map_or(0, |info| info.number);
    let mut line = String::new();
    for (i, arg) in args().enumerate() {
        let _ = write!(line, "{}{arg}", if i == 0 { "" } else { " " });
    }
    println!("hello: process {number}, arguments: {line}");
    println!("hello: placed at a random address, moved by {:#x}", start.image_bias);
    // The heap: small blocks, and one that takes frames of its own.
    let small: Vec<u64> = (1..=1000).collect();
    let large = alloc::vec![7u8; 64 * 1024];
    let sum: u64 = small.iter().sum();
    let bytes: usize = large.iter().map(|&b| usize::from(b)).sum();
    println!("hello: the heap holds 1000 numbers summing to {sum} and {bytes} bytes of sevens");
    if sum != 500_500 || bytes != 7 * 64 * 1024 {
        return 1;
    }
    println!("hello: goodbye");
    0
}
