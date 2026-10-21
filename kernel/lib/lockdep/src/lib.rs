// SPDX-FileCopyrightText: 2026 The OrthrOS contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! The Radix lock-order checker, after Linux's lockdep.
//!
//! A deadlock between locks needs two pieces of code that take the same
//! locks in opposite orders, and bad luck in how they interleave. The bad
//! luck may take years; the opposite orders are visible on the first run.
//! The checker watches every [`radix_sync::SpinLock`] and
//! [`radix_sync::Mutex`] acquisition (through the hooks the kernel
//! registers) and keeps a graph of which lock *classes* have been taken
//! while holding which. A class is every lock created by one line of code
//! ([`LockSite`]), plus the subclass `lock_nested` names. Taking a lock
//! that would close a cycle in the graph is reported at once, with the
//! places where both orders were seen, whether or not the two ever race.
//!
//! It also reports, as [`Problem`]s:
//!
//! - taking a lock of a class already held, which deadlocks at once for
//!   the same lock and is suspicious for two locks of one class
//!   ([`Problem::Recursion`]);
//! - a lock taken in an interrupt handler that is also taken elsewhere with
//!   interrupts on, which deadlocks when the interrupt arrives on the CPU
//!   holding it ([`Problem::IrqUnsafe`]), and the same through a chain of
//!   other locks ([`Problem::IrqInversion`]);
//! - a mutex taken, or a thread switched out, where sleeping is not
//!   allowed: holding a spin lock, with interrupts off or in an interrupt
//!   handler ([`Problem::SleepInAtomic`]).
//!
//! Edges are recorded only between locks taken in the same context: an
//! interrupt handler's locks have nothing to do with the locks of the
//! thread it interrupted, except through the interrupt rules above.
//!
//! Each CPU has a stack of the locks it holds. Spin locks never outlive a
//! thread's time on the CPU, but mutexes do: the scheduler moves them into
//! the thread with [`Checker::switch_out`] and back with
//! [`Checker::switch_in`].
//!
//! Everything lives in fixed tables (512 classes, 32 locks held per CPU),
//! so the checker never allocates. Running out of room is reported once
//! and turns the checker off, as does the first real report: after one
//! deadlock report the graph may be inconsistent, and a flood of follow-on
//! reports would hide the first. A self-test can ask it to keep going with
//! [`Checker::expect_reports`].
//!
//! This crate is safe code. It runs on the host, where its tests drive it
//! directly.

#![cfg_attr(not(test), no_std)]
#![forbid(unsafe_code)]

use core::fmt;
use core::panic::Location;
use core::sync::atomic::{AtomicBool, AtomicU8, AtomicU32, AtomicU64, Ordering};

use radix_sync::{Acquire, LockKind, LockSite, Once, RawSpinLock};

/// Most lock classes the checker tells apart.
pub const MAX_CLASSES: usize = 512;
/// Most locks one CPU can hold at once (counting interrupt handlers').
pub const MAX_HELD: usize = 32;
/// Most mutexes a thread can hold while it is switched out.
pub const MAX_SAVED: usize = 8;
/// Deepest `lock_nested` subclass.
pub const MAX_SUBCLASS: u8 = 7;

const WORDS: usize = MAX_CLASSES / 64;
/// Edges whose first sighting is remembered for reports.
const EDGE_RECORDS: usize = 1024;

/// Usage bit: taken in an interrupt handler.
const USED_IN_IRQ: u8 = 1;
/// Usage bit: taken outside interrupt handlers with interrupts on.
const ENABLED_IRQ: u8 = 2;

/// What the checker found.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum Problem {
    /// Two locks are taken in both orders (perhaps through other locks).
    Inversion = 1,
    /// A lock of a class already held is taken.
    Recursion = 2,
    /// A lock is taken both in an interrupt handler and with interrupts on.
    IrqUnsafe = 3,
    /// A lock taken in interrupt handlers is held while taking one that is
    /// taken elsewhere with interrupts on.
    IrqInversion = 4,
    /// A mutex is taken, or a thread switched out, where it cannot sleep.
    SleepInAtomic = 5,
    /// The checker's tables are full; it turned itself off.
    OutOfRoom = 6,
}

impl Problem {
    fn from_u8(value: u8) -> Option<Problem> {
        Some(match value {
            1 => Problem::Inversion,
            2 => Problem::Recursion,
            3 => Problem::IrqUnsafe,
            4 => Problem::IrqInversion,
            5 => Problem::SleepInAtomic,
            6 => Problem::OutOfRoom,
            _ => return None,
        })
    }
}

/// Where the code taking a lock runs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Context {
    /// The CPU.
    pub cpu: usize,
    /// How many interrupt handlers deep (0 for thread context).
    pub irq_depth: u8,
    /// Whether interrupts were on when the lock was taken.
    pub irqs_enabled: bool,
}

/// One lock in a report.
#[derive(Clone, Copy, Debug)]
pub struct LockRef {
    /// Where its class was created.
    pub created: &'static Location<'static>,
    /// Its `lock_nested` subclass.
    pub subclass: u8,
    /// Spin lock or mutex.
    pub kind: LockKind,
    /// Where it was taken.
    pub at: &'static Location<'static>,
}

