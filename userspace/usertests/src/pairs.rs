// SPDX-FileCopyrightText: 2026 The OrthrOS contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Pairs of programs that talk to each other: over an endpoint, with
//! capabilities in their messages, through notifications, through a
//! shared-memory channel, and through each other's rings. Each pair has
//! the same endpoint, and two notifications: its own to wait on, and the
//! other program's to signal.

use core::sync::atomic::{AtomicU64, Ordering};

use libradix::abi::capability::{self, ObjectKind, Rights};
use libradix::abi::endpoint::{
    INFO_CAPS_MASK, INFO_CAPS_SHIFT, INFO_LABEL_MASK, INFO_WORDS_SHIFT, MessageFlags, NO_SLOT,
};
use libradix::abi::notification::WaitFlags;
use libradix::abi::ring::Opcode;
use libradix::abi::startup::{
    SLOT_FIRST_EXTRA, SLOT_MEMORY, SLOT_RANDOM, SLOT_SPACE, SLOT_THREAD, SLOT_WINDOW,
};
use libradix::abi::{memory, random, syscalls, thread};
use libradix::channel::{CHANNEL_BYTES, Channel};
use libradix::ring::Ring;
use libradix::time::{Nanos, cycles};
use libradix::{SYS, println, sys};

use crate::{Argument, checks};
use crate::{check, fails, ok};

const ENDPOINT: u32 = SLOT_FIRST_EXTRA;
const MINE: u32 = SLOT_FIRST_EXTRA + 1;
const THEIRS: u32 = SLOT_FIRST_EXTRA + 2;

/// Free slots.
const SLOT_A: u32 = 64;
const SLOT_B: u32 = 65;
const SLOT_C: u32 = 66;
const RING: u32 = 70;
const RING_SEND: u32 = 71;

const CHANNEL_AT: u64 = 0x4000_0000;
const RING_AT: u64 = 0x5000_0000;

/// The label that ends a server's loop.
const QUIT: u32 = 1;
const ECHO: u32 = 0x100;

/// An info word.
const fn info(label: u32, words: u64, caps: u64) -> u64 {
    label as u64 | (words << INFO_WORDS_SHIFT) | (caps << INFO_CAPS_SHIFT)
}

fn label(info: u64) -> u32 {
    (info & INFO_LABEL_MASK) as u32
}

fn caps(info: u64) -> u64 {
    (info >> INFO_CAPS_SHIFT) & INFO_CAPS_MASK
}

fn bench(argument: Argument, name: &str, start: u64, end: u64) {
    if argument.bench {
        let ns = Nanos::per(end - start, argument.rounds);
        println!("BENCH name={name}_{} ns={ns} iterations={}", argument.place(), argument.rounds);
    }
}

/// The endpoint server: answers each call with its words plus one, until
/// told to quit.
pub fn echo() -> u64 {
    let Some(first) = ok!(syscalls::recv(&SYS, ENDPOINT, 0), "echo: recv") else {
        return checks::failures();
    };
    let (mut info_word, mut words) = (first.info, first.words);
    loop {
        if label(info_word) == QUIT {
            ok!(syscalls::reply_recv(&SYS, NO_SLOT, info(QUIT, 0, 0), [0; 8]), "echo: last reply");
            break;
        }
        for word in &mut words {
            *word = word.wrapping_add(1);
        }
        match syscalls::reply_recv(&SYS, ENDPOINT, info_word, words) {
            Ok(next) => (info_word, words) = (next.info, next.words),
            Err(status) => {
                check!(false, "echo: reply_recv: {}", checks::Shown(status));
                break;
            }
        }
    }
    checks::failures()
}

/// The endpoint client: calls the echo server, checking each answer.
pub fn call(argument: Argument) -> u64 {
    let start = cycles();
    for i in 0..argument.rounds {
        let words = [i, i + 1, i + 2, i + 3, i + 4, i + 5, i + 6, i + 7];
        let Some(reply) = ok!(syscalls::cap_call(&SYS, ENDPOINT, info(ECHO, 8, 0), words), "call")
        else {
            break;
        };
        if reply.words[0] != i + 1 || reply.words[7] != i + 8 || reply.badge != 0 {
            check!(false, "round {i}: the reply is {:?}", reply.words);
            break;
        }
    }
    let end = cycles();
    ok!(syscalls::cap_call(&SYS, ENDPOINT, info(QUIT, 0, 0), [0; 8]), "call: quit");
    bench(argument, "endpoint_call", start, end);
    checks::failures()
}

