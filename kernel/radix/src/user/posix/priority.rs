// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Scheduling from POSIX (Phase 8, radix-process(2)'s `priority` and
//! `scheduling`): each program thread's policy, real-time priority and
//! nice level as POSIX names them, which the scheduler carries out
//! (radix-scheduling(7)).
//!
//! As on Linux, these belong to threads, and a process's number names its
//! first thread: `setpriority` and `sched_setscheduler` on a process
//! change that thread only. A thread starts with the scheduling of the
//! program thread that made it (`fork`, `exec`, `pthread_create`, or
//! Spark's `start`), or the default one. Changing another process's
//! threads needs what `kill` needs; asking for more of the CPU (a lower
//! nice level, a real-time priority) needs RLIMIT_NICE or RLIMIT_RTPRIO
//! to allow it, as for a process without privileges on Linux.
//!
//! The CPUs a thread may run on (`sched_setaffinity`, taskset(1)) are
//! here too, and a process's I/O class and level (`ioprio_set`,
//! ionice(1)), which belong to the process and which its disks' queues
//! serve it by (`crate::fs::disk`).

use alloc::vec::Vec;

use radix_abi::errors::Kernel;
use radix_abi::process::{self, IO_LEVELS, IoClass, LimitKind, PriorityWhich, SchedulingPolicy};
use radix_hal::{MAX_CPUS, UserMode};
use radix_mm::Kmalloc;
use radix_posix::tree::Life;
use radix_sched::CpuSet;

use super::{find, find_thread, join, kmalloc, with_tree};
use crate::Arch;
use crate::sched::{self, Policy, ThreadRef};
use crate::user::methods::{Answer, answer};
use crate::user::process::ProcessRef;
use crate::user::{Result, UserRef, error};

/// The most nice levels go from: the fair class's largest share.
const NICE_MIN: i32 = -20;
/// The least: its smallest share.
const NICE_MAX: i32 = 19;
/// The highest real-time priority.
const REALTIME_MAX: u32 = 99;

/// A thread's scheduling, as POSIX names it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Scheduling {
    /// Its policy.
    pub policy: SchedulingPolicy,
    /// Its real-time priority: 1 to 99 under fifo and round_robin, else 0.
    pub priority: u8,
    /// Its nice level, -20 to 19, which the fair policies but idle use.
    pub nice: i8,
}

impl Scheduling {
    /// What a thread has unless its maker had another: the fair class at
    /// nice 0.
    pub const DEFAULT: Scheduling =
        Scheduling { policy: SchedulingPolicy::Other, priority: 0, nice: 0 };

    /// The scheduler's policy for it.
    pub fn policy(self) -> Policy {
        match self.policy {
            SchedulingPolicy::Other | SchedulingPolicy::Batch => Policy::Fair { nice: self.nice },
            SchedulingPolicy::Idle => Policy::Fair { nice: NICE_MAX as i8 },
            SchedulingPolicy::Fifo => Policy::Fifo { prio: self.priority },
            SchedulingPolicy::RoundRobin => Policy::RoundRobin { prio: self.priority },
        }
    }
}

/// The scheduling a new thread starts with: the running thread's, if it
/// is a program's.
pub fn inherited() -> Scheduling {
    sched::current()
        .and_then(|me| me.user().map(|user| *user.scheduling.lock()))
        .unwrap_or(Scheduling::DEFAULT)
}

/// A thread a call names, and whether the caller may change it.
type Named = (ThreadRef, UserRef, bool);

