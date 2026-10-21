// SPDX-FileCopyrightText: 2026 The OrthrOS contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! The fuzzer: random system calls, methods and ring entries, with
//! arguments drawn mostly from values at the edges of what the kernel
//! accepts, so that the kernel's `fuzz` self-test can show that nothing a
//! program passes crashes the kernel, trips the lock checker or leaves an
//! object behind.
//!
//! The kernel starts three of these together and gives them two
//! endpoints, two notifications and a memory object to share, so calls
//! that wait can meet a partner and capabilities travel between them. A
//! fourth program, the [`janitor`], keeps letting go of fuzzers that wait
//! for a partner, so that the three cannot all end up waiting for each
//! other; the kernel stops it, and any fuzzer still waiting, at the end.
//! Each fuzzer draws from the seed its argument gives, so a run can be
//! repeated.
//!
//! The kernel writes to memory the arguments name, so the program only
//! names memory it never uses itself: its arena (where it maps frames),
//! pages that are not mapped, read-only pages, and addresses outside the
//! user half. It never unmaps its own image, stack, start-up page or
//! ring, never moves, deletes or revokes the capabilities it runs on (its
//! thread, address space, memory, random number generator, log and ring),
//! and never ends its own thread: everything else is fair game. After
//! every call it checks what radix-syscalls(2) promises of a failed one:
//! a kernel error, and 0 in every result register. It also checks that
//! the error is one the call's documentation lists (its `raises` list,
//! and for a method that of `cap_call` too). It exits with the number of
//! calls that broke a promise or failed with an error their documentation
//! does not list.

use core::sync::atomic::{AtomicU32, Ordering};

use libradix::abi::capability as cap;
use libradix::abi::endpoint::{
    INFO_CAPS_SHIFT, INFO_WORDS_SHIFT, MessageFlags, NO_SLOT, RESERVED_LABELS,
};
use libradix::abi::errors::Kernel;
use libradix::abi::notification::WaitFlags;
use libradix::abi::ring::{ENTRIES_MAX, RingHeader, RingIndexes, Sqe};
use libradix::abi::startup::{
    SLOT_FIRST_EXTRA, SLOT_LOG, SLOT_MEMORY, SLOT_RANDOM, SLOT_SPACE, SLOT_THREAD, SLOT_WINDOW,
    SLOTS, STARTUP_ADDRESS, WINDOW_SLOTS,
};
use libradix::abi::{Status, address_space, memory, raises, random, ring, syscalls, thread};
use libradix::{println, startup, sys};

/// Calls each program makes, and rounds of letting its partners go at
/// the end.
const CALLS: u32 = 4000;
const DRAIN_ROUNDS: u32 = 16;

/// Slots the program keeps its own capabilities in, which it may delete,
/// move and revoke: the window received capabilities land in, and these.
const SCRATCH: u32 = 64;
const SCRATCH_SLOTS: u32 = 32;

/// The ring whose entries it writes, in a slot and at an address it never
/// gives up.
const RING_SLOT: u32 = 100;
const RING_AT: u64 = 0x3000_0000;
const RING_ENTRIES: u32 = 8;
/// The longest a ring_enter waits for completions, in nanoseconds.
const MAX_RING_WAIT_NS: u64 = 2_000_000;

/// Where it maps frames and makes rings: pages it never touches itself.
const ARENA: u64 = 0x4000_0000;
const ARENA_PAGES: u64 = 64;
/// A page nothing is ever mapped at: far enough past the arena that no
/// mapping or ring from there reaches it.
const UNMAPPED: u64 = 0x5000_0000;

const PAGE: u64 = address_space::PAGE_SIZE;
const USER_TOP: u64 = address_space::USER_TOP;
/// The first address of the kernel's half.
const KERNEL: u64 = 0xffff_8000_0000_0000;

/// The methods it calls, by label.
const LABELS: [u32; 17] = [
    memory::RETYPE,
    memory::INFO,
    memory::frame::INFO,
    address_space::QUERY,
    thread::SET_TLS,
    thread::SET_RECEIVE_WINDOW,
    thread::INFO,
    random::READ,
    random::WORDS,
    ring::INFO,
    cap::INFO,
    cap::DERIVE,
    cap::MOVE_TO,
    cap::DELETE,
    cap::REVOKE,
    cap::SEAL,
    cap::UNSEAL,
];

