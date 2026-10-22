// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! The fuzzer: random system calls, methods and ring entries, with
//! arguments drawn mostly from values at the edges of what the kernel
//! accepts, so that the kernel's `fuzz` self-test can show that nothing a
//! program passes crashes the kernel, trips the lock checker or leaves an
//! object behind.
//!
//! The kernel starts three of these together and gives them two
//! endpoints, two notifications, a memory object and a directory to share,
//! so calls that wait can meet a partner, capabilities travel between them
//! and files one makes another finds. The directory is a tmpfs of the
//! group's own, which the kernel takes away with whatever was mounted in
//! it once the group has gone. A
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
//! thread, address space, memory, random number generator, log, ring and
//! the group's directory), and never ends its own thread. Nor does it
//! mount `dev`, whose block devices are the disks of a machine that has
//! them. Nor can it end its own process or start
//! a thread in it: it first makes the processes it calls the process
//! methods on most, then deletes its own process's capability. Everything
//! else is fair game, and the processes it makes go when it ends. After
//! every call it checks what radix-syscalls(2) promises of a failed one:
//! an error of a domain errors.ridl defines (the kernel's, or the file
//! interfaces'), and 0 in every result register. It also checks that
//! the error is one the call's documentation lists (its `raises` list,
//! and for a method that of `cap_call` too). It exits with the number of
//! calls that broke a promise or failed with an error their documentation
//! does not list.

use core::sync::atomic::{AtomicU32, Ordering};

use libradix::abi::capability as cap;
use libradix::abi::endpoint::{
    INFO_CAPS_SHIFT, INFO_WORDS_SHIFT, MessageFlags, NO_SLOT, RESERVED_LABELS,
};
use libradix::abi::errors::{File as FileError, Kernel};
use libradix::abi::notification::WaitFlags;
use libradix::abi::ring::{ENTRIES_MAX, RingHeader, RingIndexes, Sqe};
use libradix::abi::startup::{
    SLOT_FIRST_EXTRA, SLOT_LOG, SLOT_MEMORY, SLOT_PROCESS, SLOT_RANDOM, SLOT_SPACE, SLOT_THREAD,
    SLOT_WINDOW, SLOTS, STARTUP_ADDRESS, WINDOW_SLOTS,
};
use libradix::abi::{
    Status, address_space, directory, file, memory, process, raises, random, ring, syscalls, thread,
};
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

/// The processes it makes at the start, in slots it may give up, and
/// calls the process methods on most.
const CHILD: u32 = 104;
const CHILDREN: u32 = 2;

/// The directory the group shares, in a slot it never gives up, and how
/// many of the objects passed to it come before it: those it may give up.
const DIRECTORY: u32 = SLOT_FIRST_EXTRA + 5;
const SHARED: u32 = 5;

/// Slots `open` puts the files and directories it opens in, which it may
/// give up.
const FILES: u32 = 112;
const FILE_SLOTS: u32 = 4;

/// Where `process.create` puts the endpoints and notifications it makes.
/// Nobody else can reach those, and nobody would let go of a call that
/// waited on one, so the program only ever looks at them and deletes
/// them: no other slot it picks is one of these, and nothing is moved,
/// copied or sent from them.
const MADE: u32 = 108;
const MADE_SLOTS: u32 = 2;
/// Slots `process.create` cannot put anything in: past the end, or taken.
const NOT_MADE: [u32; 3] = [SLOTS, u32::MAX, SLOT_THREAD];
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
const LABELS: [u32; 42] = [
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
    process::CREATE,
    process::SET_NAME,
    process::ADDRESS_SPACE,
    process::GIVE,
    process::SHARE,
    process::KEEP,
    process::START,
    process::EXIT,
    process::NOTIFY_EXIT,
    process::INFO,
    directory::OPEN,
    directory::MAKE_DIRECTORY,
    directory::REMOVE,
    directory::RENAME,
    directory::LIST,
    directory::STAT,
    directory::MOUNT,
    directory::UNMOUNT,
    directory::SYNC,
    directory::INFO,
    file::READ,
    file::WRITE,
    file::STAT,
    file::SET_SIZE,
    file::SYNC,
];

