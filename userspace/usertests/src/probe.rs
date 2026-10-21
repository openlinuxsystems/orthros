// SPDX-FileCopyrightText: 2026 The OrthrOS contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! The probe: every system call and method the kernel has, with good
//! arguments and bad ones, checking each answer against radix-syscalls(2)
//! and the interface pages.

use libradix::abi::capability::{self, ObjectKind, Rights};
use libradix::abi::endpoint::{INFO_WORDS_SHIFT, NO_SLOT};
use libradix::abi::errors::Kernel;
use libradix::abi::notification::WaitFlags;
use libradix::abi::process::{self, ProcessState};
use libradix::abi::ring::Opcode;
use libradix::abi::startup::{
    self, SLOT_LOG, SLOT_MEMORY, SLOT_PROCESS, SLOT_RANDOM, SLOT_SPACE, SLOT_THREAD,
    STARTUP_VERSION,
};
use libradix::abi::{Status, address_space, memory, random, ring, syscalls, thread};
use libradix::ring::Ring;
use libradix::{SYS, println, startup, sys};

use crate::checks;
use crate::{check, fails, ok};

/// Free slots for the probe's own capabilities.
const FRAME: u32 = 64;
const COPY: u32 = 65;
const MOVED: u32 = 66;
const CHILD: u32 = 67;
const RING: u32 = 70;
const BIG: u32 = 71;
const REUSED: u32 = 72;
const CHILD_PROCESS: u32 = 73;
const NOTE: u32 = 74;
const ENDPOINT: u32 = 75;
const CHILD_SPACE: u32 = 76;
const GRANDCHILD: u32 = 77;
const GRANDCHILD_VIEW: u32 = 78;

/// Free addresses for its mappings.
const MAP_AT: u64 = 0x2000_0000;
const TLS_AT: u64 = 0x2100_0000;
const RING_AT: u64 = 0x3000_0000;

/// An address in the kernel's half.
const KERNEL: u64 = 0xffff_8000_0000_0000;

const RW: Rights = Rights::READ.union(Rights::WRITE);

pub fn run() -> u64 {
    start_page();
    capabilities();
    system_calls();
    registers();
    random();
    threads();
    memory();
    derivation();
    log();
    methods();
    rings();
    processes();
    println!("probe: {} checks failed", checks::failures());
    checks::failures()
}

/// Makes a raw system call, for arguments the wrappers would not pass.
fn raw(number: u64, args: [u64; 10]) -> Result<[u64; 10], Status> {
    let (status, results) = sys::syscall(number, args);
    if status == 0 { Ok(results) } else { Err(Status(status)) }
}

/// A raw `cap_call` of method `label` with `words`.
fn raw_call(cap: u32, label: u32, words: &[u64]) -> Result<[u64; 10], Status> {
    let mut args = [0u64; 10];
    args[0] = u64::from(cap);
    args[1] = u64::from(label) | ((words.len() as u64) << INFO_WORDS_SHIFT);
    args[2..2 + words.len()].copy_from_slice(words);
    raw(syscalls::CAP_CALL, args)
}

fn start_page() {
    let start = startup();
    check!(start.version == STARTUP_VERSION, "start-up page version {}", start.version);
    check!(start.role == 1, "role {}", start.role);
    check!(start.tsc_hz > 0, "the time-stamp counter's frequency is unknown");
    check!(start.memory_bytes > 0, "no memory left: {}", start.memory_bytes);
    check!(start.extra == 0, "{} extra capabilities", start.extra);
    if let Some(info) = ok!(memory::info(&SYS, SLOT_MEMORY), "memory info") {
        let free = (info.pages - info.used) * 4096;
        check!(free == start.memory_bytes, "{free} bytes free, not {}", start.memory_bytes);
    }
}

fn capabilities() {
    let slots = [
        (SLOT_THREAD, ObjectKind::Thread, Rights::CONTROL),
        (SLOT_SPACE, ObjectKind::AddressSpace, Rights::CONTROL),
        (SLOT_MEMORY, ObjectKind::Memory, Rights::CONTROL),
        (SLOT_RANDOM, ObjectKind::Random, Rights::READ),
        (SLOT_LOG, ObjectKind::KernelLog, Rights::WRITE),
        (SLOT_PROCESS, ObjectKind::Process, Rights::CONTROL),
    ];
    for (slot, kind, right) in slots {
        if let Some(info) = ok!(capability::info(&SYS, slot), "info on slot {slot}") {
            check!(info.kind == kind, "slot {slot} holds a {}", info.kind.name());
            check!(info.rights.contains(right), "slot {slot} lacks a right: {:#x}", info.rights.0);
            check!(info.badge == 0, "slot {slot} has badge {}", info.badge);
            check!(info.version == 1, "slot {slot}'s interface is version {}", info.version);
        }
    }
    fails!(capability::info(&SYS, 200), EmptySlot, "info on an empty slot");
    fails!(capability::info(&SYS, startup::SLOTS + 10), NoSuchSlot, "info past the last slot");
    fails!(capability::delete(&SYS, 201), EmptySlot, "deleting an empty slot");
}

