// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Threads: what the scheduler runs.
//!
//! A [`Thread`] is shared ([`ThreadRef`]): the registry holds every thread
//! until the reaper frees it, and run queues, wait queues and timers hold
//! it while it is queued on them. Its scheduling state ([`State`]) is under
//! its own lock, which is taken before any run-queue lock. What a run
//! queue needs while it holds only its own lock (which class the thread is
//! queued in, its weight, where it may run) is copied into atomics that
//! are written with both locks held.

use alloc::sync::Arc;
use core::fmt;
use core::sync::atomic::{self, AtomicBool, AtomicU8, AtomicU32, AtomicU64, AtomicUsize, Ordering};

use radix_hal::Threads;
use radix_lockdep::Saved;
use radix_mm::Kmalloc;
use radix_sched::cpuset::WORDS;
use radix_sched::{CpuSet, Split, nice_valid, rt_prio_valid, weight};
use radix_sync::{Once, RawSpinLock, SpinLock};

use crate::Arch;
use crate::domains::DomainRef;
use crate::user::UserRef;

/// A thread, shared.
pub type ThreadRef = Arc<Thread, Kmalloc>;

/// A thread's saved registers and stack.
pub(super) type Context = <Arch as Threads>::Context;

/// Longest thread name kept, in bytes.
pub const NAME_LEN: usize = 23;

/// A short name, for logs and the debugger.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct Name {
    bytes: [u8; NAME_LEN],
    len: u8,
}

impl Name {
    /// `name`, cut to [`NAME_LEN`] bytes at a character boundary.
    pub fn new(name: &str) -> Name {
        let mut len = name.len().min(NAME_LEN);
        while !name.is_char_boundary(len) {
            len -= 1;
        }
        let mut bytes = [0; NAME_LEN];
        bytes[..len].copy_from_slice(&name.as_bytes()[..len]);
        Name { bytes, len: len as u8 }
    }

    /// `prefix` followed by a number, such as `idle/3`.
    pub fn numbered(prefix: &str, number: usize) -> Name {
        let mut text = radix_klog::TextBuf::<NAME_LEN>::new();
        let _ = fmt::Write::write_fmt(&mut text, format_args!("{prefix}{number}"));
        Name::new(text.as_str())
    }

    /// The name.
    pub fn as_str(&self) -> &str {
        core::str::from_utf8(&self.bytes[..usize::from(self.len)]).unwrap_or("?")
    }
}

impl fmt::Display for Name {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.pad(self.as_str())
    }
}

/// The words a [`Name`] packs into: its bytes, then its length.
const NAME_WORDS: usize = (NAME_LEN + 1) / 8;
const _: () = assert!((NAME_LEN + 1).is_multiple_of(8), "a name is whole words");

/// A thread's name, which its program may change (Phase 8,
/// `pthread_setname_np`) while logs read it, perhaps from an interrupt:
/// three words with a sequence count around them, so that a reader sees
/// the name one write left. Writers take `writer`, which keeps them from
/// being preempted halfway; a reader that keeps meeting a write (one an
/// interrupt on its own CPU stopped) takes the words as they are, as
/// Linux's readers of a task's `comm` do. They still make a name: at
/// worst, a mixed one, or `?`.
struct NameCell {
    sequence: AtomicU32,
    words: [AtomicU64; NAME_WORDS],
    writer: SpinLock<()>,
}

impl NameCell {
    /// Tries a reader makes before it takes the words as they are.
    const TRIES: usize = 4;

    fn new(name: Name) -> NameCell {
        NameCell {
            sequence: AtomicU32::new(0),
            words: NameCell::pack(name).map(AtomicU64::new),
            writer: SpinLock::new(()),
        }
    }

    fn pack(name: Name) -> [u64; NAME_WORDS] {
        let mut bytes = [0u8; NAME_WORDS * 8];
        bytes[..NAME_LEN].copy_from_slice(&name.bytes);
        bytes[NAME_LEN] = name.len;
        let (chunks, _) = bytes.as_chunks::<8>();
        core::array::from_fn(|i| u64::from_ne_bytes(chunks[i]))
    }