impl fmt::Display for LockRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let kind = match self.kind {
            LockKind::Spin => "spin lock",
            LockKind::Mutex => "mutex",
        };
        write!(f, "{kind} created at {}", self.created)?;
        if self.subclass != 0 {
            write!(f, " (subclass {})", self.subclass)?;
        }
        write!(f, ", taken at {}", self.at)
    }
}

/// A problem the checker found, with the locks involved.
#[derive(Clone, Copy, Debug)]
pub struct Report {
    /// What is wrong.
    pub problem: Problem,
    /// The CPU it was found on.
    pub cpu: usize,
    /// The lock being taken, if one is.
    pub taking: Option<LockRef>,
    /// The held lock involved, if one is.
    pub held: Option<LockRef>,
    /// Where the conflicting use was seen before: for an inversion, where
    /// the two locks were taken in the other order (first, then second);
    /// for an interrupt problem, the interrupt-handler use and the use
    /// with interrupts on.
    pub earlier: Option<(&'static Location<'static>, &'static Location<'static>)>,
    /// For an inversion through other locks, how many locks the earlier
    /// path has in between.
    pub between: usize,
    /// For [`Problem::OutOfRoom`], which table filled up.
    pub detail: &'static str,
    /// Whether the checker turned itself off after this report.
    pub turned_off: bool,
}

impl Report {
    /// Writes the report, one line per call of `line`.
    pub fn write_lines(&self, line: &mut dyn FnMut(fmt::Arguments<'_>)) {
        let headline = match self.problem {
            Problem::Inversion => "possible deadlock: two locks are taken in both orders",
            Problem::Recursion => {
                "possible deadlock: a lock is taken while one of its class is held"
            }
            Problem::IrqUnsafe => {
                "possible deadlock: a lock is taken in interrupt handlers and with interrupts on"
            }
            Problem::IrqInversion => {
                "possible deadlock: an interrupt-handler lock is held while taking a lock that is \
                 also taken with interrupts on"
            }
            Problem::SleepInAtomic => "sleeping where it is not allowed",
            Problem::OutOfRoom => "out of room",
        };
        line(format_args!("Lock checker: {headline} (CPU {})", self.cpu));
        if let Some(taking) = &self.taking {
            line(format_args!("Lock checker:   taking {taking}"));
        }
        if let Some(held) = &self.held {
            line(format_args!("Lock checker:   while holding {held}"));
        }
        match (self.problem, self.earlier) {
            (Problem::Inversion, Some((first, second))) => {
                if self.between == 0 {
                    line(format_args!(
                        "Lock checker:   the other order was seen before: the lock being taken \
                         now at {first}, then the held one at {second}"
                    ));
                } else {
                    line(format_args!(
                        "Lock checker:   the other order was seen before, through {} other \
                         lock{}: starting at {first}, then {second}",
                        self.between,
                        if self.between == 1 { "" } else { "s" }
                    ));
                }
            }
            (Problem::IrqUnsafe | Problem::IrqInversion, Some((in_irq, enabled))) => {
                line(format_args!(
                    "Lock checker:   taken in an interrupt handler at {in_irq}, and with \
                     interrupts on at {enabled}"
                ));
            }
            _ => {}
        }
        if self.problem == Problem::OutOfRoom {
            line(format_args!("Lock checker:   {}", self.detail));
        }
        if self.turned_off {
            line(format_args!("Lock checker: turned off after this report"));
        }
    }
}

/// A lock held by a CPU or a switched-out thread.
#[derive(Clone, Copy, Debug)]
struct HeldLock {
    class: u16,
    subclass: u8,
    context: u8,
    kind: LockKind,
    addr: usize,
    at: &'static Location<'static>,
}

struct Held {
    locks: [Option<HeldLock>; MAX_HELD],
    len: usize,
}

impl Held {
    const fn new() -> Held {
        Held { locks: [None; MAX_HELD], len: 0 }
    }

    fn iter(&self) -> impl DoubleEndedIterator<Item = &HeldLock> {
        self.locks[..self.len].iter().flatten()
    }

    fn push(&mut self, lock: HeldLock) -> bool {
        match self.locks.get_mut(self.len) {
            Some(slot) => {
                *slot = Some(lock);
                self.len += 1;
                true
            }
            None => false,
        }
    }

    fn remove(&mut self, index: usize) -> Option<HeldLock> {
        let lock = self.locks.get_mut(index)?.take();
        self.locks[index..self.len].rotate_left(1);
        self.len -= 1;
        lock
    }
}

/// The mutexes a switched-out thread holds; the scheduler keeps one per
/// thread.
pub struct Saved {
    locks: [Option<HeldLock>; MAX_SAVED],
    len: usize,
}

impl Saved {
    /// Holds nothing.
    pub const fn new() -> Saved {
        Saved { locks: [None; MAX_SAVED], len: 0 }
    }

    /// How many locks it holds.
    pub fn len(&self) -> usize {
        self.len
    }

    /// Whether it holds none.
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }
}

impl Default for Saved {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Clone, Copy)]
struct ClassInfo {
    created: &'static Location<'static>,
    subclass: u8,
    kind: LockKind,
    /// The first acquisition in an interrupt handler.
    irq_use: Option<&'static Location<'static>>,
    /// The first acquisition with interrupts on.
    enabled_use: Option<&'static Location<'static>>,
}

struct ClassTable {
    info: [Option<ClassInfo>; MAX_CLASSES],
    len: usize,
}

#[derive(Clone, Copy)]
struct EdgeRecord {
    from: u16,
    to: u16,
    from_at: &'static Location<'static>,
    to_at: &'static Location<'static>,
}