/// xorshift64*, seeded through splitmix64 (Vigna).
struct Rng(u64);

impl Rng {
    fn new(seed: u64) -> Rng {
        let mut z = seed.wrapping_add(0x9e37_79b9_7f4a_7c15);
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        Rng((z ^ (z >> 31)) | 1)
    }

    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_f491_4f6c_dd1d)
    }

    /// A number below `n`, which is not 0.
    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }

    /// True one time in `n`.
    fn one_in(&mut self, n: u64) -> bool {
        self.below(n) == 0
    }

    fn pick<T: Copy>(&mut self, items: &[T]) -> T {
        items[self.below(items.len() as u64) as usize]
    }
}

/// What the fuzzer knows about itself.
struct Fuzzer {
    rng: Rng,
    /// Slots of the objects the kernel passed it.
    extra: u32,
    /// Where its ring's submission entries start, as the kernel first
    /// wrote it: the header is the program's to scribble on.
    sqes: u32,
    /// Calls made, failed, that broke a promise, and that failed with an
    /// error their documentation does not list.
    calls: u32,
    failed: u32,
    broken: u32,
    undocumented: u32,
    /// The undocumented errors reported so far, as (call, label, code), so
    /// that each is reported once.
    reported: [(u64, u32, u16); REPORTED],
}

/// How many different undocumented errors a fuzzer reports.
const REPORTED: usize = 16;

impl Fuzzer {
    fn new(seed: u64, extra: u32) -> Fuzzer {
        Fuzzer {
            rng: Rng::new(seed),
            extra,
            sqes: 0,
            calls: 0,
            failed: 0,
            broken: 0,
            undocumented: 0,
            reported: [(0, 0, 0); REPORTED],
        }
    }

    /// Any slot: one of its own, one it may give up, or one that is not a
    /// slot at all (wider than 32 bits, or past the space's end).
    fn slot(&mut self) -> u64 {
        match self.rng.below(16) {
            0..=3 => u64::from(self.rng.pick(&[
                SLOT_THREAD,
                SLOT_SPACE,
                SLOT_MEMORY,
                SLOT_RANDOM,
                SLOT_LOG,
                RING_SLOT,
            ])),
            4..=10 => self.spare(),
            11 => self.rng.pick(&[0, 6, u64::from(SLOTS) - 1, u64::from(SLOTS)]),
            12 => u64::from(NO_SLOT),
            13 => self.rng.below(u64::from(SLOTS)),
            14 => self.rng.next() & 0xffff_ffff,
            _ => self.rng.next(),
        }
    }

    /// A slot it may delete, move or revoke: the objects passed to it, the
    /// receive window and its scratch slots.
    fn spare(&mut self) -> u64 {
        let slot = match self.rng.below(8) {
            0 | 1 => SLOT_FIRST_EXTRA + self.rng.below(u64::from(self.extra.max(1))) as u32,
            2 => SLOT_WINDOW + self.rng.below(u64::from(WINDOW_SLOTS)) as u32,
            _ => SCRATCH + self.rng.below(u64::from(SCRATCH_SLOTS)) as u32,
        };
        u64::from(slot)
    }

    /// An address the kernel may read or write: never memory the program
    /// uses itself.
    fn address(&mut self) -> u64 {
        match self.rng.below(12) {
            0..=3 => ARENA + self.rng.below(ARENA_PAGES) * PAGE,
            4 => ARENA + self.rng.below(ARENA_PAGES * PAGE),
            5 => self.rng.pick(&[0, 8, PAGE - 1, STARTUP_ADDRESS, STARTUP_ADDRESS + 4000]),
            // Its own code, which is read-only.
            6 => run as *const () as u64 + self.rng.below(256),
            7 => self.rng.pick(&[USER_TOP - 8, USER_TOP - PAGE, USER_TOP, USER_TOP + PAGE]),
            8 => self.rng.pick(&[0x0000_8000_0000_0000, KERNEL, KERNEL + PAGE, u64::MAX - 7]),
            9 => self.rng.pick(&[u64::MAX, 1 << 63, 0x1234_5678_9abc_def0]),
            _ => self.rng.next(),
        }
    }

