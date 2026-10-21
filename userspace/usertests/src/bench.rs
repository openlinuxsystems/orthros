// SPDX-FileCopyrightText: 2026 The OrthrOS contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Benchmarks of one program, for `ob bench`: the cheapest system calls,
//! fresh pages and memory bandwidth.

use libradix::abi::capability::{self, ObjectKind, Rights};
use libradix::abi::startup::{SLOT_FIRST_EXTRA, SLOT_MEMORY, SLOT_RANDOM, SLOT_SPACE};
use libradix::abi::{memory, random, syscalls};
use libradix::time::{Nanos, cycles};
use libradix::{SYS, println, sys};

use crate::{Argument, checks};
use crate::{check, ok};

const PAGE_SIZE: u64 = 4096;

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

/// The copies that measure memory bandwidth.
const COPY_ROUNDS: u64 = 30;

/// Where the memory benchmark maps its pages: one fresh page, then the
/// buffers it copies between, one memory object's frame after another.
const PAGE_AT: u64 = 0x6000_0000;
const COPY_AT: u64 = 0x8000_0000;

/// Slots: the one-page memory object fresh pages come from, the page
/// itself, and the frames of the copy buffers.
const SLOT_PAGE_MEMORY: u32 = 96;
const SLOT_PAGE: u32 = 97;
const SLOT_COPY: u32 = 100;

/// Each of the memory objects the kernel passes for the copy buffers:
/// 2^order pages.
const COPY_ORDER: u32 = 10;

/// What a fresh, zeroed page costs (made from memory, mapped, written,
/// unmapped and deleted, which gives its memory back), and how fast the
/// CPU copies between two buffers larger than its caches.
pub fn memory(argument: Argument) -> u64 {
    let rw = Rights::READ.union(Rights::WRITE);
    let made = memory::retype(&SYS, SLOT_MEMORY, ObjectKind::Memory, 0, SLOT_PAGE_MEMORY, 1);
    if ok!(made, "a one-page memory object").is_some() {
        measure("fresh_page", argument.rounds, || {
            let page = memory::retype(&SYS, SLOT_PAGE_MEMORY, ObjectKind::Frame, 0, SLOT_PAGE, 1)
                .and_then(|()| syscalls::map(&SYS, SLOT_SPACE, SLOT_PAGE, PAGE_AT, rw, 0, 1));
            if page.is_err() {
                return false;
            }
            // SAFETY: the page was just mapped read-write at PAGE_AT, and
            // nothing else uses it.
            unsafe { (PAGE_AT as *mut u64).write_volatile(1) };
            syscalls::unmap(&SYS, SLOT_SPACE, PAGE_AT, 1).is_ok()
                && capability::delete(&SYS, SLOT_PAGE).is_ok()
        });
    }

    // The buffers: the memory objects from SLOT_FIRST_EXTRA on, one frame
    // each, mapped one after another; half to copy from, half to copy to.
    let objects = libradix::startup().extra;
    let object_bytes = PAGE_SIZE << COPY_ORDER;
    for i in 0..objects {
        let (from, to) = (SLOT_FIRST_EXTRA + i, SLOT_COPY + i);
        let at = COPY_AT + u64::from(i) * object_bytes;
        let mapped = memory::retype(&SYS, from, ObjectKind::Frame, COPY_ORDER, to, 1)
            .and_then(|()| syscalls::map(&SYS, SLOT_SPACE, to, at, rw, 0, 1 << COPY_ORDER));
        if ok!(mapped, "a copy buffer").is_none() {
            return checks::failures();
        }
    }
    let half = u64::from(objects / 2) * object_bytes;
    if half == 0 {
        check!(false, "no memory objects to copy between");
        return checks::failures();
    }
    let (from, to) = (COPY_AT, COPY_AT + half);
    // A first copy, so that no page is touched for the first time below.
    // SAFETY: both halves were mapped read-write above and do not overlap.
    unsafe { copy(from, to, half) };
    let start = cycles();
    for _ in 0..COPY_ROUNDS {
        // SAFETY: as above.
        unsafe { copy(from, to, half) };
    }
    let mib = COPY_ROUNDS * (half >> 20);
    println!("BENCH name=memory_copy ns={} iterations={mib}", Nanos::per(cycles() - start, mib));
    checks::failures()
}

/// Copies `len` bytes from `from` to `to` with `rep movsb`, as
/// tools/linux-bench does, so that both measure the same instructions.
///
/// # Safety
///
/// Both ranges must be mapped, `to` writable, and they must not overlap.
unsafe fn copy(from: u64, to: u64, len: u64) {
    // SAFETY: the caller's contract.
    unsafe {
        core::arch::asm!(
            "rep movsb",
            inout("rsi") from => _,
            inout("rdi") to => _,
            inout("rcx") len => _,
            options(nostack, preserves_flags),
        );
    }
}