/// The scheduling methods of radix-process(2), called by `user`'s thread.
#[inline(never)]
pub fn method(user: &UserRef, label: u32, args: &[u64; 8]) -> Result<Answer> {
    match label {
        process::PRIORITY => {
            let a = process::PriorityArgs::decode(args)?;
            let nice = priority(user, a.which, a.who, a.set.then_some(a.nice))?;
            let results = process::PriorityResults { nice };
            answer(label, process::PriorityResults::WORDS, results.encode())
        }
        process::SCHEDULING => {
            let a = process::SchedulingArgs::decode(args)?;
            let (thread, target, permitted) = thread_named(user, a.thread)?;
            let old = *target.scheduling.lock();
            if a.set {
                if !permitted {
                    return Err(error(Kernel::NotPermitted));
                }
                set_policy(&thread, &target, a.policy, a.priority)?;
            }
            let results =
                process::SchedulingResults { policy: old.policy, priority: old.priority.into() };
            answer(label, process::SchedulingResults::WORDS, results.encode())
        }
        process::SET_AFFINITY => {
            let a = process::SetAffinityArgs::decode(args)?;
            let (thread, _, permitted) = thread_named(user, a.thread)?;
            if !permitted {
                return Err(error(Kernel::NotPermitted));
            }
            // Bits past the kernel's CPUs are ignored, as Linux ignores
            // those past its own.
            let mut bytes = [0u8; MAX_CPUS / 8];
            let len = (a.cpus.len as usize).min(bytes.len());
            if !radix_ipc::range::user_bytes(a.cpus.address, a.cpus.len)
                || !Arch::copy_from_user(&mut bytes[..len], a.cpus.address)
            {
                return Err(error(Kernel::BadAddress));
            }
            let words = core::array::from_fn(|i| {
                u64::from_le_bytes(bytes[i * 8..i * 8 + 8].try_into().unwrap_or([0; 8]))
            });
            sched::set_affinity(&thread, CpuSet::from_words(words))
                .map_err(|_| Kernel::InvalidArgument.status(1))?;
            answer(label, 0, [0; 8])
        }
        process::AFFINITY => {
            let a = process::AffinityArgs::decode(args)?;
            let (thread, _, _) = thread_named(user, a.thread)?;
            let size = (MAX_CPUS / 8) as u64;
            if a.cpus.len < size {
                return Err(Kernel::InvalidArgument.status(1));
            }
            let mut bytes = [0u8; MAX_CPUS / 8];
            for (chunk, word) in bytes.chunks_mut(8).zip(thread.cpu_mask().words()) {
                chunk.copy_from_slice(&word.to_le_bytes());
            }
            if !radix_ipc::range::user_bytes(a.cpus.address, size)
                || !Arch::copy_to_user(a.cpus.address, &bytes)
            {
                return Err(error(Kernel::BadAddress));
            }
            answer(
                label,
                process::AffinityResults::WORDS,
                process::AffinityResults { size }.encode(),
            )
        }
        process::IO_PRIORITY => {
            let a = process::IoPriorityArgs::decode(args)?;
            if a.level >= IO_LEVELS {
                return Err(Kernel::InvalidArgument.status(4));
            }
            let set = a.set.then_some((a.io_class, a.level));
            let (class, level) = io_priority(user, a.which, a.who, set)?;
            let results = process::IoPriorityResults { io_class: class, level };
            answer(label, process::IoPriorityResults::WORDS, results.encode())
        }
        _ => Err(error(Kernel::NoSuchMethod)),
    }
}

/// How urgently a process's I/O class and level are served: lower first
/// (`crate::fs::disk`). No class set is best effort at level 4.
pub fn io_rank(class: u32, level: u32) -> u32 {
    match IoClass::from_raw(class) {
        Some(IoClass::Realtime) => level,
        Some(IoClass::Idle) => 2 * IO_LEVELS,
        Some(IoClass::BestEffort) => IO_LEVELS + level,
        Some(IoClass::None) | None => IO_LEVELS + 4,
    }
}