    /// An address where nothing is mapped, or can be.
    fn unmapped(&mut self) -> u64 {
        match self.rng.below(4) {
            0 => UNMAPPED + self.rng.below(PAGE),
            1 => self.rng.pick(&[0, 8, USER_TOP - 1, USER_TOP]),
            2 => self.rng.pick(&[0x0000_8000_0000_0000, KERNEL, u64::MAX - 7, u64::MAX]),
            _ => KERNEL | self.rng.next(),
        }
    }

    /// A page-aligned address to map or unmap at: mostly in the arena,
    /// never on the program's own pages.
    fn page_address(&mut self) -> u64 {
        match self.rng.below(10) {
            0..=6 => ARENA + self.rng.below(ARENA_PAGES) * PAGE,
            7 => self.rng.pick(&[0, PAGE, USER_TOP - PAGE, USER_TOP, KERNEL, !(PAGE - 1)]),
            8 => ARENA + 1 + self.rng.below(PAGE - 1),
            _ => self.rng.next() & !(PAGE - 1) & !(1 << 63),
        }
    }

    /// A length in bytes.
    fn length(&mut self) -> u64 {
        match self.rng.below(10) {
            0..=4 => self.rng.pick(&[0, 1, 7, 8, 63, 64, 255, 256, 257, 4095, 4096, 4097]),
            5 => self.rng.pick(&[random::READ_MAX, random::READ_MAX + 1, 1 << 20]),
            6 => self.rng.pick(&[u32::MAX.into(), u64::MAX, 1 << 63]),
            _ => self.rng.below(512),
        }
    }

    /// A number of pages.
    fn pages(&mut self) -> u64 {
        match self.rng.below(8) {
            0..=4 => self.rng.pick(&[1, 1, 2, 3, 4, 8, 16]),
            5 => self.rng.pick(&[0, ARENA_PAGES, 512, 1 << 20]),
            6 => self.rng.pick(&[1 << 52, u64::MAX, u64::MAX / PAGE + 1]),
            _ => self.rng.below(64),
        }
    }

    /// Rights: mostly the ones there are, sometimes bits that mean nothing.
    fn rights(&mut self) -> u64 {
        if self.rng.one_in(8) { self.rng.next() } else { self.rng.below(1 << 10) }
    }

    /// A badge.
    fn badge(&mut self) -> u64 {
        self.rng.pick(&[0, 0, 1, 2, 0x80, u64::MAX, self.rng.0 >> 8])
    }

    /// A timeout in nanoseconds: rarely 0 (none), which may wait for good.
    fn timeout(&mut self) -> u64 {
        if self.rng.one_in(64) {
            return 0;
        }
        self.rng.pick(&[1, 1_000, 100_000, 1_000_000, MAX_RING_WAIT_NS])
    }

    /// A word of any kind.
    fn word(&mut self) -> u64 {
        match self.rng.below(5) {
            0 => self.slot(),
            1 => self.address(),
            2 => self.length(),
            3 => self.rng.below(32),
            _ => self.rng.next(),
        }
    }

    /// An info word: a label, a word count, a capability count and flags,
    /// sometimes past their limits or with bits that mean nothing.
    fn info(&mut self, label: u32, no_block: bool) -> u64 {
        let words = if self.rng.one_in(16) { self.rng.below(16) } else { self.rng.below(9) };
        let caps = match self.rng.below(8) {
            0..=4 => 0,
            5 | 6 => self.rng.below(words.min(4) + 1),
            _ => self.rng.below(8),
        };
        let mut info = u64::from(label) | (words << INFO_WORDS_SHIFT) | (caps << INFO_CAPS_SHIFT);
        if no_block {
            info |= MessageFlags::NO_BLOCK.0;
        }
        if self.rng.one_in(4) {
            info |= MessageFlags::MOVE_CAPS.0;
        }
        if self.rng.one_in(32) {
            info |= 1 << (42 + self.rng.below(22));
        }
        info
    }

    /// The info word of a message to another program: a label below the
    /// methods', and mostly without waiting.
    fn message_info(&mut self) -> u64 {
        let label = self.rng.below(0x1_0000) as u32;
        let no_block = !self.rng.one_in(16);
        self.info(label, no_block)
    }

    /// A message's data words: capabilities to pass first, as slots. Moved
    /// ones only come from slots it may give up.
    fn message(&mut self, info: u64) -> [u64; 8] {
        let moving = info & MessageFlags::MOVE_CAPS.0 != 0;
        core::array::from_fn(|i| {
            if i < 4 { if moving { self.spare() } else { self.slot() } } else { self.word() }
        })
    }