struct EdgeLog {
    records: [Option<EdgeRecord>; EDGE_RECORDS],
    len: usize,
    edges: usize,
}

impl EdgeLog {
    fn find(&self, from: u16, to: u16) -> Option<EdgeRecord> {
        self.records[..self.len].iter().flatten().find(|r| r.from == from && r.to == to).copied()
    }
}

/// A set of classes.
type ClassSet = [u64; WORDS];

/// Counts for the debugger.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Stats {
    /// Whether the checker is on.
    pub enabled: bool,
    /// Classes seen.
    pub classes: usize,
    /// Distinct lock orders seen.
    pub edges: usize,
    /// Reports made.
    pub reports: u32,
}

/// One class, for the debugger.
#[derive(Clone, Copy, Debug)]
pub struct ClassView {
    /// Its number.
    pub id: usize,
    /// Where its locks are created.
    pub created: &'static Location<'static>,
    /// Its subclass.
    pub subclass: u8,
    /// Spin lock or mutex.
    pub kind: LockKind,
    /// Taken in interrupt handlers.
    pub in_irq: bool,
    /// Taken with interrupts on.
    pub irqs_on: bool,
    /// Classes taken while holding this one.
    pub after: usize,
}

/// One held lock, for the debugger.
#[derive(Clone, Copy, Debug)]
pub struct HeldView {
    /// Its class's creation site.
    pub created: &'static Location<'static>,
    /// Where it was taken.
    pub at: &'static Location<'static>,
    /// Spin lock or mutex.
    pub kind: LockKind,
    /// Interrupt depth it was taken at.
    pub context: u8,
}

/// Clears the per-CPU reentrancy flag when the checker is done.
struct Busy<'a>(&'a AtomicBool);

impl Drop for Busy<'_> {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Release);
    }
}

/// The checker, for a machine of up to `CPUS` CPUs.
pub struct Checker<const CPUS: usize> {
    enabled: AtomicBool,
    expecting: AtomicBool,
    reports: AtomicU32,
    last_problem: AtomicU8,
    reporter: Once<fn(&Report)>,
    classes: RawSpinLock<ClassTable>,
    /// Per class: the class of the next subclass, plus one.
    nested: [AtomicU32; MAX_CLASSES],
    usage: [AtomicU8; MAX_CLASSES],
    /// Bit `b` of row `a`: `b` was taken while holding `a`.
    forward: [[AtomicU64; WORDS]; MAX_CLASSES],
    /// The same edges, transposed.
    backward: [[AtomicU64; WORDS]; MAX_CLASSES],
    /// Serialises adding edges.
    graph: RawSpinLock<EdgeLog>,
    held: [RawSpinLock<Held>; CPUS],
    busy: [AtomicBool; CPUS],
}

impl<const CPUS: usize> Checker<CPUS> {
    /// A checker that is off and has seen nothing.
    pub const fn new() -> Self {
        Self {
            enabled: AtomicBool::new(false),
            expecting: AtomicBool::new(false),
            reports: AtomicU32::new(0),
            last_problem: AtomicU8::new(0),
            reporter: Once::new(),
            classes: RawSpinLock::new(ClassTable { info: [None; MAX_CLASSES], len: 0 }),
            nested: [const { AtomicU32::new(0) }; MAX_CLASSES],
            usage: [const { AtomicU8::new(0) }; MAX_CLASSES],
            forward: [const { [const { AtomicU64::new(0) }; WORDS] }; MAX_CLASSES],
            backward: [const { [const { AtomicU64::new(0) }; WORDS] }; MAX_CLASSES],
            graph: RawSpinLock::new(EdgeLog { records: [None; EDGE_RECORDS], len: 0, edges: 0 }),
            held: [const { RawSpinLock::new(Held::new()) }; CPUS],
            busy: [const { AtomicBool::new(false) }; CPUS],
        }
    }

    /// Sets the function reports go to (once).
    pub fn set_reporter(&self, reporter: fn(&Report)) {
        let _ = self.reporter.set(reporter);
    }

    /// Turns the checker on or off. Turn it on only while no CPU holds a
    /// lock, as the kernel does at boot.
    pub fn set_enabled(&self, on: bool) {
        self.enabled.store(on, Ordering::SeqCst);
    }

    /// Whether the checker is on.
    pub fn enabled(&self) -> bool {
        self.enabled.load(Ordering::Relaxed)
    }

    /// While `expect` is set, reports do not turn the checker off: for the
    /// self-test that seeds problems on purpose.
    pub fn expect_reports(&self, expect: bool) {
        self.expecting.store(expect, Ordering::SeqCst);
    }

    /// How many reports have been made.
    pub fn reports(&self) -> u32 {
        self.reports.load(Ordering::SeqCst)
    }

    /// The problem of the latest report.
    pub fn last_problem(&self) -> Option<Problem> {
        Problem::from_u8(self.last_problem.load(Ordering::SeqCst))
    }

    /// Counts for the debugger.
    pub fn stats(&self) -> Stats {
        let classes = self.classes.try_lock().map_or(0, |t| t.len);
        let edges = self.graph.try_lock().map_or(0, |g| g.edges);
        Stats { enabled: self.enabled(), classes, edges, reports: self.reports() }
    }