/// Markers for the registers `registers` checks.
const MARK_RBX: u64 = 0x5eed_0000_0000_0b0b;
const MARK_RBP: u64 = 0x5eed_0000_0000_0b0f;

/// A system call keeps the registers it does not use, and one that fails
/// leaves 0 in every result register, nothing the kernel had in hand.
fn registers() {
    let mut r = [0u64; 10];
    for (i, word) in r.iter_mut().enumerate() {
        *word = 0x7777_0000_0000_0000 | i as u64;
    }
    let (status, rbx, rbp): (u64, u64, u64);
    // SAFETY: rbx and rbp, which Rust reserves, are saved on the stack and
    // restored before the block ends; system call 99 does not exist, so
    // the kernel touches no memory of the program's. rcx and r11, which
    // `syscall` overwrites anyway, carry the two registers out.
    unsafe {
        core::arch::asm!(
            "push rbx",
            "push rbp",
            "mov rbx, {rbx}",
            "mov rbp, {rbp}",
            "syscall",
            "mov rcx, rbx",
            "mov r11, rbp",
            "pop rbp",
            "pop rbx",
            rbx = const MARK_RBX,
            rbp = const MARK_RBP,
            inlateout("rax") 99u64 => status,
            inlateout("rdi") r[0],
            inlateout("rsi") r[1],
            inlateout("rdx") r[2],
            inlateout("r10") r[3],
            inlateout("r8") r[4],
            inlateout("r9") r[5],
            inlateout("r12") r[6],
            inlateout("r13") r[7],
            inlateout("r14") r[8],
            inlateout("r15") r[9],
            out("rcx") rbx,
            out("r11") rbp,
        );
    }
    check!(
        Kernel::from_status(Status(status)) == Some(Kernel::NoSuchSyscall),
        "system call 99 with markers: {}",
        checks::Shown(Status(status))
    );
    check!(rbx == MARK_RBX && rbp == MARK_RBP, "rbx {rbx:#x} and rbp {rbp:#x} changed");
    check!(r == [0; 10], "a failed call left results behind: {r:x?}");
}