    /// A label for `cap_call`: a method, most of the time.
    fn label(&mut self) -> u32 {
        match self.rng.below(10) {
            0..=6 => self.rng.pick(&LABELS),
            7 => (RESERVED_LABELS as u32) | self.rng.below(16) as u32,
            8 => self.rng.below(16) as u32 | ((self.rng.below(12) as u32) << 16),
            _ => self.rng.next() as u32,
        }
    }

    /// The arguments of method `label`, in its words' order.
    fn method_args(&mut self, label: u32) -> [u64; 8] {
        let mut args = [0u64; 8];
        match label {
            memory::RETYPE => {
                args[0] = self.rng.pick(&[0, 1, 1, 1, 2, 7, 12, 255]);
                args[1] = self.rng.pick(&[0, 0, 0, 1, 2, 4, 9, 20, 21, 64]);
                args[2] = self.spare();
                args[3] = self.rng.pick(&[1, 1, 2, 4, 0, 1024, 1025, u32::MAX.into(), 1 << 40]);
            }
            address_space::QUERY | thread::SET_TLS => args[0] = self.address(),
            thread::SET_RECEIVE_WINDOW => {
                args[0] = self.rng.pick(&[SLOT_WINDOW, SCRATCH, 0, SLOTS - 1, u32::MAX]).into();
                args[1] = self.rng.pick(&[WINDOW_SLOTS, 1, 0, SCRATCH_SLOTS, 4096, 4097]).into();
            }
            random::READ => {
                args[0] = self.address();
                args[1] = self.length();
            }
            cap::DERIVE => {
                args[0] = self.slot();
                args[1] = self.rights();
                args[2] = self.badge();
            }
            cap::MOVE_TO => args[0] = self.slot(),
            cap::SEAL | cap::UNSEAL => {
                args[0] = self.slot();
                args[1] = self.slot();
            }
            _ => {
                for word in &mut args {
                    *word = self.word();
                }
            }
        }
        // Now and then, one word of any kind in place of a good one.
        if self.rng.one_in(8) {
            args[self.rng.below(8) as usize] = self.word();
        }
        args
    }

    /// One `cap_call` of a method on a capability. Methods that give up the
    /// capability only go to slots it may give up, and it never ends its
    /// own thread.
    fn method(&mut self) -> (u64, [u64; 10]) {
        let label = self.label();
        if label == thread::EXIT {
            return (syscalls::YIELD, [0; 10]);
        }
        let gives_up = matches!(label, cap::MOVE_TO | cap::DELETE | cap::REVOKE);
        let target = if gives_up { self.spare() } else { self.slot() };
        let mut words = self.rng.below(9);
        if self.rng.one_in(16) {
            words = self.rng.below(16);
        }
        let mut info = u64::from(label) | (words << INFO_WORDS_SHIFT);
        if self.rng.one_in(16) {
            info |= self.info(label, true) & !u64::from(u32::MAX);
        }
        let mut args = [0u64; 10];
        args[0] = target;
        args[1] = info;
        args[2..].copy_from_slice(&self.method_args(label));
        (syscalls::CAP_CALL, args)
    }

    /// An endpoint slot: one of those passed to it, mostly.
    fn endpoint(&mut self) -> u64 {
        if self.rng.one_in(4) {
            self.slot()
        } else {
            u64::from(SLOT_FIRST_EXTRA) + self.rng.below(2)
        }
    }

    /// A notification slot, likewise.
    fn notification(&mut self) -> u64 {
        if self.rng.one_in(4) {
            self.slot()
        } else {
            u64::from(SLOT_FIRST_EXTRA) + 2 + self.rng.below(2)
        }
    }

