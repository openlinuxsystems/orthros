// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! The user-mode self-tests (Phase 5), which `selftest` runs by name. They
//! start programs from the `usertests` boot module (`user::launch`), whose
//! roles are in `userspace/usertests`:
//!
//! - `user`: the probe goes through every system call and every method
//!   with good and bad arguments, checking each answer, and exits with 0;
//!   then programs make each kind of fault (a null pointer, a write to a
//!   read-only page, code on the stack, a privileged instruction, a jump
//!   into the kernel's half, an undefined instruction, a breakpoint and a
//!   division by zero), each of which must end that program and nothing
//!   else.
//! - `ipc`: user-mode programs ping-pong over an endpoint (on one CPU and
//!   across two), pass capabilities in messages, signal notifications,
//!   talk through a shared-memory channel, and ping-pong over two rings.
//! - `fuzz`: groups of three programs make random system calls, methods
//!   and ring entries, with arguments at the edges of what the kernel
//!   accepts, sharing endpoints, notifications and memory; whatever still
//!   waits at the end is stopped. The kernel must not panic or trip the
//!   lock checker, every failed call must return a kernel error that its
//!   documentation lists and nothing else, and the capability store must
//!   pass its audit.
//! - `bench`: measures system calls, IPC round trips, context switches,
//!   fresh pages, memory bandwidth, TLB shootdowns and system calls on two
//!   CPUs at once (`ob bench` reads the numbers, docs/design/ipc.md says
//!   how).
//!
//! Each test logs `selftest NAME: passed` or `failed` lines. `user`, `ipc`
//! and `fuzz` also check that the capability store and the object registry
//! hold what they held before, once every program has gone.

use radix_cap::{KINDS, Rights};
use radix_klog::{error, info, warn};

use crate::sched;
use crate::user::launch::{self, Group, Spec};
use crate::user::{EXIT_FAULT, EXIT_STOPPED};
use crate::{caps, user};

/// The programs of the image (`userspace/usertests/src/main.rs` numbers
/// them the same way).
mod role {
    pub const PROBE: u32 = 1;
    pub const FAULT: u32 = 2;
    pub const ECHO: u32 = 3;
    pub const CALL: u32 = 4;
    pub const CAP_SEND: u32 = 5;
    pub const CAP_RECEIVE: u32 = 6;
    pub const NOTIFY_PING: u32 = 7;
    pub const NOTIFY_PONG: u32 = 8;
    pub const CHANNEL_A: u32 = 9;
    pub const CHANNEL_B: u32 = 10;
    pub const RING_A: u32 = 11;
    pub const RING_B: u32 = 12;
    pub const SYSCALL_BENCH: u32 = 13;
    pub const MEMORY_BENCH: u32 = 14;
    pub const YIELD_A: u32 = 15;
    pub const YIELD_B: u32 = 16;
    pub const SHOOT_A: u32 = 17;
    pub const SHOOT_B: u32 = 18;
    pub const PARALLEL_A: u32 = 19;
    pub const PARALLEL_B: u32 = 20;
    pub const FUZZ: u32 = 21;
    pub const FUZZ_JANITOR: u32 = 22;
    pub const FILE_BENCH: u32 = 23;
}

/// The faults the fault program makes, by its argument.
const FAULTS: [&str; 8] = [
    "a null pointer",
    "a write to a read-only page",
    "code on the stack",
    "a privileged instruction",
    "a jump into the kernel's half",
    "an undefined instruction",
    "a breakpoint",
    "a division by zero",
];

/// The flag in a program's argument that has it report its timings.
const BENCH: u64 = 1 << 63;
/// The flag that tells a pair of programs they run on two CPUs.
const ACROSS: u64 = 1 << 62;

/// How long a test's programs get, in nanoseconds.
const TIMEOUT_NS: u64 = 60_000_000_000;

/// Every right, for objects passed to programs.
const ALL: Rights = Rights::ALL;

struct Outcome {
    name: &'static str,
    failures: u32,
}