/// Paths for the directory methods, in the program's read-only data: a few
/// names, so that calls meet what earlier ones made, and paths at the
/// edges of what radix-directory(2) takes.
const PATHS: [&[u8]; 26] = [
    b"a",
    b"a",
    b"b",
    b"c",
    b"a/b",
    b"a/c",
    b"b/a",
    b"a/b/c",
    b"a/b/c/d",
    b".",
    b"..",
    b"",
    b"a/..",
    b"../a",
    b"/a",
    b"a//b",
    b"a/",
    b"./b",
    b"a\0b",
    b"\xff\xfe",
    b"x.txt",
    b"A",
    &LONG_NAME,
    &TOO_LONG_NAME,
    &TOO_LONG_PATH,
    &DEEP_PATH,
];
const LONG_NAME: [u8; directory::NAME_MAX as usize] = [b'n'; directory::NAME_MAX as usize];
const TOO_LONG_NAME: [u8; directory::NAME_MAX as usize + 1] =
    [b'n'; directory::NAME_MAX as usize + 1];
const TOO_LONG_PATH: [u8; directory::PATH_MAX as usize + 1] =
    [b'p'; directory::PATH_MAX as usize + 1];
/// `a/a/a/...`, as long as a path may be.
const DEEP_PATH: [u8; directory::PATH_MAX as usize - 1] = deep_path();

const fn deep_path() -> [u8; directory::PATH_MAX as usize - 1] {
    let mut path = [b'a'; directory::PATH_MAX as usize - 1];
    let mut i = 1;
    while i < path.len() {
        path[i] = b'/';
        i += 2;
    }
    path
}

