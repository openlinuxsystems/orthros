// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Benchmarks of one program, for `ob bench`: the cheapest system calls,
//! fresh pages, memory bandwidth and files.

use libradix::abi::capability::{self, ObjectKind, Rights};
use libradix::abi::startup::{SLOT_FIRST_EXTRA, SLOT_MEMORY, SLOT_RANDOM, SLOT_SPACE};
use libradix::abi::{directory, file, memory, random, syscalls};
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

/// The copies that measure memory bandwidth: batches of them, of which
/// the fastest counts, as in tools/linux-bench, so that a moment of
/// contention on a shared machine does not.
const COPY_BATCH: u64 = 6;
const COPY_BATCHES: usize = 5;

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
    let mut fastest = u64::MAX;
    for _ in 0..COPY_BATCHES {
        let start = cycles();
        for _ in 0..COPY_BATCH {
            // SAFETY: as above.
            unsafe { copy(from, to, half) };
        }
        fastest = fastest.min(cycles() - start);
    }
    let mib = COPY_BATCH * (half >> 20);
    println!("BENCH name=memory_copy ns={} iterations={mib}", Nanos::per(fastest, mib));
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

/// The slots of the file benchmarks: the tmpfs directory the kernel
/// passes, the file they read and write, and the one `open` puts what it
/// opens in, again and again.
const SLOT_DIRECTORY: u32 = SLOT_FIRST_EXTRA;
const SLOT_FILE: u32 = 96;
const SLOT_OPENED: u32 = 97;

/// The file's name and size.
const FILE_NAME: &[u8] = b"bench";
const FILE_SIZE: usize = PAGE_SIZE as usize;

/// What the file benchmarks read into and write from.
static mut BUFFER: [u8; FILE_SIZE] = [0x5a; FILE_SIZE];

/// What a file costs on a tmpfs, as tools/linux-bench measures it on one:
/// opening and closing it, its status, and reading and writing a page of
/// it, the page already in memory.
pub fn files(argument: Argument) -> u64 {
    let rw = Rights::READ.union(Rights::WRITE);
    let create = directory::OpenFlags::CREATE;
    // SAFETY: only this function, on this program's one thread, touches
    // the buffer, and the kernel only while a call below runs.
    let buffer = unsafe { &mut *core::ptr::addr_of_mut!(BUFFER) };
    let made = directory::open(&SYS, SLOT_DIRECTORY, FILE_NAME, create, rw, SLOT_FILE)
        .and_then(|_| file::write(&SYS, SLOT_FILE, 0, buffer));
    if ok!(made, "the file to benchmark").is_none() {
        return checks::failures();
    }
    let rounds = argument.rounds;
    measure("file_open_close", rounds, || {
        let none = directory::OpenFlags::NONE;
        directory::open(&SYS, SLOT_DIRECTORY, FILE_NAME, none, Rights::READ, SLOT_OPENED).is_ok()
            && capability::delete(&SYS, SLOT_OPENED).is_ok()
    });
    measure("file_stat", rounds, || {
        directory::stat(&SYS, SLOT_DIRECTORY, FILE_NAME, directory::PathFlags::NONE)
            .is_ok_and(|stat| stat.size == FILE_SIZE as u64)
    });
    measure("file_read_4k", rounds, || {
        file::read(&SYS, SLOT_FILE, 0, buffer).is_ok_and(|read| read.count == FILE_SIZE as u64)
    });
    measure("file_write_4k", rounds, || {
        file::write(&SYS, SLOT_FILE, 0, buffer).is_ok_and(|wrote| wrote.count == FILE_SIZE as u64)
    });
    checks::failures()
}