    /// Calls `f` for every class seen. Skips them all if another CPU is
    /// registering a class right now.
    pub fn classes(&self, mut f: impl FnMut(&ClassView)) {
        let Some(table) = self.classes.try_lock() else { return };
        for (id, info) in table.info[..table.len].iter().enumerate() {
            let Some(info) = info else { continue };
            let usage = self.usage[id].load(Ordering::Relaxed);
            let after = self.forward[id]
                .iter()
                .map(|w| w.load(Ordering::Relaxed).count_ones())
                .sum::<u32>();
            f(&ClassView {
                id,
                created: info.created,
                subclass: info.subclass,
                kind: info.kind,
                in_irq: usage & USED_IN_IRQ != 0,
                irqs_on: usage & ENABLED_IRQ != 0,
                after: after as usize,
            });
        }
    }

    /// Calls `f` for every lock CPU `cpu` holds, outermost first. Skips
    /// them if that CPU is inside the checker right now.
    pub fn held(&self, cpu: usize, mut f: impl FnMut(&HeldView)) {
        let Some(held) = self.held.get(cpu).and_then(RawSpinLock::try_lock) else { return };
        let Some(table) = self.classes.try_lock() else { return };
        for lock in held.iter() {
            if let Some(info) = table.info[usize::from(lock.class)] {
                f(&HeldView {
                    created: info.created,
                    at: lock.at,
                    kind: lock.kind,
                    context: lock.context,
                });
            }
        }
    }

    /// A lock is about to be waited for, or a `try_lock` got it. Call with
    /// interrupts masked (`ctx` says whether they were on before).
    pub fn acquire(&self, ctx: Context, lock: &Acquire<'_>) {
        if !self.enabled() {
            return;
        }
        let Some(_busy) = self.enter(ctx.cpu) else { return };
        let Some(class) = self.class_of(lock.site, lock.subclass, lock.kind) else {
            self.out_of_room(ctx.cpu, "more than 512 lock classes");
            return;
        };
        let taking = HeldLock {
            class,
            subclass: lock.subclass,
            context: ctx.irq_depth,
            kind: lock.kind,
            addr: lock.addr,
            at: lock.at,
        };
        let mut held = self.held[ctx.cpu].lock();
        self.mark_usage(ctx, &taking);

        if lock.kind == LockKind::Mutex && !lock.trylock {
            let spin =
                held.iter().rev().find(|h| h.context == ctx.irq_depth && h.kind == LockKind::Spin);
            if spin.is_some() || ctx.irq_depth > 0 || !ctx.irqs_enabled {
                let held = spin.map(|h| self.lock_ref(h));
                self.report(Report {
                    problem: Problem::SleepInAtomic,
                    cpu: ctx.cpu,
                    taking: Some(self.lock_ref(&taking)),
                    held,
                    earlier: None,
                    between: 0,
                    detail: "",
                    turned_off: false,
                });
            }
        }

        for prev in held.iter().rev().filter(|h| h.context == ctx.irq_depth) {
            if prev.class == class {
                if !lock.trylock {
                    self.report(Report {
                        problem: Problem::Recursion,
                        cpu: ctx.cpu,
                        taking: Some(self.lock_ref(&taking)),
                        held: Some(self.lock_ref(prev)),
                        earlier: None,
                        between: 0,
                        detail: "",
                        turned_off: false,
                    });
                }
            } else if !lock.trylock {
                self.add_edge(ctx.cpu, prev, &taking);
            }
        }

        if !held.push(taking) {
            drop(held);
            self.out_of_room(ctx.cpu, "more than 32 locks held at once on one CPU");
        }
    }

    /// The lock at `addr` was let go. Call with interrupts masked.
    pub fn release(&self, cpu: usize, addr: usize) {
        if !self.enabled() {
            return;
        }
        let Some(_busy) = self.enter(cpu) else { return };
        let mut held = self.held[cpu].lock();
        // Locks taken before the checker was on, or while it was busy
        // reporting, were never recorded.
        if let Some(index) =
            held.locks[..held.len].iter().rposition(|h| h.is_some_and(|h| h.addr == addr))
        {
            held.remove(index);
        }
    }

    /// The running thread on `cpu` is about to be switched out: moves the
    /// locks it holds into `saved`, and reports any spin lock among them
    /// (it may not sleep or be preempted holding one). Call with interrupts
    /// masked.
    pub fn switch_out(&self, cpu: usize, saved: &mut Saved) {
        if !self.enabled() {
            return;
        }
        let Some(_busy) = self.enter(cpu) else { return };
        let mut held = self.held[cpu].lock();
        let mut index = 0;
        let mut spin = None;
        while index < held.len {
            match held.locks[index] {
                Some(lock) if lock.context == 0 => {
                    held.remove(index);
                    if lock.kind == LockKind::Spin {
                        spin.get_or_insert(lock);
                    } else if let Some(slot) = saved.locks.get_mut(saved.len) {
                        *slot = Some(lock);
                        saved.len += 1;
                    }
                }
                _ => index += 1,
            }
        }
        drop(held);
        if let Some(lock) = spin {
            self.report(Report {
                problem: Problem::SleepInAtomic,
                cpu,
                taking: None,
                held: Some(self.lock_ref(&lock)),
                earlier: None,
                between: 0,
                detail: "",
                turned_off: false,
            });
        }
    }