/// Filesystems to mount, by name: never `dev` (see the module's comment).
const FILESYSTEMS: [&[u8]; 8] =
    [b"tmpfs", b"tmpfs", b"proc", b"sys", b"bootfs", b"fat", b"nfs", b""];

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
    /// Directory and file methods called, and how many of them did what
    /// they were asked, so the log shows that files were made and used.
    file_calls: u32,
    file_done: u32,
    /// The undocumented errors reported so far, as (call, label, error),
    /// so that each is reported once.
    reported: [(u64, u32, u32); REPORTED],
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
            file_calls: 0,
            file_done: 0,
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
                SLOT_PROCESS,
                RING_SLOT,
            ])),
            4..=10 => self.spare(),
            11 => self.rng.pick(&[0, 6, u64::from(SLOTS) - 1, u64::from(SLOTS)]),
            12 => u64::from(NO_SLOT),
            13 => {
                let slot = self.rng.below(u64::from(SLOTS));
                if (u64::from(MADE)..u64::from(MADE + MADE_SLOTS)).contains(&slot) {
                    0
                } else {
                    slot
                }
            }
            14 => self.rng.next() & 0xffff_ffff,
            _ => self.rng.next(),
        }
    }

    /// A slot it may delete, move or revoke: the objects passed to it, the
    /// receive window, the processes it made and its scratch slots.
    fn spare(&mut self) -> u64 {
        let shared = self.extra.clamp(1, SHARED);
        let slot = match self.rng.below(9) {
            0 | 1 => SLOT_FIRST_EXTRA + self.rng.below(u64::from(shared)) as u32,
            2 => SLOT_WINDOW + self.rng.below(u64::from(WINDOW_SLOTS)) as u32,
            3 => CHILD + self.rng.below(u64::from(CHILDREN)) as u32,
            4 => self.opened() as u32,
            _ => SCRATCH + self.rng.below(u64::from(SCRATCH_SLOTS)) as u32,
        };
        u64::from(slot)
    }

    /// One of the slots `open` puts what it opens in.
    fn opened(&mut self) -> u64 {
        u64::from(FILES + self.rng.below(u64::from(FILE_SLOTS)) as u32)
    }

    /// A directory to call a directory method on: mostly the group's, or
    /// one `open` opened.
    fn directory(&mut self) -> u64 {
        match self.rng.below(8) {
            0..=4 => u64::from(DIRECTORY),
            5 | 6 => self.opened(),
            _ => self.slot(),
        }
    }

    /// A file to call a file method on: mostly one `open` opened.
    fn file(&mut self) -> u64 {
        if self.rng.one_in(8) { self.slot() } else { self.opened() }
    }

    /// A path, as its address and length: mostly one of [`PATHS`].
    fn path(&mut self) -> (u64, u64) {
        if self.rng.one_in(8) {
            (self.address(), self.length())
        } else {
            let path = self.rng.pick(&PATHS);
            (path.as_ptr() as u64, path.len() as u64)
        }
    }

    /// An offset in a file: mostly near its start or the edges of a page,
    /// sometimes far past any end, or `APPEND`.
    fn offset(&mut self) -> u64 {
        match self.rng.below(8) {
            0..=4 => self.rng.pick(&[0, 0, 1, 511, 512, 4095, 4096, 4097, 8192, 70_000]),
            5 => self.rng.pick(&[file::APPEND, file::APPEND, file::APPEND - 1, 1 << 40, 1 << 63]),
            _ => self.rng.below(20_000),
        }
    }

    /// Data to write, as its address and length: mostly the program's
    /// text, which the kernel only reads.
    fn data(&mut self) -> (u64, u64) {
        if self.rng.one_in(2) {
            (TEXT.as_ptr() as u64, self.rng.below(TEXT.len() as u64 + 1))
        } else {
            (self.address(), self.length())
        }
    }

    /// A process to call a process method on: mostly one it made, which
    /// may be gone by now, or one a message brought.
    fn process(&mut self) -> u64 {
        match self.rng.below(8) {
            0..=4 => u64::from(CHILD + self.rng.below(u64::from(CHILDREN)) as u32),
            5 => self.spare(),
            _ => self.slot(),
        }
    }

    /// One of the slots `process.create` puts endpoints and notifications
    /// in.
    fn made(&mut self) -> u64 {
        u64::from(MADE + self.rng.below(u64::from(MADE_SLOTS)) as u32)
    }

    /// A slot in a process it made, to give or start something in.
    fn child_slot(&mut self) -> u64 {
        match self.rng.below(8) {
            0..=5 => self.rng.below(u64::from(process::MIN_SLOTS) + 4),
            6 => self.rng.pick(&[u64::from(SLOTS), u64::from(process::MAX_SLOTS), u32::MAX.into()]),
            _ => self.rng.next(),
        }
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
            process::CREATE => {
                // Mostly a process, an endpoint or a notification; the
                // kinds are those of radix-capability(2).
                args[0] = self.rng.pick(&[4, 4, 5, 6, 0, 3, 14, 255]);
                args[1] = match args[0] {
                    4 => self.rng.pick(&[16, 16, 32, 256, 4096, 15, 4097, 0]),
                    _ => self.rng.pick(&[0, 0, 0, 1, 16]),
                };
                args[2] = match args[0] {
                    4 => self.spare(),
                    // A slot that is not there, or taken, will do too.
                    _ if self.rng.one_in(4) => self.rng.pick(&NOT_MADE).into(),
                    _ => self.made(),
                };
            }
            process::SET_NAME => {
                (args[0], args[1]) = if self.rng.one_in(2) {
                    (TEXT.as_ptr() as u64, self.rng.below(20))
                } else {
                    (self.address(), self.length())
                };
            }
            process::ADDRESS_SPACE | process::KEEP => args[0] = self.spare(),
            process::GIVE => {
                args[0] = self.spare();
                args[1] = self.child_slot();
            }
            process::SHARE => {
                args[0] = self.slot();
                args[1] = self.child_slot();
                args[2] = self.rights();
                args[3] = self.badge();
            }
            process::START => {
                // Nothing runs at these in a process it made but what it
                // mapped there: the thread soon faults, which ends the
                // process.
                args[0] = self.page_address();
                args[1] = self.page_address();
                args[2] = self.rng.next();
                args[3] = self.child_slot();
            }
            process::EXIT => {
                // 0 mostly, which the kernel log does not report.
                args[0] = if self.rng.one_in(8) { self.rng.next() } else { 0 };
            }
            process::NOTIFY_EXIT => args[0] = self.notification(),
            directory::OPEN => {
                (args[0], args[1]) = self.path();
                // Mostly no flags or create, sometimes with the others,
                // sometimes bits that mean nothing.
                args[2] = if self.rng.one_in(16) {
                    self.rng.next()
                } else {
                    self.rng.pick(&[0, 1, 1, 1, 1, 1, 3, 4, 5, 8, 9, 15])
                };
                args[3] = self.rights();
                args[4] = if self.rng.one_in(8) { self.slot() } else { self.opened() };
            }
            directory::MAKE_DIRECTORY
            | directory::REMOVE
            | directory::STAT
            | directory::UNMOUNT => (args[0], args[1]) = self.path(),
            directory::RENAME => {
                (args[0], args[1]) = self.path();
                args[2] = self.directory();
                (args[3], args[4]) = self.path();
            }
            directory::LIST => {
                args[0] = self.rng.pick(&[0, 0, 0, 1, 2, 3, u64::MAX, self.rng.0]);
                args[1] = self.address();
                args[2] = self.length();
            }
            directory::MOUNT => {
                (args[0], args[1]) = self.path();
                // The filesystem's name comes below.
                args[4] = if self.rng.one_in(4) { self.file() } else { 0 };
                args[5] = self.rng.pick(&[0, 0, 1, self.rng.0]);
            }
            file::READ => {
                args[0] = self.offset();
                args[1] = self.address();
                args[2] = self.length();
            }
            file::WRITE => {
                args[0] = self.offset();
                (args[1], args[2]) = self.data();
            }
            file::SET_SIZE => {
                args[0] = self.rng.pick(&[0, 0, 1, 100, 4096, 4097, 70_000, 1 << 20, 1 << 40]);
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
        // But `give` and `keep` move only capabilities it may give up,
        if matches!(label, process::GIVE | process::KEEP) {
            args[0] = self.spare();
        }
        // and the filesystem to mount is one of FILESYSTEMS, never `dev`,
        // or a name the kernel cannot read,
        if label == directory::MOUNT {
            let filesystem = self.rng.pick(&FILESYSTEMS);
            (args[2], args[3]) = if self.rng.one_in(8) {
                (self.unmapped(), self.length())
            } else {
                (filesystem.as_ptr() as u64, filesystem.len() as u64)
            };
        }
        // and whatever `process.create` makes other than a process goes
        // where `MADE` says.
        let to = args[2];
        let kept_apart = (u64::from(MADE)..u64::from(MADE + MADE_SLOTS)).contains(&to)
            || NOT_MADE.iter().any(|&slot| u64::from(slot) == to);
        if label == process::CREATE && args[0] != 4 && !kept_apart {
            args[2] = self.made();
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
        let target = if matches!(label, cap::DELETE | cap::INFO) && self.rng.one_in(4) {
            self.made()
        } else if gives_up {
            self.spare()
        } else if label >> 16 == process::CREATE >> 16 {
            self.process()
        } else if label >> 16 == directory::OPEN >> 16 {
            self.directory()
        } else if label >> 16 == file::READ >> 16 {
            self.file()
        } else {
            self.slot()
        };
        let mut words = self.rng.below(9);
        if self.rng.one_in(16) {
            words = self.rng.below(16);
        }
        // With no words, `give` and `keep` would move what is in slot 0.
        if matches!(label, process::GIVE | process::KEEP) {
            words = words.max(1);
        }
        // Files and directories need their words to get anywhere: mostly
        // exactly those, so that calls make and use files.
        if let Some(needed) = file_words(label)
            && !self.rng.one_in(4)
        {
            words = needed;
        }
        let mut info = u64::from(label) | (words << INFO_WORDS_SHIFT);
        if self.rng.one_in(16) {
            info |= self.info(label, true) & !u64::from(u32::MAX);
        }
        let mut args = [0u64; 10];
        args[0] = target;
        args[1] = info;
        args[2..].copy_from_slice(&self.method_args(label));
        // Room for what `open` opens, now and then: what was in its slot
        // goes first. (More often would leave the slots mostly empty for
        // the file methods.)
        let to = args[6];
        if label == directory::OPEN
            && (u64::from(FILES)..u64::from(FILES + FILE_SLOTS)).contains(&to)
            && self.rng.one_in(4)
        {
            self.call(syscalls::CAP_CALL, [to, cap::DELETE.into(), 0, 0, 0, 0, 0, 0, 0, 0]);
        }
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
        // Its own text, but never what follows it, which holds the lines
        // the boot test looks for.
        if number == syscalls::DEBUG_LOG && args[1] == TEXT.as_ptr() as u64 {
            args[2] = args[2].min(TEXT.len() as u64);
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
        let interface = (args[1] as u32) >> 16;
        if number == syscalls::CAP_CALL
            && (interface == directory::OPEN >> 16 || interface == file::READ >> 16)
        {
            self.file_calls += 1;
            self.file_done += u32::from(status == 0);
        }
        if status == 0 {
            return;
        }
        self.failed += 1;
        let error = Failure::from_status(Status(status));
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
            let key = (number, label, error.key());
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

/// How many words a directory or file method takes.
fn file_words(label: u32) -> Option<u64> {
    let words = match label {
        directory::OPEN => directory::OpenArgs::WORDS,
        directory::MAKE_DIRECTORY => directory::MakeDirectoryArgs::WORDS,
        directory::REMOVE => directory::RemoveArgs::WORDS,
        directory::RENAME => directory::RenameArgs::WORDS,
        directory::LIST => directory::ListArgs::WORDS,
        directory::STAT => directory::StatArgs::WORDS,
        directory::MOUNT => directory::MountArgs::WORDS,
        directory::UNMOUNT => directory::UnmountArgs::WORDS,
        file::READ => file::ReadArgs::WORDS,
        file::WRITE => file::WriteArgs::WORDS,
        file::SET_SIZE => file::SetSizeArgs::WORDS,
        directory::SYNC | directory::INFO | file::STAT | file::SYNC => 0,
        _ => return None,
    };
    Some(words as u64)
}

/// What a failed call failed with: an error of the kernel's own, or of
/// the file interfaces (radix-directory(2), radix-file(2)).
#[derive(Clone, Copy, PartialEq, Eq)]
enum Failure {
    Kernel(Kernel),
    File(FileError),
}

impl Failure {
    fn from_status(status: Status) -> Option<Failure> {
        Kernel::from_status(status)
            .map(Failure::Kernel)
            .or_else(|| FileError::from_status(status).map(Failure::File))
    }

    fn name(self) -> &'static str {
        match self {
            Failure::Kernel(error) => error.name(),
            Failure::File(error) => error.name(),
        }
    }

    /// Its domain and code, as one word that is never 0.
    fn key(self) -> u32 {
        match self {
            Failure::Kernel(error) => u32::from(Kernel::DOMAIN) << 16 | u32::from(error.code()),
            Failure::File(error) => u32::from(FileError::DOMAIN) << 16 | u32::from(error.code()),
        }
    }
}

/// Whether the documentation lists `error` for call `number` with info word
/// `info`: a method may fail with the errors of its own list and those of
/// `cap_call`, and a call that does not exist only with no_such_syscall.
fn documented(number: u64, info: u64, error: Failure) -> bool {
    let name = error.name();
    let Some(own) = raises::syscall(number) else {
        return error == Failure::Kernel(Kernel::NoSuchSyscall);
    };
    own.contains(&name)
        || (number == syscalls::CAP_CALL
            && raises::method(info as u32).is_some_and(|method| method.contains(&name)))
}

/// Makes the processes the fuzzer calls the process methods on, then
/// deletes its own process's capability (which cannot be copied), so that
/// no call it makes can end its process or start a thread in it. Its
/// `exit` then ends it through its thread (radix-process(2)).
///
/// Slot 0, and `SLOT_PROCESS` once it is empty, get processes too, which
/// stay there: a method called with fewer words than it takes reads 0 for
/// the rest, so `process.create` would otherwise put endpoints there, in
/// slots the program waits on.
fn make_children() -> Result<(), Status> {
    use libradix::SYS;
    let make = |slots, to| process::create(&SYS, SLOT_PROCESS, cap::ObjectKind::Process, slots, to);
    for k in 0..CHILDREN {
        make(process::MIN_SLOTS << k, CHILD + k)?;
    }
    make(process::MIN_SLOTS, 0)?;
    make(process::MIN_SLOTS, SCRATCH)?;
    cap::delete(&SYS, SLOT_PROCESS)?;
    cap::move_to(&SYS, SCRATCH, SLOT_PROCESS)
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
    if let Err(status) = make_children() {
        println!("fuzz: no processes to call methods on: {status:#x?}");
        return 1;
    }
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
         undocumented error; {} on files and directories, {} of them done",
        fuzzer.calls,
        fuzzer.failed,
        fuzzer.broken,
        fuzzer.undocumented,
        fuzzer.file_calls,
        fuzzer.file_done
    );
    u64::from(fuzzer.broken + fuzzer.undocumented)
}