impl Outcome {
    fn new(name: &'static str) -> Outcome {
        Outcome { name, failures: 0 }
    }

    fn fail(&mut self, args: core::fmt::Arguments<'_>) {
        error!("selftest {}: failed; {args}", self.name);
        self.failures += 1;
    }

    /// Waits for the group's programs and checks that each exited with
    /// `want`.
    fn check(&mut self, what: &str, group: &Group, want: u64) {
        if !group.wait(TIMEOUT_NS) {
            self.fail(format_args!("{what}: the programs did not finish in time"));
            group.stop();
        }
        for index in 0..group.len() {
            match group.status(index) {
                Some(status) if status == want => {}
                Some(EXIT_STOPPED) => {
                    self.fail(format_args!("{what}: {} was stopped", group.name(index)));
                }
                Some(status) => self.fail(format_args!(
                    "{what}: {} exited with {status:#x}, not {want:#x}",
                    group.name(index)
                )),
                None => self.fail(format_args!("{what}: {} did not exit", group.name(index))),
            }
        }
    }

    /// Fails unless the store and the registry hold what they held
    /// `before` the test's programs started.
    fn nothing_left(&mut self, before: Option<&Census>) {
        let after = census();
        if before.is_none() || after.as_ref() != before {
            self.fail(format_args!("objects were left behind: {before:?} before, {after:?} after"));
        }
    }

    fn end(self, summary: core::fmt::Arguments<'_>) {
        if self.failures == 0 {
            info!("selftest {}: passed; {summary}", self.name);
        }
    }
}

/// What the capability store and the user object registry hold, by kind.
#[derive(Debug, PartialEq, Eq)]
struct Census {
    spaces: u32,
    caps: [u64; KINDS],
    objects: [u64; KINDS],
    registered: [u32; KINDS],
}

/// The census, once every group has gone: a space, capability or object a
/// program's end missed shows up as a difference.
fn census() -> Option<Census> {
    let (store, registered) =
        caps::with(|locked| (locked.census(), user::objects::census(&locked.registry))).ok()?;
    Some(Census { spaces: store.spaces, caps: store.caps, objects: store.objects, registered })
}

/// Starts the programs of `specs` in a new group.
fn group(
    outcome: &mut Outcome,
    what: &str,
    make: impl FnOnce(&mut Group) -> Result<(), ()>,
) -> Option<Group> {
    let mut group = match Group::new() {
        Ok(group) => group,
        Err(status) => {
            outcome.fail(format_args!("{what}: no group: {:#x}", status.0));
            return None;
        }
    };
    if make(&mut group).is_err() {
        outcome.fail(format_args!("{what}: could not start the programs"));
        return Some(group);
    }
    Some(group)
}

fn start(group: &mut Group, spec: Spec<'_>) -> Result<(), ()> {
    group.start(&spec).map(|_| ()).map_err(|_| ())
}

fn has_image(name: &str) -> bool {
    if launch::image().is_none() {
        warn!(
            "selftest {name}: skipped; the boot loader loaded no `{}` module",
            launch::MODULE_WORD
        );
        return false;
    }
    true
}

/// How many CPUs are online.
fn online() -> usize {
    sched::online().len()
}

/// The CPUs a pair of programs runs on: the same one, or two.
fn cpus(across: bool) -> Option<(usize, usize)> {
    if !across {
        return Some((0, 0));
    }
    (online() >= 2).then_some((0, 1))
}

/// `selftest=user`.
pub fn user() {
    if !has_image("user") {
        return;
    }
    let mut outcome = Outcome::new("user");
    let before = census();
    if let Some(group) = group(&mut outcome, "probe", |group| {
        start(group, Spec { name: "probe", role: role::PROBE, argument: 0, cpu: 0, extra: &[] })
    }) {
        outcome.check("probe", &group, 0);
    }
    for (which, fault) in FAULTS.iter().enumerate() {
        let cpu = which % online().max(1);
        let made = group(&mut outcome, fault, |group| {
            start(
                group,
                Spec { name: "fault", role: role::FAULT, argument: which as u64, cpu, extra: &[] },
            )
        });
        if let Some(group) = made {
            outcome.check(fault, &group, EXIT_FAULT);
        }
    }
    outcome.nothing_left(before.as_ref());
    outcome.end(format_args!(
        "the probe passed every check and {} kinds of fault each ended only their program",
        FAULTS.len()
    ));
}