/// Sends capabilities: copied, moved, refused for want of room or of the
/// grant right, and in a call whose reply carries one back.
pub fn cap_send() -> u64 {
    // A badged copy of its own notification, for the receiver to signal.
    ok!(capability::derive(&SYS, MINE, SLOT_A, Rights::ALL, 0x40), "derive a badged copy");
    ok!(syscalls::send(&SYS, ENDPOINT, info(0x200, 1, 1), word(SLOT_A)), "send a copy");
    let bits = ok!(syscalls::wait(&SYS, MINE, WaitFlags::NONE, 0), "wait");
    check!(bits.map(|b| b.bits) == Some(0x40), "the copy signalled {bits:?}");
    let moved = info(0x201, 1, 1) | MessageFlags::MOVE_CAPS.bits();
    ok!(syscalls::send(&SYS, ENDPOINT, moved, word(SLOT_A)), "send a move");
    fails!(capability::info(&SYS, SLOT_A), EmptySlot, "the slot a capability moved from");
    // The moved copy's badge, then the receiver's word that its window is
    // closed (through its unbadged capability: bit 0).
    let mut seen = 0;
    while seen != 0x41 {
        match ok!(syscalls::wait(&SYS, MINE, WaitFlags::NONE, 0), "wait") {
            Some(bits) => seen |= bits.bits,
            None => break,
        }
    }
    ok!(memory::retype(&SYS, SLOT_MEMORY, ObjectKind::Frame, 0, SLOT_B, 1), "retype a frame");
    fails!(
        syscalls::send(&SYS, ENDPOINT, info(0x202, 1, 1), word(SLOT_B)),
        NoRoom,
        "a capability for a receiver with no room"
    );
    ok!(syscalls::send(&SYS, ENDPOINT, info(0x203, 0, 0), [0; 8]), "a plain message");
    ok!(capability::derive(&SYS, ENDPOINT, SLOT_C, Rights::SEND, 0), "derive without grant");
    fails!(
        syscalls::send(&SYS, SLOT_C, info(0x203, 1, 1), word(SLOT_B)),
        MissingRights,
        "a capability through an endpoint capability without grant"
    );
    // A call with a frame; the reply moves it back.
    if let Some(reply) = ok!(
        syscalls::cap_call(&SYS, ENDPOINT, info(0x204, 1, 1), word(SLOT_B)),
        "a call with a cap"
    ) {
        check!(caps(reply.info) == 1, "the reply carries {} capabilities", caps(reply.info));
        let slot = reply.words[0] as u32;
        check!((SLOT_WINDOW..SLOT_WINDOW + 16).contains(&slot), "the reply's slot is {slot}");
        if let Some(i) = ok!(capability::info(&SYS, slot), "info on the returned frame") {
            check!(i.kind == ObjectKind::Frame, "a {} came back", i.kind.name());
        }
    }
    ok!(syscalls::send(&SYS, ENDPOINT, info(QUIT, 0, 0), [0; 8]), "quit");
    checks::failures()
}

fn word(slot: u32) -> [u64; 8] {
    let mut words = [0; 8];
    words[0] = u64::from(slot);
    words
}

