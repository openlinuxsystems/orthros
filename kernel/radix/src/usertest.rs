// SPDX-FileCopyrightText: 2026 The OrthrOS contributors
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
//! - `bench`: measures system call and IPC round trips (`ob bench` reads
//!   the numbers, docs/design/ipc.md says how).
//!
//! Each test logs `selftest NAME: passed` or `failed` lines. `user` and
//! `ipc` also check that the capability store and the object registry hold
//! what they held before, once every program has gone.

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
                    self.fail(format_args!("{what}: {} was stopped", group.name(index)))
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
    fn nothing_left(&mut self, before: Option<Census>) {
        let after = census();
        if before.is_none() || after != before {
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
    let store = caps::with(|store| store.census()).ok()?;
    let registered = user::objects::census()?;
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
    outcome.nothing_left(before);
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
    outcome.nothing_left(before);
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
    for across in [false, true] {
        for (what, roles) in PAIRS {
            if roles.0 == role::CAP_SEND {
                continue;
            }
            let before = sched::counters();
            let Some(group) = pair(&mut outcome, what, roles, BENCH | PAIR_ROUNDS, across) else {
                continue;
            };
            outcome.check(what, &group, 0);
            let place = if across { "cross_cpu" } else { "same_cpu" };
            let mut name = radix_klog::TextBuf::<32>::new();
            let _ = core::fmt::Write::write_fmt(&mut name, format_args!("{what}_{place}"));
            report_counters(name.as_str(), PAIR_ROUNDS, &before);
        }
    }
    outcome.end(format_args!("the numbers are above, one BENCH line each"));
}

/// Rounds of each system call the benchmark makes, and of each pair's
/// round trips.
const SYSCALL_ROUNDS: u64 = 200_000;
const PAIR_ROUNDS: u64 = 100_000;

/// Logs what the kernel did for a benchmark since `before`: switches,
/// timer writes, interrupts sent, halts and polls, which `ob bench` divides
/// by the rounds. Each is a VM exit or more under a hypervisor, so they
/// explain the numbers.
fn report_counters(group: &str, rounds: u64, before: &sched::Counters) {
    let counts = sched::counters().since(before);
    info!(
        "BENCHCOUNT group={group} rounds={rounds} switches={} timer_writes={} ipis={} halts={} \
         polls={}",
        counts.switches, counts.timer_writes, counts.ipis, counts.halts, counts.polls
    );
}