    fn unpack(words: [u64; NAME_WORDS]) -> Name {
        let mut bytes = [0u8; NAME_WORDS * 8];
        for (chunk, word) in bytes.as_chunks_mut::<8>().0.iter_mut().zip(words) {
            chunk.copy_from_slice(&word.to_ne_bytes());
        }
        let mut name = Name { bytes: [0; NAME_LEN], len: bytes[NAME_LEN].min(NAME_LEN as u8) };
        name.bytes.copy_from_slice(&bytes[..NAME_LEN]);
        name
    }

    fn get(&self) -> Name {
        let mut words = [0; NAME_WORDS];
        for _ in 0..NameCell::TRIES {
            let before = self.sequence.load(Ordering::Acquire);
            for (word, cell) in words.iter_mut().zip(&self.words) {
                *word = cell.load(Ordering::Relaxed);
            }
            atomic::fence(Ordering::Acquire);
            if before.is_multiple_of(2) && self.sequence.load(Ordering::Relaxed) == before {
                break;
            }
        }
        NameCell::unpack(words)
    }

    fn set(&self, name: Name) {
        let _writer = self.writer.lock();
        let sequence = self.sequence.load(Ordering::Relaxed);
        self.sequence.store(sequence.wrapping_add(1), Ordering::Relaxed);
        atomic::fence(Ordering::Release);
        for (cell, word) in self.words.iter().zip(NameCell::pack(name)) {
            cell.store(word, Ordering::Relaxed);
        }
        self.sequence.store(sequence.wrapping_add(2), Ordering::Release);
    }
}

/// How a thread is scheduled.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Policy {
    /// The fair class, at a nice level from -20 (the largest share) to 19.
    Fair {
        /// The nice level.
        nice: i8,
    },
    /// Real-time, first in first out, at priority 1 to 99 (99 the most
    /// urgent): runs until it blocks or yields, or a more urgent thread
    /// wants its CPU.
    Fifo {
        /// The priority.
        prio: u8,
    },
    /// Real-time round robin: like [`Policy::Fifo`], but after
    /// `RR_QUANTUM_NS` of running it goes behind the other threads of its
    /// priority.
    RoundRobin {
        /// The priority.
        prio: u8,
    },
}

impl Policy {
    /// The policy threads start with: fair, nice 0.
    pub const DEFAULT: Policy = Policy::Fair { nice: 0 };

    /// Whether the nice level or priority is in range.
    pub fn is_valid(self) -> bool {
        match self {
            Policy::Fair { nice } => nice_valid(nice),
            Policy::Fifo { prio } | Policy::RoundRobin { prio } => rt_prio_valid(prio),
        }
    }

    /// The real-time priority, 0 for the fair class.
    pub fn rt_prio(self) -> u8 {
        match self {
            Policy::Fair { .. } => 0,
            Policy::Fifo { prio } | Policy::RoundRobin { prio } => prio,
        }
    }

    /// The fair-class weight of its nice level (that of nice 0 for
    /// real-time threads, which use it only while not real-time).
    pub fn weight(self) -> u64 {
        match self {
            Policy::Fair { nice } => weight(nice),
            _ => weight(0),
        }
    }
}

impl fmt::Display for Policy {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Policy::Fair { nice } => write!(f, "fair, nice {nice}"),
            Policy::Fifo { prio } => write!(f, "FIFO {prio}"),
            Policy::RoundRobin { prio } => write!(f, "round robin {prio}"),
        }
    }
}

/// Where a thread is in its life.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Run {
    /// Made, not yet started.
    New,
    /// On a run queue: running, or waiting for its turn.
    Ready,
    /// Asleep, for the reason given, until something wakes it.
    Blocked(&'static str),
    /// Exited; the reaper frees it once its CPU has switched away.
    Dead,
}

impl fmt::Display for Run {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Run::New => f.write_str("new"),
            Run::Ready => f.write_str("ready"),
            Run::Blocked(reason) => write!(f, "blocked ({reason})"),
            Run::Dead => f.write_str("dead"),
        }
    }
}