/// Receives the capabilities `cap_send` sends.
pub fn cap_receive() -> u64 {
    let Some(first) = ok!(syscalls::recv(&SYS, ENDPOINT, 0), "recv") else {
        return checks::failures();
    };
    let w1 = first.words[0] as u32;
    check!(label(first.info) == 0x200 && caps(first.info) == 1, "first message {:#x}", first.info);
    check!(w1 == SLOT_WINDOW, "the copy arrived in slot {w1}");
    if let Some(i) = ok!(capability::info(&SYS, w1), "info on the copy") {
        check!(i.kind == ObjectKind::Notification && i.badge == 0x40, "the copy: {i:?}");
    }
    ok!(syscalls::notify(&SYS, w1), "notify through the copy");
    if let Some(second) = ok!(syscalls::recv(&SYS, ENDPOINT, 0), "recv") {
        let w2 = second.words[0] as u32;
        check!(w2 == SLOT_WINDOW + 1, "the moved capability arrived in slot {w2}");
        ok!(syscalls::notify(&SYS, w2), "notify through the moved capability");
    }
    // No room: the next capability is refused, plain messages still come.
    ok!(thread::set_receive_window(&SYS, SLOT_THREAD, SLOT_WINDOW, 0), "close the window");
    ok!(syscalls::notify(&SYS, THEIRS), "say so");
    if let Some(plain) = ok!(syscalls::recv(&SYS, ENDPOINT, 0), "recv") {
        check!(label(plain.info) == 0x203, "after the refused message came {:#x}", plain.info);
    }
    ok!(thread::set_receive_window(&SYS, SLOT_THREAD, SLOT_WINDOW, 16), "open the window");
    if let Some(call) = ok!(syscalls::recv(&SYS, ENDPOINT, 0), "recv the call") {
        let frame = call.words[0] as u32;
        check!(caps(call.info) == 1, "the call carries {} capabilities", caps(call.info));
        if let Some(i) = ok!(memory::frame::info(&SYS, frame), "frame info") {
            check!(i.pages == 1, "the frame has {} pages", i.pages);
        }
        let reply = info(0x204, 1, 1) | MessageFlags::MOVE_CAPS.bits();
        ok!(syscalls::reply_recv(&SYS, NO_SLOT, reply, word(frame)), "reply with the frame");
        fails!(capability::info(&SYS, frame), EmptySlot, "the frame moved back");
    }
    if let Some(quit) = ok!(syscalls::recv(&SYS, ENDPOINT, 0), "recv quit") {
        check!(label(quit.info) == QUIT, "the last message is {:#x}", quit.info);
    }
    checks::failures()
}

/// Signals the other program's notification and waits on its own.
pub fn notify_ping(argument: Argument) -> u64 {
    let start = cycles();
    for i in 0..argument.rounds {
        ok!(syscalls::notify(&SYS, THEIRS), "notify");
        match ok!(syscalls::wait(&SYS, MINE, WaitFlags::NONE, 0), "wait") {
            Some(bits) if bits.bits == 1 => {}
            other => {
                check!(false, "round {i}: woken with {other:?}");
                break;
            }
        }
    }
    let end = cycles();
    fails!(syscalls::wait(&SYS, MINE, WaitFlags::POLL, 0), WouldBlock, "polling nothing");
    fails!(syscalls::wait(&SYS, MINE, WaitFlags::NONE, 1_000_000), TimedOut, "waiting 1 ms");
    bench(argument, "notification_pingpong", start, end);
    checks::failures()
}

/// Answers each of `notify_ping`'s signals.
pub fn notify_pong(argument: Argument) -> u64 {
    for _ in 0..argument.rounds {
        if ok!(syscalls::wait(&SYS, MINE, WaitFlags::NONE, 0), "wait").is_none() {
            break;
        }
        ok!(syscalls::notify(&SYS, THEIRS), "notify");
    }
    checks::failures()
}

/// The channel's first end: makes the frame, passes it on, and sends.
pub fn channel_a(argument: Argument) -> u64 {
    let rw = Rights::READ.union(Rights::WRITE);
    ok!(memory::retype(&SYS, SLOT_MEMORY, ObjectKind::Frame, 0, SLOT_A, 1), "retype");
    ok!(syscalls::map(&SYS, SLOT_SPACE, SLOT_A, CHANNEL_AT, rw, 0, 1), "map");
    ok!(syscalls::send(&SYS, ENDPOINT, info(0x300, 1, 1), word(SLOT_A)), "pass the frame");
    // SAFETY: the frame is mapped read-write at CHANNEL_AT for the whole
    // program, is one page (CHANNEL_BYTES), new and so zeroed, and only
    // the other end shares it.
    let channel = unsafe { Channel::new(CHANNEL_AT, 0, MINE, THEIRS) };
    const _: () = assert!(CHANNEL_BYTES == 4096);
    let start = cycles();
    for i in 0..argument.rounds {
        let message = [i, 1, 2, 3, 4, 5, 6, 7];
        if ok!(channel.send(&message), "channel send").is_none() {
            break;
        }
        match ok!(channel.recv(), "channel recv") {
            Some(back) if back[0] == i + 1 && back[7] == 8 => {}
            other => {
                check!(false, "round {i}: got {other:?}");
                break;
            }
        }
    }
    let end = cycles();
    ok!(channel.send(&[u64::MAX; 8]), "channel quit");
    bench(argument, "channel_roundtrip", start, end);
    checks::failures()
}