/// `io_priority`: the most urgent class and level among the processes
/// `which` and `who` name, as they were, after setting them all to
/// `set`'s, if it is given.
fn io_priority(
    user: &UserRef,
    which: PriorityWhich,
    who: u64,
    set: Option<(IoClass, u32)>,
) -> Result<(IoClass, u32)> {
    let processes = processes_named(user, which, who)?;
    let mut most: Option<(u32, u32)> = None;
    let mut refused = false;
    for (process, permitted) in &processes {
        let (class, level) = process.io_priority();
        if most.is_none_or(|(c, l)| io_rank(class, level) < io_rank(c, l)) {
            most = Some((class, level));
        }
        match set {
            Some(_) if !permitted => refused = true,
            Some((class, level)) => process.set_io_priority(class.raw(), level),
            None => {}
        }
    }
    let (class, level) = most.ok_or(error(Kernel::NoSuchProcess))?;
    if refused {
        return Err(error(Kernel::NotPermitted));
    }
    Ok((IoClass::from_raw(class).unwrap_or(IoClass::None), level))
}

/// The live processes `which` and `who` name for `user`'s thread, as
/// `io_priority` says, and whether the caller may change each.
fn processes_named(
    user: &UserRef,
    which: PriorityWhich,
    who: u64,
) -> Result<Vec<(ProcessRef, bool), Kmalloc>> {
    let mut processes: Vec<(ProcessRef, bool), Kmalloc> = Vec::new_in(kmalloc());
    if which == PriorityWhich::Process {
        let (_, target, permitted) = thread_named(user, who)?;
        processes.try_reserve(1).map_err(|_| error(Kernel::NoMemory))?;
        let _ = processes.push_within_capacity((target.process.clone(), permitted));
        return Ok(processes);
    }
    // Every process is user 0's until there are users (Phase 13).
    if which == PriorityWhich::User && who != 0 {
        return Err(error(Kernel::NoSuchProcess));
    }
    let me = &user.process;
    with_tree(|tree| -> Result<()> {
        join(tree, me)?;
        let tree = &*tree;
        let group = match (which, who) {
            (PriorityWhich::Group, 0) => tree.get(me.number()).map_or(0, |m| m.group),
            (PriorityWhich::Group, group) => group,
            _ => 0,
        };
        for member in tree.members() {
            if matches!(member.life, Life::Zombie { .. })
                || (which == PriorityWhich::Group && member.group != group)
            {
                continue;
            }
            processes.try_reserve(1).map_err(|_| error(Kernel::NoMemory))?;
            let permitted = tree.may_signal(me.number(), member.pid, super::same_user);
            let _ = processes.push_within_capacity((member.handle.clone(), permitted));
        }
        Ok(())
    })?;
    Ok(processes)
}

/// The thread numbered `number` (0: the calling one, `user`'s), and
/// whether `user`'s process may change it: a thread of its own process,
/// the first thread of another process with that number, or another
/// thread of another process by its own number, as Linux's thread IDs.
fn thread_named(user: &UserRef, number: u64) -> Result<Named> {
    let me = &user.process;
    if number == 0 {
        let thread = sched::current().ok_or(error(Kernel::Gone))?;
        return Ok((thread, user.clone(), true));
    }
    if let Some((thread, target)) = find_thread(me, number) {
        return Ok((thread, target, true));
    }
    let process = with_tree(|tree| -> Result<Option<(ProcessRef, bool)>> {
        join(tree, me)?;
        let permitted = tree.may_signal(me.number(), number, super::same_user);
        Ok(find(tree, number).map(|process| (process, permitted)))
    })?;
    if let Some((process, permitted)) = process {
        let (thread, target) = find_thread(&process, number).ok_or(error(Kernel::NoSuchProcess))?;
        return Ok((thread, target, permitted));
    }
    for (process, permitted) in processes_named(user, PriorityWhich::User, 0)? {
        if let Some((thread, target)) = find_thread(&process, number) {
            return Ok((thread, target, permitted));
        }
    }
    Err(error(Kernel::NoSuchProcess))
}