/// Two programs sharing an endpoint and two notifications.
fn pair(
    outcome: &mut Outcome,
    what: &str,
    roles: (u32, u32),
    argument: u64,
    across: bool,
) -> Option<Group> {
    let (a, b) = cpus(across)?;
    let argument = if across { argument | ACROSS } else { argument };
    group(outcome, what, |group| {
        let endpoint = group.endpoint().map_err(|_| ())?;
        let first = group.notification().map_err(|_| ())?;
        let second = group.notification().map_err(|_| ())?;
        let extra_a = [(endpoint, ALL, 0), (first, ALL, 0), (second, ALL, 0)];
        let extra_b = [(endpoint, ALL, 0), (second, ALL, 0), (first, ALL, 0)];
        start(group, Spec { name: "a", role: roles.0, argument, cpu: a, extra: &extra_a })?;
        start(group, Spec { name: "b", role: roles.1, argument, cpu: b, extra: &extra_b })
    })
}

/// Groups of fuzzers `fuzz` runs, and the fuzzers in each; each group also
/// has a janitor.
const FUZZ_GROUPS: u64 = 8;
const FUZZERS: u64 = 3;
/// How long a group gets at most, in nanoseconds; then the kernel stops the
/// fuzzers still waiting. The janitor keeps any from waiting long, so they
/// all finish well before.
const FUZZ_TIME_NS: u64 = 30_000_000_000;
/// How often the kernel looks at a group of fuzzers.
const FUZZ_POLL_NS: u64 = 50_000_000;
/// The memory object a group's fuzzers share: 2^order pages.
const FUZZ_MEMORY_ORDER: u32 = 6;
/// The directory in the root where each round of fuzzers gets a tmpfs of
/// its own to call the directory and file methods in, and the most it
/// holds.
const FUZZ_TREE: &str = "fuzz";
const FUZZ_TREE_BYTES: u64 = 1 << 20;