/// The channel's other end: maps the frame it is given and echoes.
pub fn channel_b() -> u64 {
    let rw = Rights::READ.union(Rights::WRITE);
    let Some(passed) = ok!(syscalls::recv(&SYS, ENDPOINT, 0), "recv the frame") else {
        return checks::failures();
    };
    let frame = passed.words[0] as u32;
    if ok!(syscalls::map(&SYS, SLOT_SPACE, frame, CHANNEL_AT, rw, 0, 1), "map").is_none() {
        return checks::failures();
    }
    // SAFETY: as in `channel_a`; this end maps the same frame.
    let channel = unsafe { Channel::new(CHANNEL_AT, 1, MINE, THEIRS) };
    while let Some(mut message) = ok!(channel.recv(), "channel recv") {
        if message[0] == u64::MAX {
            break;
        }
        for word in &mut message {
            *word += 1;
        }
        if ok!(channel.send(&message), "channel send").is_none() {
            break;
        }
    }
    checks::failures()
}

/// User data of a ring entry's own completion, and of a message posted
/// to the other ring.
const OWN: u64 = 1 << 40;
const MESSAGE: u64 = 2 << 40;
const RING_QUIT: u64 = 3 << 40;

/// Sets up this program's ring and swaps send-only capabilities to it
/// with the other program, `first` sending first. Returns the ring and
/// the slot of the other's.
fn rings(first: bool) -> Option<(Ring, u32)> {
    let ring = ok!(Ring::setup(SLOT_SPACE, RING_AT, 8, RING), "ring setup")?;
    let send_only = Rights::SEND.union(Rights::TRANSFER);
    ok!(capability::derive(&SYS, RING, RING_SEND, send_only, 0), "derive send-only")?;
    // Moved, not copied: a send-only capability has no right to copy.
    let moved = info(0x400, 1, 1) | MessageFlags::MOVE_CAPS.bits();
    let give = || syscalls::send(&SYS, ENDPOINT, moved, word(RING_SEND));
    let take = || syscalls::recv(&SYS, ENDPOINT, 0).map(|r| r.words[0] as u32);
    let theirs = if first {
        ok!(give(), "give the ring")?;
        ok!(take(), "take the other ring")?
    } else {
        let theirs = ok!(take(), "take the other ring")?;
        ok!(give(), "give the ring")?;
        theirs
    };
    if let Some(i) = ok!(capability::info(&SYS, theirs), "info on the other ring") {
        check!(i.kind == ObjectKind::Ring && i.rights.contains(Rights::SEND), "got {i:?}");
        check!(!i.rights.contains(Rights::CONTROL), "a send-only ring capability can control");
    }
    fails!(
        syscalls::ring_enter(&SYS, theirs, 0, 0, 0),
        MissingRights,
        "entering another program's ring"
    );
    Some((ring, theirs))
}