    /// The thread whose locks are in `saved` now runs on `cpu`: gives them
    /// back to the CPU.
    pub fn switch_in(&self, cpu: usize, saved: &mut Saved) {
        let count = core::mem::replace(&mut saved.len, 0);
        if !self.enabled() {
            return;
        }
        let Some(_busy) = self.enter(cpu) else { return };
        let mut held = self.held[cpu].lock();
        for slot in &mut saved.locks[..count] {
            if let Some(lock) = slot.take() {
                held.push(lock);
            }
        }
    }

    fn enter(&self, cpu: usize) -> Option<Busy<'_>> {
        let flag = self.busy.get(cpu)?;
        if flag.swap(true, Ordering::Acquire) { None } else { Some(Busy(flag)) }
    }

    /// The class of a lock from `site` with `subclass`, registering it on
    /// first sight; `None` when the table is full.
    fn class_of(&self, site: &LockSite, subclass: u8, kind: LockKind) -> Option<u16> {
        let mut class = match site.cached_class() {
            Some(class) => class,
            None => self.register(site.location(), 0, kind, None, Some(site))?,
        };
        for level in 1..=subclass.min(MAX_SUBCLASS) {
            class = match self.nested[class as usize].load(Ordering::Acquire) {
                0 => self.register(site.location(), level, kind, Some(class), None)?,
                next => next - 1,
            };
        }
        u16::try_from(class).ok()
    }

    fn register(
        &self,
        created: &'static Location<'static>,
        subclass: u8,
        kind: LockKind,
        parent: Option<u32>,
        site: Option<&LockSite>,
    ) -> Option<u32> {
        let mut table = self.classes.lock();
        if let Some(parent) = parent {
            match self.nested[parent as usize].load(Ordering::Acquire) {
                0 => {}
                next => return Some(next - 1),
            }
        }
        // Another lock from the same line may have registered the class.
        let found = table.info[..table.len].iter().position(|info| {
            info.is_some_and(|info| {
                info.subclass == subclass
                    && info.created.line() == created.line()
                    && info.created.column() == created.column()
                    && info.created.file() == created.file()
            })
        });
        let class = match found {
            Some(class) => class,
            None => {
                let class = table.len;
                let slot = table.info.get_mut(class)?;
                *slot =
                    Some(ClassInfo { created, subclass, kind, irq_use: None, enabled_use: None });
                table.len += 1;
                class
            }
        } as u32;
        if let Some(parent) = parent {
            self.nested[parent as usize].store(class + 1, Ordering::Release);
        }
        if let Some(site) = site {
            site.cache_class(class);
        }
        Some(class)
    }

    fn lock_ref(&self, lock: &HeldLock) -> LockRef {
        let created =
            self.classes.lock().info[usize::from(lock.class)].map_or(lock.at, |i| i.created);
        LockRef { created, subclass: lock.subclass, kind: lock.kind, at: lock.at }
    }

    fn has_edge(&self, from: u16, to: u16) -> bool {
        let (from, to) = (usize::from(from), usize::from(to));
        self.forward[from][to / 64].load(Ordering::Acquire) & (1 << (to % 64)) != 0
    }

    /// The classes reachable from `start` (included) along the edges of
    /// `matrix`.
    fn reachable(matrix: &[[AtomicU64; WORDS]; MAX_CLASSES], start: u16) -> ClassSet {
        let mut seen: ClassSet = [0; WORDS];
        let mut queue = [0u16; MAX_CLASSES];
        let (mut head, mut tail) = (0, 1);
        queue[0] = start;
        seen[usize::from(start) / 64] |= 1 << (start % 64);
        while head < tail {
            let node = usize::from(queue[head]);
            head += 1;
            for (word, row) in matrix[node].iter().enumerate() {
                let mut fresh = row.load(Ordering::Acquire) & !seen[word];
                seen[word] |= fresh;
                while fresh != 0 {
                    let bit = fresh.trailing_zeros() as usize;
                    fresh &= fresh - 1;
                    if tail < MAX_CLASSES {
                        queue[tail] = (word * 64 + bit) as u16;
                        tail += 1;
                    }
                }
            }
        }
        seen
    }

    /// A shortest path from `from` to `to`, as its first hop and its
    /// length in edges.
    fn path(&self, from: u16, to: u16) -> Option<(u16, usize)> {
        let mut parent = [u16::MAX; MAX_CLASSES];
        let mut queue = [0u16; MAX_CLASSES];
        let (mut head, mut tail) = (0, 1);
        queue[0] = from;
        parent[usize::from(from)] = from;
        while head < tail {
            let node = queue[head];
            head += 1;
            for (word, row) in self.forward[usize::from(node)].iter().enumerate() {
                let mut bits = row.load(Ordering::Acquire);
                while bits != 0 {
                    let next = (word * 64 + bits.trailing_zeros() as usize) as u16;
                    bits &= bits - 1;
                    if parent[usize::from(next)] != u16::MAX {
                        continue;
                    }
                    parent[usize::from(next)] = node;
                    if next == to {
                        let mut length = 1;
                        let mut hop = next;
                        while parent[usize::from(hop)] != from {
                            hop = parent[usize::from(hop)];
                            length += 1;
                        }
                        return Some((hop, length));
                    }
                    if tail < MAX_CLASSES {
                        queue[tail] = next;
                        tail += 1;
                    }
                }
            }
        }
        None
    }

    fn any_usage(&self, set: &ClassSet, bit: u8) -> Option<u16> {
        set.iter().enumerate().find_map(|(word, &bits)| {
            let mut bits = bits;
            while bits != 0 {
                let class = word * 64 + bits.trailing_zeros() as usize;
                bits &= bits - 1;
                if self.usage[class].load(Ordering::Relaxed) & bit != 0 {
                    return Some(class as u16);
                }
            }
            None
        })
    }

    /// Records that `next` was taken while holding `prev`, unless that
    /// closes a cycle.
    fn add_edge(&self, cpu: usize, prev: &HeldLock, next: &HeldLock) {
        if self.has_edge(prev.class, next.class) {
            return;
        }
        let mut log = self.graph.lock();
        if self.has_edge(prev.class, next.class) {
            return;
        }
        if let Some((hop, length)) = self.path(next.class, prev.class) {
            let earlier = log.find(next.class, hop).map(|r| (r.from_at, r.to_at));
            drop(log);
            self.report(Report {
                problem: Problem::Inversion,
                cpu,
                taking: Some(self.lock_ref(next)),
                held: Some(self.lock_ref(prev)),
                earlier,
                between: length - 1,
                detail: "",
                turned_off: false,
            });
            return;
        }
        let (from, to) = (usize::from(prev.class), usize::from(next.class));
        self.forward[from][to / 64].fetch_or(1 << (to % 64), Ordering::Release);
        self.backward[to][from / 64].fetch_or(1 << (from % 64), Ordering::Release);
        log.edges += 1;
        if log.len < EDGE_RECORDS {
            let index = log.len;
            log.records[index] = Some(EdgeRecord {
                from: prev.class,
                to: next.class,
                from_at: prev.at,
                to_at: next.at,
            });
            log.len += 1;
        }
        drop(log);
        // An interrupt-handler lock (or one held around it) now leads to a
        // lock taken with interrupts on?
        let before = Self::reachable(&self.backward, prev.class);
        let after = Self::reachable(&self.forward, next.class);
        if let (Some(safe), Some(unsafe_class)) =
            (self.any_usage(&before, USED_IN_IRQ), self.any_usage(&after, ENABLED_IRQ))
        {
            self.report_irq(cpu, Problem::IrqInversion, safe, unsafe_class, Some(prev), Some(next));
        }
    }

    /// Records how `lock` is being taken, and checks the interrupt rules
    /// the first time a class is used a new way.
    fn mark_usage(&self, ctx: Context, lock: &HeldLock) {
        let bit = if ctx.irq_depth > 0 {
            USED_IN_IRQ
        } else if ctx.irqs_enabled {
            ENABLED_IRQ
        } else {
            return;
        };
        let class = usize::from(lock.class);
        if self.usage[class].load(Ordering::Relaxed) & bit != 0 {
            return;
        }
        let old = self.usage[class].fetch_or(bit, Ordering::AcqRel);
        if old & bit != 0 {
            return;
        }
        if let Some(info) = self.classes.lock().info[class].as_mut() {
            if bit == USED_IN_IRQ {
                info.irq_use = Some(lock.at);
            } else {
                info.enabled_use = Some(lock.at);
            }
        }
        if old & !bit != 0 {
            self.report_irq(ctx.cpu, Problem::IrqUnsafe, lock.class, lock.class, None, Some(lock));
            return;
        }
        let conflict = if bit == USED_IN_IRQ {
            let after = Self::reachable(&self.forward, lock.class);
            self.any_usage(&after, ENABLED_IRQ).map(|other| (lock.class, other))
        } else {
            let before = Self::reachable(&self.backward, lock.class);
            self.any_usage(&before, USED_IN_IRQ).map(|other| (other, lock.class))
        };
        if let Some((safe, unsafe_class)) = conflict {
            self.report_irq(ctx.cpu, Problem::IrqInversion, safe, unsafe_class, None, Some(lock));
        }
    }

    fn report_irq(
        &self,
        cpu: usize,
        problem: Problem,
        safe: u16,
        unsafe_class: u16,
        held: Option<&HeldLock>,
        taking: Option<&HeldLock>,
    ) {
        let (irq_use, enabled_use) = {
            let table = self.classes.lock();
            let safe = table.info[usize::from(safe)];
            let unsafe_class = table.info[usize::from(unsafe_class)];
            (safe.and_then(|i| i.irq_use), unsafe_class.and_then(|i| i.enabled_use))
        };
        self.report(Report {
            problem,
            cpu,
            taking: taking.map(|l| self.lock_ref(l)),
            held: held.map(|l| self.lock_ref(l)),
            earlier: irq_use.zip(enabled_use),
            between: 0,
            detail: "",
            turned_off: false,
        });
    }

    fn out_of_room(&self, cpu: usize, detail: &'static str) {
        self.report(Report {
            problem: Problem::OutOfRoom,
            cpu,
            taking: None,
            held: None,
            earlier: None,
            between: 0,
            detail,
            turned_off: true,
        });
        self.enabled.store(false, Ordering::SeqCst);
    }

    fn report(&self, mut report: Report) {
        if !self.enabled() {
            return;
        }
        if !self.expecting.load(Ordering::SeqCst) {
            report.turned_off = true;
            self.enabled.store(false, Ordering::SeqCst);
        }
        self.reports.fetch_add(1, Ordering::SeqCst);
        self.last_problem.store(report.problem as u8, Ordering::SeqCst);
        if let Some(reporter) = self.reporter.get() {
            reporter(&report);
        }
    }
}