/// A thread's scheduling state, under its lock.
pub(super) struct State {
    pub(super) run: Run,
    pub(super) policy: Policy,
    /// The real-time priority lent by the most urgent thread waiting for a
    /// mutex it holds (priority inheritance); 0 for none.
    pub(super) boost: u8,
    pub(super) affinity: CpuSet,
    /// The service the fair class owes it (its lag, in nanoseconds of
    /// running at nice 0), kept while it is off the fair queues.
    pub(super) lag_ns: i64,
}

impl State {
    /// The real-time priority it runs at: its own or a lent one, whichever
    /// is higher; 0 for the fair class.
    pub(super) fn rt_prio(&self) -> u8 {
        self.policy.rt_prio().max(self.boost)
    }
}

/// Which queue a thread is on, in [`Thread::queued`].
pub(super) const NOT_QUEUED: u8 = 0;
/// On a fair queue.
pub(super) const FAIR: u8 = 1;
/// On a real-time queue.
pub(super) const REAL_TIME: u8 = 2;

/// Counters for the debugger, the self-tests and `/proc`.
#[derive(Default)]
pub(super) struct Stats {
    pub(super) runtime_ns: AtomicU64,
    pub(super) switches: AtomicU64,
    pub(super) migrations: AtomicU64,
    pub(super) wakeups: AtomicU64,
    pub(super) last_ran_ns: AtomicU64,
    /// The timer's samples of it running in user mode and in the kernel
    /// (`radix_sched::account`).
    pub(super) user_ticks: AtomicU64,
    pub(super) system_ticks: AtomicU64,
    /// The times it gave the CPU up to wait, and the times it was made to
    /// (preempted, or yielding).
    pub(super) voluntary: AtomicU64,
    pub(super) involuntary: AtomicU64,
}

/// A kernel thread.
pub struct Thread {
    id: u64,
    name: NameCell,
    pub(super) context: Context,
    pub(super) entry: fn(usize),
    pub(super) arg: usize,
    pub(super) state: SpinLock<State>,
    /// The CPU whose run queue it is on, or last ran on.
    pub(super) cpu: AtomicUsize,
    /// Set by [`super::wake`]; a thread that blocks with it set does not
    /// sleep, so a wake that comes before the block is not lost.
    pub(super) wake_pending: AtomicBool,
    stop: AtomicBool,
    /// Set while it has a signal to take, or a stop to make for job
    /// control, on its way back to user mode (Phase 8): with `stop`, what
    /// the return path and interruptible waits look at
    /// ([`Thread::interrupted`]).
    signalled: AtomicBool,
    domain: DomainRef,
    /// Stack pages charged to the domain, given back when it is freed.
    pub(super) charged: u64,
    /// Mutexes the lock checker saw it holding when it was switched out.
    /// Switching threads takes this lock, so anyone else takes it with
    /// interrupts off.
    pub(super) lockdep: RawSpinLock<Saved>,
    // The rest is written under its run queue's lock (and, but for the
    // balancer's moves, its own).
    /// [`NOT_QUEUED`], [`FAIR`] or [`REAL_TIME`].
    pub(super) queued: AtomicU8,
    /// The real-time priority it is queued at.
    pub(super) queued_prio: AtomicU8,
    /// Whether it is round robin, while queued real-time.
    pub(super) round_robin: AtomicBool,
    /// Its nice level's weight.
    pub(super) weight: AtomicU64,
    /// Where it may run.
    allowed: [AtomicU64; WORDS],
    /// What is left of its round-robin quantum.
    pub(super) quantum_left_ns: AtomicU64,
    /// The mutex it waits for, by key, for priority inheritance.
    pub(super) blocked_on: AtomicUsize,
    /// Set once the reaper is done with it.
    pub(super) reaped: AtomicBool,
    /// Set once it no longer counts among its domain's threads.
    left: AtomicBool,
    pub(super) stats: Stats,
    /// Its CPU time as [`Thread::cpu_times`] last split it.
    split: SpinLock<Split>,
    /// What a thread of a program has besides (Phase 5): set before it
    /// first runs, never for kernel threads.
    pub(super) user: Once<UserRef>,
}