/// `selftest=fuzz`.
pub fn fuzz() {
    if !has_image("fuzz") {
        return;
    }
    let mut outcome = Outcome::new("fuzz");
    let before = census();
    let first = crate::settings().fuzz_seed.unwrap_or_else(radix_random::u64) & 0xffff_ffff;
    let seed = |n: u64| first.wrapping_add(n) & 0xffff_ffff;
    let last = seed(FUZZ_GROUPS * FUZZERS - 1);
    info!(
        "selftest fuzz: seeds {first:#x} to {last:#x}; boot with fuzz-seed={first} to repeat them"
    );
    let (mut finished, mut stopped) = (0, 0);
    for round in 0..FUZZ_GROUPS {
        let tree = match crate::fs::scratch_tree(FUZZ_TREE, FUZZ_TREE_BYTES) {
            Ok(tree) => tree,
            Err(err) => {
                outcome.fail(format_args!("no tree for the fuzzers: {err}"));
                continue;
            }
        };
        let made = group(&mut outcome, "fuzzers", |group| {
            let endpoints = [group.endpoint().map_err(|_| ())?, group.endpoint().map_err(|_| ())?];
            let notifications =
                [group.notification().map_err(|_| ())?, group.notification().map_err(|_| ())?];
            let memory = group.memory(FUZZ_MEMORY_ORDER).map_err(|_| ())?;
            let directory = group.directory(tree.clone()).map_err(|_| ())?;
            // Badged and unbadged capabilities, as the programs' slots 8 to
            // 13 (`userspace/usertests/src/fuzz.rs` counts on that order).
            let extra = [
                (endpoints[0], ALL, 0),
                (endpoints[1], ALL, 7),
                (notifications[0], ALL, 0),
                (notifications[1], ALL, 0x30),
                (memory, ALL, 0),
                (directory, ALL, 0),
            ];
            for k in 0..FUZZERS {
                let argument = seed(round * FUZZERS + k);
                let cpu = k as usize % online().max(1);
                start(
                    group,
                    Spec { name: "fuzz", role: role::FUZZ, argument, cpu, extra: &extra },
                )?;
            }
            let cpu = FUZZERS as usize % online().max(1);
            let janitor =
                Spec { name: "janitor", role: role::FUZZ_JANITOR, argument: 0, cpu, extra: &extra };
            start(group, janitor)
        });
        let Some(group) = made else {
            drop(tree);
            take_fuzz_tree(&mut outcome);
            continue;
        };
        let fuzzers = 0..FUZZERS as usize;
        let started = sched::now_ns();
        while !fuzzers.clone().all(|index| group.status(index).is_some())
            && sched::now_ns() - started < FUZZ_TIME_NS
        {
            group.wait(FUZZ_POLL_NS);
        }
        if !fuzzers.clone().all(|index| group.status(index).is_some()) {
            // What the fuzzers wait for, for whoever reads the log.
            info!("selftest fuzz: fuzzers still running after {} s:", FUZZ_TIME_NS / 1_000_000_000);
            sched::write_threads(&mut |line| info!("{line}"));
            sched::write_queues(&mut |line| info!("{line}"));
        }
        group.stop();
        for index in fuzzers {
            let argument = seed(round * FUZZERS + index as u64);
            match group.status(index) {
                Some(0) => finished += 1,
                Some(EXIT_STOPPED) => stopped += 1,
                Some(status) => outcome.fail(format_args!(
                    "the fuzzer with seed {argument:#x} exited with {status:#x}"
                )),
                None => {
                    outcome.fail(format_args!("the fuzzer with seed {argument:#x} did not exit"));
                }
            }
        }
        match group.status(FUZZERS as usize) {
            Some(EXIT_STOPPED) => {}
            status => outcome.fail(format_args!("the janitor ended with {status:#x?}")),
        }
        drop(group);
        drop(tree);
        take_fuzz_tree(&mut outcome);
    }
    // Waiting fuzzers can be stopped, but the janitor lets go of each soon
    // enough that most should get through.
    let fuzzers = FUZZ_GROUPS * FUZZERS;
    if finished < fuzzers / 2 {
        outcome.fail(format_args!("only {finished} of {fuzzers} fuzzers got through their calls"));
    }
    match caps::with(|store| store.audit().map(|_| ())) {
        Ok(Ok(())) => {}
        Ok(Err(problem)) => outcome.fail(format_args!("the capability store's audit: {problem}")),
        Err(err) => outcome.fail(format_args!("no capability store: {err}")),
    }
    outcome.nothing_left(before.as_ref());
    outcome.end(format_args!(
        "{finished} of {fuzzers} fuzzers got through their calls and {stopped} were stopped while \
         waiting; every failed call returned a kernel error its documentation lists and nothing \
         else, the store passed its audit and nothing was left behind"
    ));
}

/// Takes the fuzzers' tree away once they have gone, with what they
/// mounted in it, giving their processes' ends a moment to let go of what
/// they held open.
fn take_fuzz_tree(outcome: &mut Outcome) {
    let mut result = crate::fs::remove_scratch_tree(FUZZ_TREE);
    for _ in 0..100 {
        if result != Err(radix_vfs::Error::InUse) {
            break;
        }
        sched::sleep_ns(10_000_000);
        result = crate::fs::remove_scratch_tree(FUZZ_TREE);
    }
    if let Err(err) = result {
        outcome.fail(format_args!("the fuzzers' tree could not be taken away: {err}"));
    }
}

