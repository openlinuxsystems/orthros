// SPDX-FileCopyrightText: 2026 The OrthrOS contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! The Radix system call library for user programs.
//!
//! - [`sys`]: the `syscall` instruction behind [`radix_abi::Syscalls`], so
//!   every wrapper ridlc generates in `radix_abi` works through [`SYS`];
//! - [`entry!`] and [`startup`]: where a program starts, and its start-up
//!   page (radix-startup(2));
//! - [`println!`], [`exit`] and the panic handler, through the kernel log
//!   capability every program has;
//! - [`time`]: the time-stamp counter, for benchmarks;
//! - [`ring`]: a radix-ring(2) ring's two queues;
//! - [`channel`]: a shared-memory channel between two programs, a frame
//!   both map and two notifications.
//!
//! Until Phase 6 there are no processes and no heap here: a program is
//! one thread with what the kernel gave it.

#![no_std]

pub mod channel;
pub mod print;
pub mod ring;
pub mod sys;
pub mod time;

pub use radix_abi as abi;
pub use sys::SYS;

use radix_abi::startup::{self, Startup};

/// The exit code of a program that panicked.
pub const PANIC_EXIT: u64 = 0xbad;

/// The program's start-up page.
pub fn startup() -> &'static Startup {
    // SAFETY: the kernel maps the start-up page read-only at
    // STARTUP_ADDRESS before the program starts and never unmaps it while
    // the program runs; it holds a Startup (radix-startup(2)), which is plain
    // integers, so any bytes are a valid value.
    unsafe { &*(startup::STARTUP_ADDRESS as *const Startup) }
}

/// Ends the program's thread with `code`.
pub fn exit(code: u64) -> ! {
    let _ = radix_abi::thread::exit(&SYS, startup::SLOT_THREAD, code);
    // The kernel does not return from a thread's own exit.
    loop {
        core::hint::spin_loop();
    }
}

/// Where the program starts: `_start` aligns the stack and calls
/// [`start`], which runs the program's `main` and exits with what it
/// returns.
#[macro_export]
macro_rules! entry {
    ($main:path) => {
        #[unsafe(no_mangle)]
        extern "C" fn radix_program_main() -> u64 {
            $main($crate::startup())
        }
    };
}

unsafe extern "C" {
    /// The program's `main`, from [`entry!`].
    fn radix_program_main() -> u64;
}

core::arch::global_asm!(
    r#"
    .section .text._start, "ax"
    .global _start
_start:
    xor ebp, ebp
    and rsp, -16
    call {start}
    ud2
    "#,
    start = sym start,
);

/// Called by `_start` with the start-up page's address, which [`startup`]
/// already knows.
extern "C" fn start(_startup: u64) -> ! {
    // SAFETY: `entry!` defines the function with this signature.
    let code = unsafe { radix_program_main() };
    exit(code)
}

#[panic_handler]
fn panic(info: &core::panic::PanicInfo<'_>) -> ! {
    println!("panicked: {}", info);
    exit(PANIC_EXIT)
}