/// Posts a message to the other program's ring and waits for the answer
/// in its own.
pub fn ring_a(argument: Argument) -> u64 {
    let Some((ring, theirs)) = rings(true) else { return checks::failures() };
    let start = cycles();
    'rounds: for i in 0..argument.rounds {
        check!(ring.push(Opcode::MsgRing, theirs, OWN | i, [MESSAGE | i, i, 0, 0, 0, 0]), "push");
        let (mut own, mut answer) = (false, false);
        let mut submit = 1;
        while !(own && answer) {
            if ok!(ring.enter(submit, 1, 0), "enter").is_none() {
                break 'rounds;
            }
            submit = 0;
            while let Some(c) = ring.pop() {
                if c.user_data == OWN | i && c.status == 0 {
                    own = true;
                } else if c.user_data == MESSAGE | i && c.values[0] == i + 1 {
                    answer = true;
                } else {
                    check!(false, "round {i}: completion {c:?}");
                    break 'rounds;
                }
            }
        }
    }
    let end = cycles();
    check!(ring.push(Opcode::MsgRing, theirs, OWN, [RING_QUIT, 0, 0, 0, 0, 0]), "push quit");
    ok!(ring.enter(1, 1, 0), "enter");
    bench(argument, "ring_roundtrip", start, end);
    checks::failures()
}

/// Answers each message posted to its ring with one posted back.
pub fn ring_b() -> u64 {
    let Some((ring, theirs)) = rings(false) else { return checks::failures() };
    let mut submit = 0;
    'serve: loop {
        if ok!(ring.enter(submit, 1, 0), "enter").is_none() {
            break;
        }
        submit = 0;
        while let Some(c) = ring.pop() {
            match c.user_data & !0xff_ffff_ffff {
                RING_QUIT => break 'serve,
                MESSAGE => {
                    let i = c.values[0];
                    let args = [c.user_data, i + 1, 0, 0, 0, 0];
                    check!(ring.push(Opcode::MsgRing, theirs, OWN | i, args), "push");
                    submit += 1;
                }
                OWN => {
                    check!(c.status == 0, "posting an answer: {c:?}");
                }
                _ => {
                    check!(false, "completion {c:?}");
                }
            }
        }
    }
    checks::failures()
}

/// Waits until the other program of the pair is here too: each signals
/// the other's notification, then waits on its own.
fn meet() -> bool {
    ok!(syscalls::notify(&SYS, THEIRS), "notify").is_some()
        && ok!(syscalls::wait(&SYS, MINE, WaitFlags::NONE, 0), "wait").is_some()
}

/// Two programs on one CPU, each yielding to the other: every yield is a
/// switch to the other program, in another address space.
pub fn yield_a(argument: Argument) -> u64 {
    if !meet() {
        return checks::failures();
    }
    let start = cycles();
    for _ in 0..argument.rounds {
        if ok!(syscalls::r#yield(&SYS), "yield").is_none() {
            break;
        }
    }
    let switches = 2 * argument.rounds;
    if argument.bench {
        let ns = Nanos::per(cycles() - start, switches);
        println!("BENCH name=context_switch ns={ns} iterations={switches}");
    }
    checks::failures()
}

/// The other program of [`yield_a`].
pub fn yield_b(argument: Argument) -> u64 {
    if meet() {
        for _ in 0..argument.rounds {
            if ok!(syscalls::r#yield(&SYS), "yield").is_none() {
                break;
            }
        }
    }
    checks::failures()
}

/// Where the shootdown pair maps the page they share, and where the
/// first maps a page into the second's address space and out again.
const FLAG_AT: u64 = 0x6000_0000;
const SHOOT_AT: u64 = 0x6100_0000;

/// Words of the shared page: the second program is spinning, and should
/// stop.
const READY: u64 = 0;
const STOP: u64 = 8;

/// The slots of the shared page, and of the page mapped in and out.
const SLOT_FLAG: u32 = SLOT_A;
const SLOT_SHOT: u32 = SLOT_B;

fn flag(word: u64) -> &'static AtomicU64 {
    // SAFETY: both programs keep the shared page mapped read-write at
    // FLAG_AT until they exit, and its words are only used atomically.
    unsafe { &*((FLAG_AT + word) as *const AtomicU64) }
}