    /// The next system call and its arguments.
    fn next_call(&mut self) -> (u64, [u64; 10]) {
        let mut args = [0u64; 10];
        let number = match self.rng.below(40) {
            0..=9 => return self.method(),
            10..=12 => {
                // A call on an endpoint: it waits for the reply.
                let info = self.message_info();
                args[0] = self.endpoint();
                args[1] = info;
                args[2..].copy_from_slice(&self.message(info));
                syscalls::CAP_CALL
            }
            13..=15 => {
                let info = self.message_info();
                args[0] = self.endpoint();
                args[1] = info;
                args[2..].copy_from_slice(&self.message(info));
                syscalls::SEND
            }
            16..=18 => {
                args[0] = self.endpoint();
                args[1] = if self.rng.one_in(16) { 0 } else { MessageFlags::NO_BLOCK.0 };
                if self.rng.one_in(16) {
                    args[1] |= self.rng.next();
                }
                syscalls::RECV
            }
            19..=21 => {
                let info = self.message_info();
                args[0] = if self.rng.one_in(3) { u64::from(NO_SLOT) } else { self.endpoint() };
                args[1] = info;
                args[2..].copy_from_slice(&self.message(info));
                syscalls::REPLY_RECV
            }
            22 | 23 => {
                args[0] = self.notification();
                syscalls::NOTIFY
            }
            24 | 25 => {
                args[0] = self.notification();
                args[1] = if self.rng.one_in(3) { 0 } else { WaitFlags::POLL.0 };
                if self.rng.one_in(16) {
                    args[1] = self.rng.next();
                }
                args[2] = self.timeout();
                syscalls::WAIT
            }
            26 => syscalls::YIELD,
            27..=29 => {
                args[0] = if self.rng.one_in(4) { self.slot() } else { u64::from(SLOT_SPACE) };
                args[1] = self.slot();
                args[2] = self.page_address();
                args[3] = self.rights();
                args[4] = self.rng.pick(&[0, 0, 0, 1, 2, 511, 512, u64::MAX]);
                args[5] = self.pages();
                syscalls::MAP
            }
            30 | 31 => {
                args[0] = if self.rng.one_in(4) { self.slot() } else { u64::from(SLOT_SPACE) };
                args[1] = self.page_address();
                args[2] = self.pages();
                syscalls::UNMAP
            }
            32 => {
                args[0] = if self.rng.one_in(4) { self.slot() } else { u64::from(SLOT_SPACE) };
                args[1] = self.page_address();
                args[2] = self.rng.pick(&[1, 2, 4, 8, 3, 0, ENTRIES_MAX, ENTRIES_MAX * 2]).into();
                args[3] = self.spare();
                syscalls::RING_SETUP
            }
            33..=35 => {
                self.write_entries();
                args[0] = if self.rng.one_in(8) { self.slot() } else { u64::from(RING_SLOT) };
                args[1] = self.rng.pick(&[0, 1, 2, 4, 8, 16, u32::MAX]).into();
                args[2] = self.rng.pick(&[0, 0, 1, 2, 16, 17, u32::MAX]).into();
                // Completions may never come: see `next_call`'s end.
                args[3] = self.timeout();
                syscalls::RING_ENTER
            }
            36 => {
                args[0] = if self.rng.one_in(4) { self.slot() } else { u64::from(SLOT_LOG) };
                // Mostly text that is not there: what memory the program
                // can read would only flood the log.
                (args[1], args[2]) = if self.rng.one_in(8) {
                    (TEXT.as_ptr() as u64, self.rng.below(TEXT.len() as u64 + 1))
                } else {
                    (self.unmapped(), self.length())
                };
                syscalls::DEBUG_LOG
            }
            37 => self.rng.pick(&[12, 13, 64, 1 << 32, u64::MAX]),
            _ => {
                for word in &mut args {
                    *word = self.word();
                }
                self.rng.below(12)
            }
        };
        if self.rng.one_in(32) {
            args[self.rng.below(10) as usize] = self.word();
        }
        // Nobody else completes a fuzzer's ring entries, so a ring_enter
        // that waits for completions gives up soon (and never waits for
        // good, as a timeout of 0 would, or for years).
        if number == syscalls::RING_ENTER && !(1..=MAX_RING_WAIT_NS).contains(&args[3]) {
            args[3] = MAX_RING_WAIT_NS;
        }
        (number, args)
    }