impl Thread {
    /// A thread that is not yet running, from its parts.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn new(
        id: u64,
        name: Name,
        context: Context,
        entry: fn(usize),
        arg: usize,
        policy: Policy,
        affinity: CpuSet,
        domain: DomainRef,
        charged: u64,
    ) -> Thread {
        Thread {
            id,
            name: NameCell::new(name),
            context,
            entry,
            arg,
            state: SpinLock::new(State { run: Run::New, policy, boost: 0, affinity, lag_ns: 0 }),
            cpu: AtomicUsize::new(affinity.lowest().unwrap_or(0)),
            wake_pending: AtomicBool::new(false),
            stop: AtomicBool::new(false),
            signalled: AtomicBool::new(false),
            domain,
            charged,
            lockdep: RawSpinLock::new(Saved::new()),
            queued: AtomicU8::new(NOT_QUEUED),
            queued_prio: AtomicU8::new(0),
            round_robin: AtomicBool::new(false),
            weight: AtomicU64::new(policy.weight()),
            allowed: affinity.words().map(AtomicU64::new),
            quantum_left_ns: AtomicU64::new(radix_sched::RR_QUANTUM_NS),
            blocked_on: AtomicUsize::new(0),
            reaped: AtomicBool::new(false),
            left: AtomicBool::new(false),
            stats: Stats::default(),
            split: SpinLock::new(Split::new()),
            user: Once::new(),
        }
    }

    /// Its number, from the count processes take theirs from. A
    /// process's first thread has the process's number, and so does the
    /// thread that runs a program `exec` gave the process, while the
    /// thread that called `exec` ends; no other two threads share one.
    pub fn id(&self) -> u64 {
        self.id
    }

    /// Its program's side, if it is a thread of a program.
    pub fn user(&self) -> Option<&UserRef> {
        self.user.get()
    }

    /// Sets its program's thread pointer (Phase 5): at once if it runs
    /// on this CPU, otherwise from its next switch in.
    pub fn set_thread_pointer(&self, base: u64) {
        <crate::Arch as radix_hal::UserMode>::set_thread_pointer(&self.context, base);
    }

    /// Gives it, before it first runs, the running thread's extended state
    /// (Phase 8): a forked thread goes on with its parent's x87 and vector
    /// registers.
    pub fn copy_extended_state(&self) {
        <crate::Arch as radix_hal::UserMode>::copy_extended_state(&self.context);
    }

    /// Its name.
    pub fn name(&self) -> Name {
        self.name.get()
    }

    /// Renames it (Phase 8, radix-process(2)'s `set_thread_name`).
    pub fn set_name(&self, name: Name) {
        self.name.set(name);
    }

    /// The resource domain it is charged to.
    pub fn domain(&self) -> &DomainRef {
        &self.domain
    }

    /// The token that names it in mutex words: its address, which is
    /// non-zero and even.
    pub fn token(&self) -> usize {
        self as *const Thread as usize
    }

    /// Where it is in its life: ready, blocked and why, or dead.
    pub fn run(&self) -> Run {
        self.state.lock_irq().run
    }

    /// The CPU it runs on, or last ran or queued on.
    pub fn cpu(&self) -> usize {
        self.cpu.load(Ordering::Relaxed)
    }

    /// Nanoseconds it has run.
    pub fn runtime_ns(&self) -> u64 {
        self.stats.runtime_ns.load(Ordering::Relaxed)
    }

    /// Its CPU time split between user mode and the kernel, as the
    /// timer's samples share it, neither part ever less than it was
    /// ([`Split`]).
    pub fn cpu_times(&self) -> (u64, u64) {
        let (user, system) = self.ticks();
        self.split.lock().update(self.runtime_ns(), user, system)
    }

    /// The timer's samples of it in user mode and in the kernel.
    pub fn ticks(&self) -> (u64, u64) {
        (
            self.stats.user_ticks.load(Ordering::Relaxed),
            self.stats.system_ticks.load(Ordering::Relaxed),
        )
    }

    /// The times it gave up the CPU to wait, and the times it was made to.
    pub fn switch_counts(&self) -> (u64, u64) {
        (
            self.stats.voluntary.load(Ordering::Relaxed),
            self.stats.involuntary.load(Ordering::Relaxed),
        )
    }

    /// How many times it was switched to.
    pub fn switches(&self) -> u64 {
        self.stats.switches.load(Ordering::Relaxed)
    }

    /// How many times it moved to another CPU.
    pub fn migrations(&self) -> u64 {
        self.stats.migrations.load(Ordering::Relaxed)
    }

    /// Asks it to stop: it sees [`Thread::should_stop`] and exits. Kernel
    /// threads check for it where they can stop cleanly; threads of user
    /// programs are stopped at their next entry into the kernel or return
    /// from an interrupt. The out-of-memory policy and a process's end use
    /// it.
    pub fn request_stop(&self) {
        self.stop.store(true, Ordering::Release);
    }

    /// Whether it has been asked to stop.
    pub fn should_stop(&self) -> bool {
        self.stop.load(Ordering::Acquire)
    }

    /// Marks that it has a signal to take (or a stop to make for job
    /// control) on its way back to user mode, or clears that. Whoever sets
    /// it then wakes the thread ([`super::wake`]), so that a wait it is in
    /// ends as interrupted.
    pub fn set_signalled(&self, signalled: bool) {
        self.signalled.store(signalled, Ordering::Release);
    }

    /// Whether it has a signal to take ([`Thread::set_signalled`]).
    pub fn is_signalled(&self) -> bool {
        self.signalled.load(Ordering::Acquire)
    }

    /// Whether a wait it is in should end early: it has been asked to stop,
    /// or has a signal to take.
    pub fn interrupted(&self) -> bool {
        self.should_stop() || self.is_signalled()
    }

    /// Stops counting it among its domain's threads: when it exits, so a
    /// domain's thread count falls although others still hold references.
    pub(super) fn leave_domain(&self) {
        if !self.left.swap(true, Ordering::AcqRel) && self.domain.thread_left() {
            // Its last thread: wake whoever waits for the domain to empty.
            super::wait::wake_all(self.domain.key());
        }
    }

    /// Whether the reaper has finished with it: it has exited and no CPU
    /// runs it any more.
    pub fn is_reaped(&self) -> bool {
        self.reaped.load(Ordering::Acquire)
    }

    /// Its scheduling state, for reports. Gives up if the lock is held.
    pub fn info(&self) -> Option<ThreadInfo> {
        let state = self.state.try_lock_irq()?;
        Some(ThreadInfo {
            run: state.run,
            policy: state.policy,
            boost: state.boost,
            affinity: state.affinity,
        })
    }

    /// The CPUs it may run on, as `set_affinity` last set them.
    pub fn cpu_mask(&self) -> CpuSet {
        self.allowed()
    }

    /// Where it may run, from the copy a run queue reads.
    pub(super) fn allowed(&self) -> CpuSet {
        CpuSet::from_words(self.allowed.each_ref().map(|w| w.load(Ordering::Relaxed)))
    }

    /// Updates that copy; with its lock held.
    pub(super) fn set_allowed(&self, set: &CpuSet) {
        for (word, value) in self.allowed.iter().zip(set.words()) {
            word.store(value, Ordering::Relaxed);
        }
    }
}

/// A snapshot of a thread's scheduling state.
#[derive(Clone, Copy, Debug)]
pub struct ThreadInfo {
    /// Where it is in its life.
    pub run: Run,
    /// Its own policy.
    pub policy: Policy,
    /// A lent real-time priority, 0 for none.
    pub boost: u8,
    /// Where it may run.
    pub affinity: CpuSet,
}

impl Drop for Thread {
    fn drop(&mut self) {
        self.domain.uncharge(self.charged);
        self.leave_domain();
    }
}