/// A TLB shootdown: the first program, on one CPU, maps a page into the
/// second's address space and unmaps it again, while the second runs on
/// another CPU; every unmap must reach that CPU's TLB.
pub fn shoot_a(argument: Argument) -> u64 {
    let rw = Rights::READ.union(Rights::WRITE);
    ok!(memory::retype(&SYS, SLOT_MEMORY, ObjectKind::Frame, 0, SLOT_FLAG, 1), "retype");
    ok!(memory::retype(&SYS, SLOT_MEMORY, ObjectKind::Frame, 0, SLOT_SHOT, 1), "retype");
    ok!(syscalls::map(&SYS, SLOT_SPACE, SLOT_FLAG, FLAG_AT, rw, 0, 1), "map");
    ok!(syscalls::send(&SYS, ENDPOINT, info(0x400, 1, 1), word(SLOT_FLAG)), "pass the page");
    let Some(theirs) = ok!(syscalls::recv(&SYS, ENDPOINT, 0), "receive the space") else {
        return checks::failures();
    };
    let space = theirs.words[0] as u32;
    // The same system calls on this program's own space, which no other
    // CPU runs: the shootdown is the difference.
    let start = cycles();
    for _ in 0..argument.rounds {
        let local = syscalls::map(&SYS, SLOT_SPACE, SLOT_SHOT, SHOOT_AT, rw, 0, 1)
            .and_then(|()| syscalls::unmap(&SYS, SLOT_SPACE, SHOOT_AT, 1));
        if ok!(local, "map and unmap in this space").is_none() {
            break;
        }
    }
    let end = cycles();
    if argument.bench {
        let ns = Nanos::per(end - start, argument.rounds);
        println!("BENCH name=tlb_local ns={ns} iterations={}", argument.rounds);
    }
    while flag(READY).load(Ordering::Acquire) == 0 {
        core::hint::spin_loop();
    }
    let start = cycles();
    for _ in 0..argument.rounds {
        let shot = syscalls::map(&SYS, space, SLOT_SHOT, SHOOT_AT, rw, 0, 1)
            .and_then(|()| syscalls::unmap(&SYS, space, SHOOT_AT, 1));
        if ok!(shot, "map and unmap in the other space").is_none() {
            break;
        }
    }
    let end = cycles();
    flag(STOP).store(1, Ordering::Release);
    if argument.bench {
        let ns = Nanos::per(end - start, argument.rounds);
        println!("BENCH name=tlb_shootdown ns={ns} iterations={}", argument.rounds);
    }
    checks::failures()
}

/// The other program of [`shoot_a`]: maps the shared page, hands over its
/// address space, and spins in user mode until told to stop.
pub fn shoot_b() -> u64 {
    let rw = Rights::READ.union(Rights::WRITE);
    let Some(passed) = ok!(syscalls::recv(&SYS, ENDPOINT, 0), "receive the page") else {
        return checks::failures();
    };
    let page = passed.words[0] as u32;
    if ok!(syscalls::map(&SYS, SLOT_SPACE, page, FLAG_AT, rw, 0, 1), "map").is_none() {
        return checks::failures();
    }
    ok!(syscalls::send(&SYS, ENDPOINT, info(0x401, 1, 1), word(SLOT_SPACE)), "pass the space");
    flag(READY).store(1, Ordering::Release);
    while flag(STOP).load(Ordering::Acquire) == 0 {
        core::hint::spin_loop();
    }
    checks::failures()
}

/// The cheapest system calls on both CPUs at once: each program of the
/// pair runs them; the first reports.
pub fn parallel(argument: Argument, report: bool) -> u64 {
    let rounds = argument.rounds;
    let tests: [(&str, u64, &dyn Fn() -> bool); 3] = [
        ("syscall_null_parallel", rounds, &|| sys::syscall(99, [0; 10]).0 != 0),
        ("cap_info_parallel", rounds, &|| capability::info(&SYS, SLOT_RANDOM).is_ok()),
        ("random_words_parallel", rounds / 4, &|| random::words(&SYS, SLOT_RANDOM).is_ok()),
    ];
    for (name, count, body) in tests {
        if !meet() {
            break;
        }
        let start = cycles();
        for _ in 0..count {
            if !body() {
                checks::that(false, format_args!("{name} failed"));
                return checks::failures();
            }
        }
        if report && argument.bench {
            let ns = Nanos::per(cycles() - start, count);
            println!("BENCH name={name} ns={ns} iterations={count}");
        }
    }
    checks::failures()
}