    /// Writes random entries into its ring's submission queue, moves the
    /// tail by a random amount, reads the completions (or says it did),
    /// and now and then scribbles on the header.
    fn write_entries(&mut self) {
        let sq = |field: usize| ring_word(RingHeader::OFFSET_SQ + field);
        let cq = |field: usize| ring_word(RingHeader::OFFSET_CQ + field);
        let sqes = self.sqes;
        let mask = RING_ENTRIES - 1;
        let tail = sq(RingIndexes::OFFSET_TAIL).load(Ordering::Relaxed);
        let count = self.rng.below(u64::from(RING_ENTRIES) + 1) as u32;
        for i in 0..count {
            let index = tail.wrapping_add(i) & mask;
            let at = RING_AT + u64::from(sqes) + u64::from(index) * Sqe::SIZE as u64;
            let mut entry = [0u64; Sqe::SIZE / 8];
            let opcode = if self.rng.one_in(8) { self.rng.below(256) } else { self.rng.below(6) };
            let flags = if self.rng.one_in(16) { self.rng.below(256) } else { 0 };
            let reserved = if self.rng.one_in(16) { self.rng.below(1 << 16) } else { 0 };
            let cap = match self.rng.below(4) {
                0 => SLOT_FIRST_EXTRA as u64 + 2 + self.rng.below(2),
                1 => u64::from(RING_SLOT),
                _ => self.slot() & 0xffff_ffff,
            };
            entry[0] = opcode | (flags << 8) | (reserved << 16) | (cap << 32);
            entry[1] = self.rng.next();
            for arg in &mut entry[2..] {
                *arg = self.rng.pick(&[0, 1, 1_000, 1_000_000, u64::MAX, self.rng.0]);
            }
            for (k, &value) in entry.iter().enumerate() {
                // SAFETY: the entry is one of the ring's submission entries
                // (the index is masked to their number, the start is the one
                // the kernel wrote at setup), 8-aligned, in the mapped ring.
                // The kernel only ever copies entries out, so a write while
                // it reads changes what it copies and nothing more.
                unsafe { core::ptr::write_volatile((at + 8 * k as u64) as *mut u64, value) };
            }
        }
        let moved = match self.rng.below(16) {
            0 => self.rng.next() as u32,
            1 => count.wrapping_add(RING_ENTRIES),
            _ => count,
        };
        sq(RingIndexes::OFFSET_TAIL).store(tail.wrapping_add(moved), Ordering::Release);
        let head = match self.rng.below(16) {
            0 => self.rng.next() as u32,
            1..=4 => cq(RingIndexes::OFFSET_HEAD).load(Ordering::Relaxed),
            _ => cq(RingIndexes::OFFSET_TAIL).load(Ordering::Acquire),
        };
        cq(RingIndexes::OFFSET_HEAD).store(head, Ordering::Release);
        if self.rng.one_in(32) {
            let offset = self.rng.below(RingHeader::SIZE as u64 / 4) as usize * 4;
            ring_word(offset).store(self.rng.next() as u32, Ordering::Relaxed);
        }
    }

    /// Lets go of the partners that may be waiting for it, before it
    /// exits: takes what waits on both endpoints and answers a call at
    /// once, sends a receiver that waits a message, and signals both
    /// notifications. The capabilities may be gone by now; then this does
    /// nothing.
    fn drain(&mut self) {
        for _ in 0..DRAIN_ROUNDS {
            self.drain_once();
            self.call(syscalls::YIELD, [0; 10]);
        }
    }

    /// One round of [`Fuzzer::drain`].
    fn drain_once(&mut self) {
        let no_block = MessageFlags::NO_BLOCK.0;
        for endpoint in [SLOT_FIRST_EXTRA, SLOT_FIRST_EXTRA + 1].map(u64::from) {
            self.call(syscalls::RECV, [endpoint, no_block, 0, 0, 0, 0, 0, 0, 0, 0]);
            self.call(syscalls::REPLY_RECV, [NO_SLOT.into(), 0, 0, 0, 0, 0, 0, 0, 0, 0]);
            self.call(syscalls::SEND, [endpoint, no_block, 0, 0, 0, 0, 0, 0, 0, 0]);
        }
        for notification in [SLOT_FIRST_EXTRA + 2, SLOT_FIRST_EXTRA + 3].map(u64::from) {
            self.call(syscalls::NOTIFY, [notification, 0, 0, 0, 0, 0, 0, 0, 0, 0]);
        }
    }