/// The pairs of programs the `ipc` and `bench` tests run.
const PAIRS: [(&str, (u32, u32)); 5] = [
    ("endpoint", (role::CALL, role::ECHO)),
    ("capabilities", (role::CAP_SEND, role::CAP_RECEIVE)),
    ("notification", (role::NOTIFY_PING, role::NOTIFY_PONG)),
    ("channel", (role::CHANNEL_A, role::CHANNEL_B)),
    ("ring", (role::RING_A, role::RING_B)),
];

/// `selftest=ipc`.
pub fn ipc() {
    if !has_image("ipc") {
        return;
    }
    let mut outcome = Outcome::new("ipc");
    let before = census();
    let mut ran = 0;
    for across in [false, true] {
        for (what, roles) in PAIRS {
            let Some(group) = pair(&mut outcome, what, roles, 1000, across) else { continue };
            outcome.check(what, &group, 0);
            ran += 1;
        }
    }
    outcome.nothing_left(before.as_ref());
    outcome.end(format_args!(
        "{ran} pairs of programs ping-ponged over endpoints, capabilities in messages, \
         notifications, shared memory and rings, on one CPU and across two"
    ));
}

/// `selftest=bench`.
pub fn bench() {
    if !has_image("bench") {
        return;
    }
    let mut outcome = Outcome::new("bench");
    let before = sched::counters();
    if let Some(group) = group(&mut outcome, "syscalls", |group| {
        let argument = BENCH | SYSCALL_ROUNDS;
        start(
            group,
            Spec { name: "bench", role: role::SYSCALL_BENCH, argument, cpu: 0, extra: &[] },
        )
    }) {
        outcome.check("syscalls", &group, 0);
        report_counters("syscalls", SYSCALL_ROUNDS, &before);
    }
    let before = sched::counters();
    if let Some(group) = group(&mut outcome, "memory", |group| {
        let mut extra = [(0, ALL, 0); COPY_OBJECTS];
        for (slot, ..) in &mut extra {
            *slot = group.memory(COPY_ORDER).map_err(|_| ())?;
        }
        let argument = BENCH | PAGE_ROUNDS;
        start(
            group,
            Spec { name: "memory", role: role::MEMORY_BENCH, argument, cpu: 0, extra: &extra },
        )
    }) {
        outcome.check("memory", &group, 0);
        report_counters("memory", PAGE_ROUNDS, &before);
    }
    files(&mut outcome);
    for across in [false, true] {
        for (what, roles) in PAIRS {
            if roles.0 == role::CAP_SEND {
                continue;
            }
            let place = if across { "cross_cpu" } else { "same_cpu" };
            let mut name = radix_klog::TextBuf::<32>::new();
            let _ = core::fmt::Write::write_fmt(&mut name, format_args!("{what}_{place}"));
            bench_pair(&mut outcome, name.as_str(), roles, PAIR_ROUNDS, across);
        }
    }
    let yields = (role::YIELD_A, role::YIELD_B);
    bench_pair(&mut outcome, "context_switch", yields, PAIR_ROUNDS, false);
    if online() >= 2 {
        let shoot = (role::SHOOT_A, role::SHOOT_B);
        bench_pair(&mut outcome, "tlb_shootdown", shoot, SHOOT_ROUNDS, true);
        parallel(&mut outcome);
    }
    outcome.end(format_args!("the numbers are above, one BENCH line each"));
}

/// The file benchmarks: a program with a tmpfs of its own, as the file
/// benchmarks of tools/linux-bench work on one.
fn files(outcome: &mut Outcome) {
    let tree = match crate::fs::scratch_tree(BENCH_TREE, BENCH_TREE_BYTES) {
        Ok(tree) => tree,
        Err(err) => return outcome.fail(format_args!("no tree for the file benchmarks: {err}")),
    };
    let before = sched::counters();
    if let Some(group) = group(outcome, "files", |group| {
        let directory = group.directory(tree.clone()).map_err(|_| ())?;
        let argument = BENCH | FILE_ROUNDS;
        let extra = [(directory, ALL, 0)];
        start(
            group,
            Spec { name: "files", role: role::FILE_BENCH, argument, cpu: 0, extra: &extra },
        )
    }) {
        outcome.check("files", &group, 0);
        report_counters("files", FILE_ROUNDS, &before);
    }
    drop(tree);
    if let Err(err) = crate::fs::remove_scratch_tree(BENCH_TREE) {
        outcome.fail(format_args!("the file benchmarks' tree could not be taken away: {err}"));
    }
}