impl<const CPUS: usize> Default for Checker<CPUS> {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;

    use super::*;

    thread_local! {
        static SEEN: RefCell<Vec<(Problem, String)>> = const { RefCell::new(Vec::new()) };
    }

    fn record(report: &Report) {
        let mut text = String::new();
        report.write_lines(&mut |args| {
            text.push_str(&args.to_string());
            text.push('\n');
        });
        SEEN.with(|seen| seen.borrow_mut().push((report.problem, text)));
    }

    fn seen() -> Vec<(Problem, String)> {
        SEEN.with(|seen| seen.borrow_mut().drain(..).collect())
    }

    fn checker() -> Box<Checker<2>> {
        let checker = Box::new(Checker::<2>::new());
        checker.set_reporter(record);
        checker.set_enabled(true);
        seen();
        checker
    }

    const THREAD: Context = Context { cpu: 0, irq_depth: 0, irqs_enabled: true };
    const ATOMIC: Context = Context { cpu: 0, irq_depth: 0, irqs_enabled: false };
    const IRQ: Context = Context { cpu: 0, irq_depth: 1, irqs_enabled: false };

    /// A lock for the checker: its site and an address.
    struct Fake {
        site: LockSite,
        addr: usize,
        kind: LockKind,
    }

    impl Fake {
        #[track_caller]
        fn spin(addr: usize) -> Fake {
            Fake { site: LockSite::here(), addr, kind: LockKind::Spin }
        }