    /// Makes system call `number` and checks what a failed one returns.
    fn call(&mut self, number: u64, args: [u64; 10]) {
        let (status, results) = sys::syscall(number, args);
        self.calls += 1;
        if status == 0 {
            return;
        }
        self.failed += 1;
        let error = Kernel::from_status(Status(status));
        if error.is_none() || results != [0; 10] {
            self.broken += 1;
            if self.broken <= 4 {
                println!(
                    "fuzz: broken promise: call {number:#x} {args:#x?} failed with {status:#x} \
                     and results {results:#x?}"
                );
            }
        }
        if let Some(error) = error
            && !documented(number, args[1], error)
        {
            self.undocumented += 1;
            // A method is told apart by its label; other calls by number.
            let label = if number == syscalls::CAP_CALL { args[1] as u32 } else { 0 };
            let key = (number, label, error.code());
            if self.reported.contains(&key) {
                return;
            }
            if let Some(free) = self.reported.iter_mut().find(|seen| seen.2 == 0) {
                *free = key;
                let name = syscalls::name(number).unwrap_or("an unknown call");
                println!(
                    "fuzz: undocumented error: {name} (label {label:#x}) failed with {}",
                    error.name()
                );
            }
        }
    }
}

/// Whether the documentation lists `error` for call `number` with info word
/// `info`: a method may fail with the errors of its own list and those of
/// `cap_call`, and a call that does not exist only with no_such_syscall.
fn documented(number: u64, info: u64, error: Kernel) -> bool {
    let name = error.name();
    let Some(own) = raises::syscall(number) else {
        return error == Kernel::NoSuchSyscall;
    };
    own.contains(&name)
        || (number == syscalls::CAP_CALL
            && raises::method(info as u32).is_some_and(|method| method.contains(&name)))
}

/// A word of the ring's header.
fn ring_word(offset: usize) -> &'static AtomicU32 {
    // SAFETY: the ring is mapped read-write at RING_AT for as long as its
    // capability, in a slot this program never gives up, exists; the
    // header's words are 4-aligned and shared with the kernel, so they are
    // only accessed atomically.
    unsafe { &*((RING_AT + offset as u64) as *const AtomicU32) }
}

/// Text for the log, from the program's read-only data.
const TEXT: &[u8] = b"fuzz: a line from the fuzzer\twith a tab, \x07a bell and \xff bad UTF-8";

/// The janitor of a group of fuzzers: over and over, it takes what waits
/// on both endpoints and answers calls at once, sends a receiver that
/// waits a message, signals both notifications, throws away the
/// capabilities messages brought it, and sleeps for a moment. So a fuzzer
/// that waits for a partner is let go even when every other fuzzer waits
/// too, and the group gets through its calls. It never waits for good,
/// and runs until the kernel stops it.
pub fn janitor() -> u64 {
    const NAP_NS: u64 = 200_000;
    let mut fuzzer = Fuzzer::new(0, 0);
    // A ring with nothing in it, to sleep in.
    if let Err(status) = syscalls::ring_setup(&libradix::SYS, SLOT_SPACE, RING_AT, 1, RING_SLOT) {
        println!("fuzz janitor: no ring: {status:#x?}");
        return 1;
    }
    loop {
        fuzzer.drain_once();
        for slot in SLOT_WINDOW..SLOT_WINDOW + WINDOW_SLOTS {
            let _ = cap::delete(&libradix::SYS, slot);
        }
        let _ = syscalls::ring_enter(&libradix::SYS, RING_SLOT, 0, 1, NAP_NS);
        if fuzzer.broken > 0 {
            return u64::from(fuzzer.broken);
        }
    }
}

/// Runs the fuzzer with the seed in `argument`, and returns how many
/// failed calls broke the kernel's promise or failed with an error their
/// documentation does not list.
pub fn run(argument: u64) -> u64 {
    let seed = argument & 0xffff_ffff;
    let mut fuzzer = Fuzzer::new(seed, startup().extra);
    if let Err(status) =
        syscalls::ring_setup(&libradix::SYS, SLOT_SPACE, RING_AT, RING_ENTRIES, RING_SLOT)
    {
        println!("fuzz: no ring: {status:#x?}");
        return 1;
    }
    fuzzer.sqes = ring_word(RingHeader::OFFSET_SQES).load(Ordering::Relaxed);
    while fuzzer.calls < CALLS {
        let (number, args) = fuzzer.next_call();
        fuzzer.call(number, args);
    }
    fuzzer.drain();
    println!(
        "fuzz: seed {seed:#x}: {} calls, {} failed, {} broke a promise, {} failed with an \
         undocumented error",
        fuzzer.calls, fuzzer.failed, fuzzer.broken, fuzzer.undocumented
    );
    u64::from(fuzzer.broken + fuzzer.undocumented)
}