/// The threads `which` and `who` name for `user`'s thread, as
/// `priority` says, and whether the caller may change each.
fn threads_named(user: &UserRef, which: PriorityWhich, who: u64) -> Result<Vec<Named, Kmalloc>> {
    let mut threads = Vec::new_in(kmalloc());
    if which == PriorityWhich::Process {
        threads.try_reserve(1).map_err(|_| error(Kernel::NoMemory))?;
        let _ = threads.push_within_capacity(thread_named(user, who)?);
        return Ok(threads);
    }
    let processes = processes_named(user, which, who)?;
    let mut failed = false;
    for (process, permitted) in &processes {
        process.for_each_thread(&mut |thread, target| {
            if threads.try_reserve(1).is_err() {
                failed = true;
                return;
            }
            let _ = threads.push_within_capacity((thread.clone(), target.clone(), *permitted));
        });
    }
    if failed {
        return Err(error(Kernel::NoMemory));
    }
    Ok(threads)
}

/// `priority`: the lowest nice level of the threads `which` and `who`
/// name, after setting them all to `set`'s, if it is given.
fn priority(user: &UserRef, which: PriorityWhich, who: u64, set: Option<i32>) -> Result<i32> {
    let threads = threads_named(user, which, who)?;
    if threads.is_empty() {
        return Err(error(Kernel::NoSuchProcess));
    }
    let mut lowest = NICE_MAX;
    let mut failure = None;
    for (thread, target, permitted) in &threads {
        let Some(nice) = set else {
            lowest = lowest.min(i32::from(target.scheduling.lock().nice));
            continue;
        };
        let nice = nice.clamp(NICE_MIN, NICE_MAX);
        if !permitted {
            failure = Some(Kernel::NotPermitted);
            continue;
        }
        // RLIMIT_NICE allows nice levels down to 20 less it.
        let limit = target.process.posix.lock().limits.get(LimitKind::Nice).current;
        let floor = 20 - limit.min(40) as i64;
        let mut scheduling = target.scheduling.lock();
        if nice < i32::from(scheduling.nice) && i64::from(nice) < floor {
            failure = Some(Kernel::OverLimit);
            continue;
        }
        scheduling.nice = nice as i8;
        // The level is in range, so the policy is valid.
        let _ = sched::set_policy(thread, scheduling.policy());
        lowest = lowest.min(nice);
    }
    match failure {
        Some(kind) => Err(error(kind)),
        None => Ok(lowest),
    }
}

/// `scheduling` with `set`: gives `thread`, `target`'s, `policy` at real-
/// time priority `priority`, as RLIMIT_RTPRIO and RLIMIT_NICE allow.
fn set_policy(
    thread: &ThreadRef,
    target: &UserRef,
    policy: SchedulingPolicy,
    priority: u32,
) -> Result<()> {
    let realtime = matches!(policy, SchedulingPolicy::Fifo | SchedulingPolicy::RoundRobin);
    let valid = if realtime { (1..=REALTIME_MAX).contains(&priority) } else { priority == 0 };
    if !valid {
        return Err(Kernel::InvalidArgument.status(3));
    }
    let (realtime_limit, nice_limit) = {
        let posix = target.process.posix.lock();
        (
            posix.limits.get(LimitKind::RealtimePriority).current,
            posix.limits.get(LimitKind::Nice).current,
        )
    };
    let mut scheduling = target.scheduling.lock();
    // Linux's rules for a thread without privileges: a real-time policy
    // needs RLIMIT_RTPRIO, and a priority above the thread's no higher
    // than it; leaving idle needs RLIMIT_NICE to allow the nice level.
    if realtime {
        let current = u32::from(scheduling.priority);
        if (policy != scheduling.policy && realtime_limit == 0)
            || (priority > current && u64::from(priority) > realtime_limit)
        {
            return Err(error(Kernel::NotPermitted));
        }
    }
    if scheduling.policy == SchedulingPolicy::Idle
        && policy != SchedulingPolicy::Idle
        && 20 - i64::from(scheduling.nice) > nice_limit.min(40) as i64
    {
        return Err(error(Kernel::NotPermitted));
    }
    scheduling.policy = policy;
    scheduling.priority = priority as u8;
    sched::set_policy(thread, scheduling.policy()).map_err(|_| Kernel::InvalidArgument.status(3))
}