        #[track_caller]
        fn mutex(addr: usize) -> Fake {
            Fake { site: LockSite::here(), addr, kind: LockKind::Mutex }
        }

        #[track_caller]
        fn take(&self, checker: &Checker<2>, ctx: Context) {
            self.take_nested(checker, ctx, 0);
        }

        #[track_caller]
        fn take_nested(&self, checker: &Checker<2>, ctx: Context, subclass: u8) {
            checker.acquire(
                ctx,
                &Acquire {
                    site: &self.site,
                    addr: self.addr,
                    kind: self.kind,
                    subclass,
                    at: Location::caller(),
                    trylock: false,
                },
            );
        }

        #[track_caller]
        fn try_take(&self, checker: &Checker<2>, ctx: Context) {
            checker.acquire(
                ctx,
                &Acquire {
                    site: &self.site,
                    addr: self.addr,
                    kind: self.kind,
                    subclass: 0,
                    at: Location::caller(),
                    trylock: true,
                },
            );
        }

        fn drop_it(&self, checker: &Checker<2>, ctx: Context) {
            checker.release(ctx.cpu, self.addr);
        }
    }

    #[test]
    fn opposite_orders_are_reported_once() {
        let checker = checker();
        let (a, b) = (Fake::spin(1), Fake::spin(2));
        a.take(&checker, THREAD);
        b.take(&checker, THREAD);
        b.drop_it(&checker, THREAD);
        a.drop_it(&checker, THREAD);
        assert!(seen().is_empty());
        b.take(&checker, THREAD);
        a.take(&checker, THREAD);
        let reports = seen();
        assert_eq!(reports.len(), 1, "{reports:?}");
        assert_eq!(reports[0].0, Problem::Inversion);
        assert!(reports[0].1.contains("taken in both orders"), "{}", reports[0].1);
        assert!(reports[0].1.contains("the other order was seen before"), "{}", reports[0].1);
        assert!(reports[0].1.contains("turned off"), "{}", reports[0].1);
        assert!(!checker.enabled());
        assert_eq!(checker.last_problem(), Some(Problem::Inversion));
    }

    #[test]
    fn a_consistent_order_is_quiet() {
        let checker = checker();
        let locks: Vec<_> = (0..4)
            .map(|n| Fake { site: LockSite::here(), addr: n + 1, kind: LockKind::Spin })
            .collect();
        // Every lock made by that line is one class: nest them with
        // subclasses, always in the same order.
        for _ in 0..3 {
            for (level, lock) in locks.iter().enumerate() {
                lock.take_nested(&checker, THREAD, level as u8);
            }
            for lock in locks.iter().rev() {
                lock.drop_it(&checker, THREAD);
            }
        }
        assert!(seen().is_empty());
        let stats = checker.stats();
        assert_eq!(stats.classes, 4);
        assert_eq!(stats.edges, 6);
        assert!(stats.enabled);
    }

    #[test]
    fn inversions_through_other_locks_are_reported() {
        let checker = checker();
        let (a, b, c) = (Fake::spin(1), Fake::spin(2), Fake::spin(3));
        for (first, second) in [(&a, &b), (&b, &c)] {
            first.take(&checker, THREAD);
            second.take(&checker, THREAD);
            second.drop_it(&checker, THREAD);
            first.drop_it(&checker, THREAD);
        }
        c.take(&checker, THREAD);
        a.take(&checker, THREAD);
        let reports = seen();
        assert_eq!(reports.len(), 1);
        assert_eq!(reports[0].0, Problem::Inversion);
        assert!(reports[0].1.contains("through 1 other lock:"), "{}", reports[0].1);
    }