/// The parallel benchmarks: a pair of programs, one on each of the first
/// two CPUs, making system calls at once, then each calling an echo
/// server of its own on its own CPU.
fn parallel(outcome: &mut Outcome) {
    let before = sched::counters();
    let argument = BENCH | ACROSS | SYSCALL_ROUNDS;
    let made = group(outcome, "parallel", |group| {
        let endpoints = [group.endpoint().map_err(|_| ())?, group.endpoint().map_err(|_| ())?];
        let first = group.notification().map_err(|_| ())?;
        let second = group.notification().map_err(|_| ())?;
        let extra_a = [(endpoints[0], ALL, 0), (first, ALL, 0), (second, ALL, 0)];
        let extra_b = [(endpoints[1], ALL, 0), (second, ALL, 0), (first, ALL, 0)];
        let (role_a, role_b) = (role::PARALLEL_A, role::PARALLEL_B);
        start(group, Spec { name: "a", role: role_a, argument, cpu: 0, extra: &extra_a })?;
        start(group, Spec { name: "b", role: role_b, argument, cpu: 1, extra: &extra_b })?;
        for (cpu, endpoint) in endpoints.into_iter().enumerate() {
            let extra = [(endpoint, ALL, 0)];
            start(group, Spec { name: "echo", role: role::ECHO, argument: 0, cpu, extra: &extra })?;
        }
        Ok(())
    });
    if let Some(group) = made {
        outcome.check("parallel", &group, 0);
        report_counters("parallel", SYSCALL_ROUNDS, &before);
    }
}

/// Runs a pair of benchmark programs and reports what the kernel did.
fn bench_pair(outcome: &mut Outcome, name: &str, roles: (u32, u32), rounds: u64, across: bool) {
    let before = sched::counters();
    let Some(group) = pair(outcome, name, roles, BENCH | rounds, across) else { return };
    outcome.check(name, &group, 0);
    report_counters(name, rounds, &before);
}

/// Rounds of each system call the benchmark makes, of each pair's round
/// trips, of fresh pages, and of TLB shootdowns.
const SYSCALL_ROUNDS: u64 = 200_000;
const PAIR_ROUNDS: u64 = 100_000;
const PAGE_ROUNDS: u64 = 50_000;
const SHOOT_ROUNDS: u64 = 50_000;
/// Rounds of each file operation, and the tmpfs the file benchmarks get,
/// in a directory of this name in the root.
const FILE_ROUNDS: u64 = 50_000;
const BENCH_TREE: &str = "bench";
const BENCH_TREE_BYTES: u64 = 1 << 20;

/// The memory objects the memory benchmark copies between: 16 of 4 MiB,
/// two 32 MiB buffers, more than the caches of the CPUs CI runs on.
const COPY_OBJECTS: usize = 16;
const COPY_ORDER: u32 = 10;

/// Logs what the kernel did for a benchmark since `before`: switches,
/// timer writes, interrupts sent, halts and polls, which `ob bench` divides
/// by the rounds. Each is a VM exit or more under a hypervisor, so they
/// explain the numbers. With them, the time TLB shootdowns waited for
/// other CPUs and the CPUs they left to the hypervisor.
fn report_counters(group: &str, rounds: u64, before: &sched::Counters) {
    let counts = sched::counters().since(before);
    info!(
        "BENCHCOUNT group={group} rounds={rounds} switches={} timer_writes={} ipis={} halts={} \
         polls={} shootdown_wait_ns={} shootdowns_left={}",
        counts.switches,
        counts.timer_writes,
        counts.ipis,
        counts.halts,
        counts.polls,
        counts.shootdown_wait_ns,
        counts.shootdowns_left
    );
}