fn system_calls() {
    match raw(99, [0; 10]) {
        Err(status) => {
            check!(status.domain() == Kernel::DOMAIN, "domain {}", status.domain());
            check!(
                Kernel::from_status(status) == Some(Kernel::NoSuchSyscall),
                "system call 99: {}",
                checks::Shown(status)
            );
        }
        Ok(_) => {
            check!(false, "system call 99 succeeded");
        }
    }
    // A failed call of a real system call too.
    let args = [u64::from(SLOT_RANDOM), 7, 7, 7, 7, 7, 7, 7, 7, 7];
    let (status, results) = sys::syscall(syscalls::NOTIFY, args);
    check!(
        Kernel::from_status(Status(status)) == Some(Kernel::WrongKind),
        "notify on the random capability: {}",
        checks::Shown(Status(status))
    );
    check!(results == [0; 10], "notify left results behind: {results:x?}");
    ok!(syscalls::r#yield(&SYS), "yield");
    fails!(syscalls::send(&SYS, SLOT_RANDOM, 0, [0; 8]), WrongKind, "send on a random cap");
    fails!(syscalls::recv(&SYS, SLOT_MEMORY, 0), WrongKind, "recv on memory");
    fails!(syscalls::notify(&SYS, SLOT_SPACE), WrongKind, "notify on an address space");
    fails!(syscalls::wait(&SYS, SLOT_THREAD, WaitFlags::POLL, 0), WrongKind, "wait on a thread");
    fails!(
        syscalls::reply_recv(&SYS, NO_SLOT, 0, [0; 8]),
        NoCaller,
        "a reply with nobody to reply to"
    );
    // Bad info words.
    fails!(
        syscalls::cap_call(&SYS, SLOT_MEMORY, 9 << INFO_WORDS_SHIFT, [0; 8]),
        InvalidArgument,
        "a message of nine words"
    );
    fails!(
        syscalls::cap_call(&SYS, SLOT_MEMORY, 1 << 50, [0; 8]),
        InvalidArgument,
        "an info word with unknown flags"
    );
}

fn random() {
    let (mut a, mut b) = ([0u8; 64], [0u8; 64]);
    ok!(random::read(&SYS, SLOT_RANDOM, &mut a), "random read");
    ok!(random::read(&SYS, SLOT_RANDOM, &mut b), "random read");
    check!(a != b && a != [0; 64], "two random reads gave the same bytes");
    let mut big = [0u8; 4096];
    ok!(random::read(&SYS, SLOT_RANDOM, &mut big), "a random read of a page");
    check!(
        big.iter().filter(|&&byte| byte == 0).count() < 64,
        "a page of random bytes is mostly 0"
    );
    let too_many = random::READ_MAX + 1;
    fails!(
        raw_call(SLOT_RANDOM, random::READ, &[big.as_ptr() as u64, too_many]),
        InvalidArgument,
        "a random read past READ_MAX"
    );
    fails!(
        raw_call(SLOT_RANDOM, random::READ, &[KERNEL, 16]),
        BadAddress,
        "a random read into the kernel"
    );
    fails!(
        raw_call(SLOT_RANDOM, random::READ, &[0x5000_0000, 16]),
        BadAddress,
        "a random read into unmapped memory"
    );
    fails!(
        raw_call(SLOT_RANDOM, random::READ, &[u64::MAX - 4, 16]),
        BadAddress,
        "a random read that wraps around"
    );
    let x = ok!(random::words(&SYS, SLOT_RANDOM), "random words");
    let y = ok!(random::words(&SYS, SLOT_RANDOM), "random words");
    check!(x.is_some() && x != y, "two random words calls gave the same words");
    fails!(random::words(&SYS, SLOT_LOG), NoSuchMethod, "random words from the log");
}

fn threads() {
    let start = startup();
    if let Some(info) = ok!(thread::info(&SYS, SLOT_THREAD), "thread info") {
        check!(
            info.id == u64::from(start.thread_id),
            "thread id {} not {}",
            info.id,
            start.thread_id
        );
        check!(info.cpu == start.cpu, "on CPU {} not {}", info.cpu, start.cpu);
    }
    // Thread-local storage: FS points at a page with a mark in it.
    let page = ok!(map_frame(FRAME, TLS_AT, RW), "a page for TLS");
    if page.is_some() {
        // SAFETY: the page was just mapped read-write at TLS_AT.
        unsafe { (TLS_AT as *mut u64).write_volatile(0x5eed_f00d) };
        ok!(thread::set_tls(&SYS, SLOT_THREAD, TLS_AT), "set_tls");
        let mark: u64;
        // SAFETY: FS's base is TLS_AT, whose first word is mapped.
        unsafe { core::arch::asm!("mov {0}, fs:[0]", out(reg) mark) };
        check!(mark == 0x5eed_f00d, "fs:[0] is {mark:#x}");
        ok!(syscalls::r#yield(&SYS), "yield");
        let again: u64;
        // SAFETY: as above; the kernel restores FS's base on every switch.
        unsafe { core::arch::asm!("mov {0}, fs:[0]", out(reg) again) };
        check!(again == 0x5eed_f00d, "fs:[0] after a yield is {again:#x}");
        ok!(syscalls::unmap(&SYS, SLOT_SPACE, TLS_AT, 1), "unmapping the TLS page");
        ok!(capability::delete(&SYS, FRAME), "deleting the TLS frame");
    }
    fails!(thread::set_tls(&SYS, SLOT_THREAD, KERNEL), InvalidArgument, "TLS in the kernel");
    fails!(
        thread::set_receive_window(&SYS, SLOT_THREAD, u32::MAX, 2),
        InvalidArgument,
        "a receive window past the end"
    );
    ok!(thread::set_receive_window(&SYS, SLOT_THREAD, startup::SLOT_WINDOW, 16), "a window");
    fails!(thread::info(&SYS, SLOT_SPACE), NoSuchMethod, "thread info on an address space");
}

/// Retypes a one-page frame into `slot` and maps it at `address`.
fn map_frame(slot: u32, address: u64, rights: Rights) -> Result<(), Status> {
    memory::retype(&SYS, SLOT_MEMORY, ObjectKind::Frame, 0, slot, 1)?;
    syscalls::map(&SYS, SLOT_SPACE, slot, address, rights, 0, 1)
}

fn memory() {
    ok!(memory::retype(&SYS, SLOT_MEMORY, ObjectKind::Frame, 0, FRAME, 1), "retype a frame");
    if let Some(info) = ok!(memory::frame::info(&SYS, FRAME), "frame info") {
        check!(info.pages == 1, "the frame has {} pages", info.pages);
    }
    fails!(
        memory::retype(&SYS, SLOT_MEMORY, ObjectKind::Frame, 0, FRAME, 1),
        Occupied,
        "retyping into a full slot"
    );
    fails!(
        memory::retype(&SYS, SLOT_MEMORY, ObjectKind::Endpoint, 0, BIG, 1),
        InvalidArgument,
        "retyping memory into an endpoint"
    );
    fails!(
        memory::retype(&SYS, SLOT_MEMORY, ObjectKind::Frame, 12, BIG, 1),
        Exhausted,
        "retyping more memory than there is"
    );
    // Map, write, read, query, unmap.
    if ok!(syscalls::map(&SYS, SLOT_SPACE, FRAME, MAP_AT, RW, 0, 1), "map").is_some() {
        let word = MAP_AT as *mut u64;
        // SAFETY: the frame was just mapped read-write at MAP_AT.
        let first = unsafe { word.read_volatile() };
        check!(first == 0, "a new frame holds {first:#x}");
        // SAFETY: as above.
        unsafe { word.add(511).write_volatile(0xabcd) };
        // SAFETY: as above.
        let back = unsafe { word.add(511).read_volatile() };
        check!(back == 0xabcd, "read back {back:#x}");
        if let Some(q) = ok!(address_space::query(&SYS, SLOT_SPACE, MAP_AT + 8), "query") {
            check!(q.mapped && q.rights == RW, "query: {} {:#x}", q.mapped, q.rights.0);
        }
        fails!(
            syscalls::map(&SYS, SLOT_SPACE, FRAME, MAP_AT, RW, 0, 1),
            AlreadyMapped,
            "mapping over a mapping"
        );
        ok!(syscalls::unmap(&SYS, SLOT_SPACE, MAP_AT, 1), "unmap");
        if let Some(q) = ok!(address_space::query(&SYS, SLOT_SPACE, MAP_AT), "query") {
            check!(!q.mapped, "still mapped after unmap");
        }
    }
    let rwx = RW.union(Rights::EXECUTE);
    fails!(syscalls::map(&SYS, SLOT_SPACE, FRAME, MAP_AT, rwx, 0, 1), InvalidArgument, "W+X");
    fails!(
        syscalls::map(&SYS, SLOT_SPACE, FRAME, MAP_AT, Rights::WRITE, 0, 1),
        InvalidArgument,
        "write without read"
    );
    fails!(
        syscalls::map(&SYS, SLOT_SPACE, FRAME, KERNEL, RW, 0, 1),
        InvalidArgument,
        "in the kernel"
    );
    fails!(syscalls::map(&SYS, SLOT_SPACE, FRAME, 0, RW, 0, 1), InvalidArgument, "at 0");
    fails!(
        syscalls::map(&SYS, SLOT_SPACE, FRAME, MAP_AT + 1, RW, 0, 1),
        InvalidArgument,
        "unaligned"
    );
    fails!(syscalls::map(&SYS, SLOT_SPACE, FRAME, MAP_AT, RW, 1, 1), OutOfRange, "past the frame");
    fails!(syscalls::map(&SYS, SLOT_SPACE, FRAME, MAP_AT, RW, 0, 2), OutOfRange, "too many pages");
    fails!(
        syscalls::map(&SYS, SLOT_MEMORY, FRAME, MAP_AT, RW, 0, 1),
        WrongKind,
        "memory as a space"
    );
    fails!(syscalls::unmap(&SYS, SLOT_SPACE, MAP_AT, 1), NotMapped, "unmapping nothing");
    fails!(
        address_space::query(&SYS, SLOT_SPACE, KERNEL),
        InvalidArgument,
        "querying the kernel's half"
    );
    reused_memory();
}

/// Memory that goes back to its memory object and is retyped again comes
/// back zero, past the kernel's first step of zeroing too: a frame of 128
/// pages, written all over, deleted, and made again from the same memory.
fn reused_memory() {
    const ORDER: u32 = 7;
    const PAGES: u64 = 1 << ORDER;
    let made = ok!(
        memory::retype(&SYS, SLOT_MEMORY, ObjectKind::Memory, ORDER, BIG, 1),
        "retype a memory object"
    );
    if made.is_none() {
        return;
    }
    for round in 0..2 {
        if ok!(memory::retype(&SYS, BIG, ObjectKind::Frame, ORDER, REUSED, 1), "retype").is_none()
            || ok!(syscalls::map(&SYS, SLOT_SPACE, REUSED, MAP_AT, RW, 0, PAGES), "map").is_none()
        {
            break;
        }
        let mut dirty = 0;
        for page in 0..PAGES {
            let words = (MAP_AT + page * 4096) as *mut u64;
            for at in [0, 511] {
                // SAFETY: the frame is mapped read-write at MAP_AT, PAGES
                // pages long.
                let word = unsafe { words.add(at) };
                // SAFETY: as above.
                dirty += u64::from(unsafe { word.read_volatile() } != 0);
                // SAFETY: as above.
                unsafe { word.write_volatile(0x5a5a_5a5a_5a5a_5a5a) };
            }
        }
        check!(dirty == 0, "round {round}: {dirty} words of a new frame are not zero");
        ok!(syscalls::unmap(&SYS, SLOT_SPACE, MAP_AT, PAGES), "unmap");
        ok!(capability::delete(&SYS, REUSED), "delete the frame");
    }
    ok!(capability::delete(&SYS, BIG), "delete the memory object");
}

fn derivation() {
    // A read-only copy maps read-only, and not read-write.
    ok!(capability::derive(&SYS, FRAME, COPY, Rights::READ, 0), "derive read-only");
    if let Some(info) = ok!(capability::info(&SYS, COPY), "info on the copy") {
        check!(info.rights == Rights::READ, "the copy has rights {:#x}", info.rights.0);
    }
    fails!(
        syscalls::map(&SYS, SLOT_SPACE, COPY, MAP_AT, RW, 0, 1),
        MissingRights,
        "RW by a RO copy"
    );
    ok!(syscalls::map(&SYS, SLOT_SPACE, COPY, MAP_AT, Rights::READ, 0, 1), "RO by the RO copy");
    if let Some(q) = ok!(address_space::query(&SYS, SLOT_SPACE, MAP_AT), "query") {
        check!(q.rights == Rights::READ, "the read-only mapping has {:#x}", q.rights.0);
    }
    ok!(syscalls::unmap(&SYS, SLOT_SPACE, MAP_AT, 1), "unmap");
    fails!(capability::derive(&SYS, FRAME, BIG, Rights::ALL, 5), BadBadge, "a badge on a frame");
    fails!(
        capability::derive(&SYS, FRAME, COPY, Rights::ALL, 0),
        Occupied,
        "derive into a full slot"
    );
    // Moving keeps the capability, deleting ends it.
    ok!(capability::move_to(&SYS, COPY, MOVED), "move");
    fails!(capability::info(&SYS, COPY), EmptySlot, "the slot moved from");
    ok!(capability::info(&SYS, MOVED), "the slot moved to");
    ok!(capability::delete(&SYS, MOVED), "delete");
    fails!(capability::info(&SYS, MOVED), EmptySlot, "a deleted slot");
    // Revoking deletes what was derived and keeps the original.
    ok!(capability::derive(&SYS, FRAME, CHILD, Rights::ALL, 0), "derive");
    ok!(capability::revoke(&SYS, FRAME), "revoke");
    fails!(capability::info(&SYS, CHILD), EmptySlot, "a revoked child");
    ok!(capability::info(&SYS, FRAME), "the revoked original");
    // Deleting a mapped frame's last capability unmaps it.
    ok!(syscalls::map(&SYS, SLOT_SPACE, FRAME, MAP_AT, RW, 0, 1), "map");
    ok!(capability::delete(&SYS, FRAME), "delete the mapped frame");
    if let Some(q) = ok!(address_space::query(&SYS, SLOT_SPACE, MAP_AT), "query") {
        check!(!q.mapped, "a deleted frame stays mapped");
    }
}

fn log() {
    let text = b"probe: a line with a tab\tand a bell\x07, cut past 256 bytes \
        .............................................................................\
        .............................................................................\
        ..............................................................................";
    ok!(syscalls::debug_log(&SYS, SLOT_LOG, text), "a long line");
    ok!(syscalls::debug_log(&SYS, SLOT_LOG, b"probe: bad UTF-8 \xff\xfe here"), "bad UTF-8");
    let mut args = [0u64; 10];
    args[0] = u64::from(SLOT_LOG);
    args[1] = KERNEL;
    args[2] = 16;
    fails!(raw(syscalls::DEBUG_LOG, args), BadAddress, "a log line from the kernel's memory");
    args[1] = 0x5000_0000;
    fails!(raw(syscalls::DEBUG_LOG, args), BadAddress, "a log line from unmapped memory");
    fails!(syscalls::debug_log(&SYS, SLOT_RANDOM, b"no"), WrongKind, "logging through random");
}

fn methods() {
    fails!(raw_call(SLOT_MEMORY, 0x0001_0099, &[]), NoSuchMethod, "an unknown memory method");
    fails!(raw_call(SLOT_MEMORY, 0x0005_0001, &[]), NoSuchMethod, "another interface's method");
    fails!(raw_call(SLOT_MEMORY, 0xffff_0099, &[]), NoSuchMethod, "an unknown capability method");
    fails!(
        raw_call(SLOT_MEMORY, memory::RETYPE, &[ObjectKind::Frame.raw().into(), 1 << 40, 90, 1]),
        InvalidArgument,
        "an order that is not a u32"
    );
    fails!(raw_call(SLOT_LOG, ring::INFO, &[]), NoSuchMethod, "ring info on the log");
}

fn rings() {
    fails!(
        syscalls::ring_setup(&SYS, SLOT_SPACE, RING_AT, 3, RING),
        InvalidArgument,
        "a ring of 3 entries"
    );
    fails!(
        syscalls::ring_setup(&SYS, SLOT_SPACE, RING_AT, 8192, RING),
        InvalidArgument,
        "a ring of 8192 entries"
    );
    fails!(
        syscalls::ring_setup(&SYS, SLOT_SPACE, KERNEL, 8, RING),
        InvalidArgument,
        "a ring in the kernel's half"
    );
    let Some(ring) = ok!(Ring::setup(SLOT_SPACE, RING_AT, 8, RING), "ring setup") else { return };
    fails!(
        syscalls::ring_setup(&SYS, SLOT_SPACE, RING_AT, 8, BIG),
        AlreadyMapped,
        "a ring over a ring"
    );
    if let Some(info) = ok!(ring::info(&SYS, RING), "ring info") {
        check!(info.entries == 8 && info.completions == 16, "ring info {info:?}");
    }
    // nop, random, timeout and msg_ring to itself.
    check!(ring.push(Opcode::Nop, 0, 1, [42, 0, 0, 0, 0, 0]), "push nop");
    check!(ring.push(Opcode::Random, 0, 2, [0; 6]), "push random");
    check!(ring.push(Opcode::Timeout, 0, 3, [1_000_000, 0, 0, 0, 0, 0]), "push timeout");
    check!(ring.push(Opcode::MsgRing, RING, 4, [99, 7, 8, 0, 0, 0]), "push msg_ring");
    if let Some((submitted, _)) = ok!(ring.enter(4, 5, 0), "enter") {
        check!(submitted == 4, "{submitted} entries taken");
    }
    let mut seen = [false; 5];
    while let Some(c) = ring.pop() {
        match c.user_data {
            1 => check!(c.status == 0 && c.values[0] == 42, "nop: {c:?}"),
            2 => check!(c.status == 0 && c.values != [0, 0], "random: {c:?}"),
            3 => check!(c.status == 0, "timeout: {c:?}"),
            4 => check!(c.status == 0, "msg_ring: {c:?}"),
            99 => check!(c.values == [7, 8], "posted: {c:?}"),
            other => check!(false, "a completion for {other}"),
        };
        if let Some(i) = [1, 2, 3, 4, 99].iter().position(|&u| u == c.user_data) {
            seen[i] = true;
        }
    }
    check!(seen == [true; 5], "completions seen: {seen:?}");
    // Bad entries complete with errors.
    check!(ring.push(Opcode::Nop, 0, 5, [0; 6]), "push");
    raw_opcode(&ring, 99);
    check!(ring.push(Opcode::Notify, SLOT_LOG, 7, [0; 6]), "push notify on the log");
    ok!(ring.enter(3, 3, 0), "enter");
    while let Some(c) = ring.pop() {
        let error = Kernel::from_status(Status(c.status));
        match c.user_data {
            5 => check!(c.status == 0, "nop: {c:?}"),
            6 => check!(error == Some(Kernel::InvalidArgument), "opcode 99: {c:?}"),
            7 => check!(error == Some(Kernel::WrongKind), "notify on the log: {c:?}"),
            other => check!(false, "a completion for {other}"),
        };
    }
    // A full completion queue: sixteen unread, the next entry waits.
    for round in 0..2 {
        for i in 0..8 {
            check!(ring.push(Opcode::Nop, 0, 100 + round * 8 + i, [0; 6]), "push {i}");
        }
        ok!(ring.enter(8, 0, 0), "enter round {round}");
    }
    check!(ring.push(Opcode::Nop, 0, 200, [0; 6]), "push past full");
    fails!(ring.enter(1, 0, 0), Busy, "entering a ring whose completions are full");
    let mut read = 0;
    while ring.pop().is_some() {
        read += 1;
    }
    check!(read == 16, "{read} completions, not 16");
    ok!(ring.enter(1, 1, 0), "enter after reading");
    check!(ring.pop().map(|c| c.user_data) == Some(200), "the entry that waited");
    // Nonsense in the shared indexes confuses only the program.
    let (head, tail) = ring.submission();
    ring.set_submission_tail(head.wrapping_add(1000));
    fails!(ring.enter(1, 0, 0), InvalidArgument, "a submission tail far ahead");
    ring.set_submission_tail(tail);
    ring.set_completion_head(ring.completion_tail().wrapping_add(5000));
    check!(ring.push(Opcode::Nop, 0, 300, [0; 6]), "push");
    fails!(ring.enter(1, 0, 0), Busy, "a completion head that makes no sense");
    ring.set_completion_head(ring.completion_tail());
    ok!(ring.enter(1, 1, 0), "enter with the head fixed");
    let _ = ring.pop();
    fails!(ring.enter(0, 17, 0), InvalidArgument, "waiting for more than fit");
    fails!(ring.enter(0, 1, 1_000_000), TimedOut, "waiting for nothing, with a timeout");
    ok!(capability::delete(&SYS, RING), "deleting the ring");
    if let Some(q) = ok!(address_space::query(&SYS, SLOT_SPACE, RING_AT), "query") {
        check!(!q.mapped, "a deleted ring stays mapped");
    }
}

/// Queues an entry with an opcode the kernel does not know, with user
/// data 6.
fn raw_opcode(ring: &Ring, opcode: u8) {
    // The library takes only known opcodes, so queue a nop and rewrite
    // its first byte where it lies, at the submission tail.
    check!(ring.push(Opcode::Nop, 0, 6, [0; 6]), "push");
    let (_, tail) = ring.submission();
    let entry = RING_AT + 4096 + u64::from(tail.wrapping_sub(1) & 7) * 64;
    // SAFETY: the entry lies in the submission array of the ring mapped
    // at RING_AT (its entries start one page in), which the kernel has
    // not taken yet.
    unsafe { (entry as *mut u8).write_volatile(opcode) };
}

/// Processes: its own, the objects it makes charged to it, and a child it
/// builds and ends before any thread of the child runs.
fn processes() {
    if let Some(info) = ok!(process::info(&SYS, SLOT_PROCESS), "info on its process") {
        check!(info.state == ProcessState::Running, "its process is {}", info.state.name());
        check!(info.threads == 1, "its process has {} threads", info.threads);
        check!(info.code == 0 && info.number > 0, "code {:#x}, number {}", info.code, info.number);
    }
    ok!(process::set_name(&SYS, SLOT_PROCESS, b"probe"), "naming its process");
    fails!(process::set_name(&SYS, SLOT_PROCESS, b"a name far too long"), InvalidArgument, "long");
    fails!(process::set_name(&SYS, SLOT_PROCESS, b"\xff"), InvalidArgument, "not UTF-8");
    fails!(
        raw_call(SLOT_PROCESS, process::SET_NAME, &[KERNEL, 4]).map(|_| ()),
        BadAddress,
        "a name in the kernel's half"
    );
    fails!(raw_call(SLOT_THREAD, process::INFO, &[]).map(|_| ()), NoSuchMethod, "on a thread");

    // Objects charged to its process.
    ok!(process::create(&SYS, SLOT_PROCESS, ObjectKind::Notification, 0, NOTE), "a notification");
    ok!(process::create(&SYS, SLOT_PROCESS, ObjectKind::Endpoint, 0, ENDPOINT), "an endpoint");
    if let Some(info) = ok!(capability::info(&SYS, ENDPOINT), "info on the endpoint") {
        check!(info.kind == ObjectKind::Endpoint, "made a {}", info.kind.name());
        check!(info.rights.contains(Rights::GRANT), "the endpoint lacks a right");
    }
    fails!(
        process::create(&SYS, SLOT_PROCESS, ObjectKind::Frame, 0, BIG),
        InvalidArgument,
        "frame"
    );
    fails!(
        process::create(&SYS, SLOT_PROCESS, ObjectKind::Endpoint, 1, BIG),
        InvalidArgument,
        "an endpoint with a size"
    );
    fails!(
        process::create(&SYS, SLOT_PROCESS, ObjectKind::Process, process::MIN_SLOTS - 1, BIG),
        InvalidArgument,
        "a process with too few slots"
    );
    fails!(
        process::create(&SYS, SLOT_PROCESS, ObjectKind::Notification, 0, NOTE),
        Occupied,
        "into a full slot"
    );

    // A child: built, then ended before it ever ran.
    if ok!(
        process::create(&SYS, SLOT_PROCESS, ObjectKind::Process, process::MIN_SLOTS, CHILD_PROCESS),
        "making a process"
    )
    .is_none()
    {
        return;
    }
    if let Some(info) = ok!(process::info(&SYS, CHILD_PROCESS), "info on the child") {
        check!(info.state == ProcessState::Created, "the child is {}", info.state.name());
        check!(info.threads == 0 && info.number > 0, "{} threads", info.threads);
    }
    ok!(process::address_space(&SYS, CHILD_PROCESS, CHILD_SPACE), "the child's address space");
    if let Some(info) = ok!(capability::info(&SYS, CHILD_SPACE), "info on its space") {
        check!(info.kind == ObjectKind::AddressSpace, "its space is a {}", info.kind.name());
    }
    ok!(process::share(&SYS, CHILD_PROCESS, NOTE, 3, Rights::SEND, 4), "sharing");
    fails!(process::share(&SYS, CHILD_PROCESS, NOTE, 3, Rights::SEND, 0), Occupied, "twice");
    fails!(
        process::share(&SYS, CHILD_PROCESS, SLOT_LOG, 5, Rights::WRITE, 9),
        BadBadge,
        "a badge on the log"
    );
    fails!(process::give(&SYS, CHILD_PROCESS, ENDPOINT, 99), NoSuchSlot, "past its slots");
    ok!(process::give(&SYS, CHILD_PROCESS, ENDPOINT, 4), "giving it the endpoint");
    fails!(capability::info(&SYS, ENDPOINT), EmptySlot, "the given endpoint is still here");
    ok!(capability::derive(&SYS, NOTE, BIG, Rights::SEND, 0), "a copy that cannot move");
    fails!(process::keep(&SYS, CHILD_PROCESS, BIG), MissingRights, "keeping it");
    ok!(capability::delete(&SYS, BIG), "deleting it");
    ok!(capability::derive(&SYS, NOTE, BIG, Rights::SEND.union(Rights::TRANSFER), 0), "a copy");
    ok!(process::keep(&SYS, CHILD_PROCESS, BIG), "keeping it");
    fails!(capability::info(&SYS, BIG), EmptySlot, "the kept copy is still here");
    fails!(process::keep(&SYS, CHILD_PROCESS, BIG), EmptySlot, "keeping nothing");
    fails!(
        process::start(&SYS, CHILD_PROCESS, KERNEL, MAP_AT, 0, 1),
        InvalidArgument,
        "a thread in the kernel's half"
    );
    // A process made for the child, which holds the only capability to
    // itself that can end it: it ends with the child.
    ok!(
        process::create(&SYS, CHILD_PROCESS, ObjectKind::Process, process::MIN_SLOTS, GRANDCHILD),
        "a process made for the child"
    );
    ok!(capability::derive(&SYS, GRANDCHILD, GRANDCHILD_VIEW, Rights::READ, 0), "a view of it");
    ok!(process::give(&SYS, GRANDCHILD, GRANDCHILD, 1), "giving it itself");
    ok!(process::notify_exit(&SYS, CHILD_PROCESS, NOTE), "asking to hear of its end");
    ok!(process::exit(&SYS, CHILD_PROCESS, 7), "ending it");
    if let Some(info) = ok!(process::info(&SYS, GRANDCHILD_VIEW), "info on the one made for it") {
        check!(info.state == ProcessState::Exited, "the one made for it is {}", info.state.name());
        check!(
            info.code == process::EXIT_STOPPED,
            "the one made for it ended with {:#x}",
            info.code
        );
    }
    if let Some(info) = ok!(process::info(&SYS, CHILD_PROCESS), "info on the ended child") {
        check!(info.state == ProcessState::Exited, "the child is {}", info.state.name());
        check!(info.code == 7, "the child's code is {:#x}", info.code);
    }
    if let Some(bits) =
        ok!(syscalls::wait(&SYS, NOTE, WaitFlags::POLL, 0).map(|r| r.bits), "its end signalled")
    {
        check!(bits == 1, "its end set bits {bits:#x}");
    }
    fails!(process::start(&SYS, CHILD_PROCESS, MAP_AT, MAP_AT, 0, 1), Gone, "starting it after");
    fails!(
        process::create(&SYS, CHILD_PROCESS, ObjectKind::Notification, 0, BIG),
        Gone,
        "charging it after"
    );
    fails!(process::give(&SYS, CHILD_PROCESS, NOTE, 5), Gone, "giving it something after");
    // Its address space outlives it, empty, while a capability holds it.
    if let Some(q) = ok!(address_space::query(&SYS, CHILD_SPACE, MAP_AT), "query its space") {
        check!(!q.mapped, "its space has a mapping");
    }
    ok!(process::notify_exit(&SYS, CHILD_PROCESS, NOTE), "asking again after");
    if let Some(bits) =
        ok!(syscalls::wait(&SYS, NOTE, WaitFlags::POLL, 0).map(|r| r.bits), "signalled at once")
    {
        check!(bits == 1, "set bits {bits:#x}");
    }
    for slot in [CHILD_SPACE, CHILD_PROCESS, NOTE, GRANDCHILD_VIEW] {
        ok!(capability::delete(&SYS, slot), "deleting slot {slot}");
    }
}