    #[test]
    fn recursion_is_reported_unless_nested() {
        let checker = checker();
        let make = |addr| Fake::spin(addr);
        let (x, y) = (make(1), make(2));
        x.take(&checker, THREAD);
        y.take_nested(&checker, THREAD, 1);
        assert!(seen().is_empty());
        y.drop_it(&checker, THREAD);
        y.try_take(&checker, THREAD);
        assert!(seen().is_empty(), "a trylock cannot deadlock");
        y.drop_it(&checker, THREAD);
        y.take(&checker, THREAD);
        let reports = seen();
        assert_eq!(reports.len(), 1);
        assert_eq!(reports[0].0, Problem::Recursion);
    }

    #[test]
    fn interrupt_unsafe_locks_are_reported() {
        let checker = checker();
        let lock = Fake::spin(1);
        lock.try_take(&checker, IRQ);
        lock.drop_it(&checker, IRQ);
        lock.take(&checker, ATOMIC);
        lock.drop_it(&checker, ATOMIC);
        assert!(seen().is_empty(), "taking it with interrupts off is fine");
        lock.take(&checker, THREAD);
        let reports = seen();
        assert_eq!(reports.len(), 1);
        assert_eq!(reports[0].0, Problem::IrqUnsafe);
        assert!(reports[0].1.contains("in an interrupt handler at"), "{}", reports[0].1);
    }

    #[test]
    fn interrupt_safe_locks_must_not_lead_to_unsafe_ones() {
        let checker = checker();
        let (safe, other) = (Fake::spin(1), Fake::spin(2));
        safe.take(&checker, IRQ);
        safe.drop_it(&checker, IRQ);
        other.take(&checker, THREAD);
        other.drop_it(&checker, THREAD);
        safe.take(&checker, ATOMIC);
        other.take(&checker, ATOMIC);
        let reports = seen();
        assert_eq!(reports.len(), 1);
        assert_eq!(reports[0].0, Problem::IrqInversion);
    }

    #[test]
    fn interrupt_handlers_have_their_own_order() {
        let checker = checker();
        let (a, b) = (Fake::spin(1), Fake::spin(2));
        a.take(&checker, ATOMIC);
        b.take(&checker, IRQ);
        b.drop_it(&checker, IRQ);
        a.drop_it(&checker, ATOMIC);
        b.take(&checker, ATOMIC);
        a.take(&checker, ATOMIC);
        assert!(seen().is_empty());
    }

    #[test]
    fn mutexes_need_a_context_that_can_sleep() {
        let checker = checker();
        checker.expect_reports(true);
        let (spin, mutex) = (Fake::spin(1), Fake::mutex(2));
        mutex.take(&checker, THREAD);
        mutex.drop_it(&checker, THREAD);
        assert!(seen().is_empty());
        spin.take(&checker, THREAD);
        mutex.take(&checker, THREAD);
        mutex.drop_it(&checker, THREAD);
        spin.drop_it(&checker, THREAD);
        mutex.take(&checker, ATOMIC);
        mutex.drop_it(&checker, ATOMIC);
        mutex.try_take(&checker, ATOMIC);
        mutex.drop_it(&checker, ATOMIC);
        let problems: Vec<_> = seen().into_iter().map(|(p, _)| p).collect();
        assert_eq!(problems, [Problem::SleepInAtomic, Problem::SleepInAtomic]);
        assert!(checker.enabled(), "expected reports leave the checker on");
        assert_eq!(checker.reports(), 2);
    }

    #[test]
    fn switching_moves_mutexes_with_the_thread() {
        let checker = checker();
        let (mutex, spin) = (Fake::mutex(1), Fake::spin(2));
        let mut saved = Saved::new();
        mutex.take(&checker, THREAD);
        checker.switch_out(0, &mut saved);
        assert_eq!(saved.len(), 1);
        // The thread wakes up on CPU 1, takes a spin lock and lets go.
        let cpu1 = Context { cpu: 1, ..THREAD };
        checker.switch_in(1, &mut saved);
        assert!(saved.is_empty());
        let mut held = 0;
        checker.held(1, |_| held += 1);
        assert_eq!(held, 1);
        spin.take(&checker, cpu1);
        spin.drop_it(&checker, cpu1);
        mutex.drop_it(&checker, cpu1);
        checker.held(1, |_| held += 1);
        assert_eq!(held, 1);
        assert!(seen().is_empty());
        // Switching out holding a spin lock is a bug.
        spin.take(&checker, THREAD);
        checker.switch_out(0, &mut saved);
        let reports = seen();
        assert_eq!(reports.len(), 1);
        assert_eq!(reports[0].0, Problem::SleepInAtomic);
        assert!(saved.is_empty());
    }

    #[test]
    fn running_out_of_room_turns_the_checker_off() {
        let checker = checker();
        checker.expect_reports(true);
        let locks: Vec<_> = (0..=MAX_HELD)
            .map(|n| Fake { site: LockSite::here(), addr: n + 1, kind: LockKind::Spin })
            .collect();
        for (n, lock) in locks.iter().enumerate() {
            lock.take_nested(&checker, ATOMIC, (n % 8) as u8);
        }
        let problems: Vec<_> = seen().into_iter().map(|(p, _)| p).collect();
        assert!(problems.contains(&Problem::OutOfRoom), "{problems:?}");
        assert!(!checker.enabled());
    }

    #[test]
    fn the_checker_does_nothing_while_off() {
        let checker = checker();
        checker.set_enabled(false);
        let lock = Fake::spin(1);
        lock.take(&checker, THREAD);
        lock.take(&checker, THREAD);
        assert!(seen().is_empty());
        assert_eq!(checker.stats().classes, 0);
    }
}
