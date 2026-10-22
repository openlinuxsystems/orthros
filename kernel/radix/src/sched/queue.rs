// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! One CPU's run queue: the threads that may run there.
//!
//! Real-time threads wait in a [`RtQueue`] and run before any fair thread;
//! the rest wait in an EEVDF [`FairQueue`]. The running thread stays in
//! its queue, marked as running, and the CPU's idle thread runs when both
//! are empty. Everything here runs with the queue's lock held and
//! interrupts off, and never allocates: the queues are given room for
//! every thread ahead of time ([`super::make_room`]).
//!
//! # Weights
//!
//! A fair thread's weight in the queue is its nice level's weight, scaled
//! by the CPU weights of its resource domains. On each CPU, a domain's
//! time is split between its own threads and its child domains in
//! proportion to their weights, the domain's own threads together counting
//! as one child of the default weight (100). So two domains of weights 200
//! and 100 with runnable threads on one CPU get two thirds and one third of
//! its fair time, however many threads each has. [`Rq::reweigh`] works the
//! shares out again whenever the fair queue's membership changes; the
//! queue's reweighting keeps each thread's lag, so a change rewards or
//! punishes no one. The weights are scaled so that they add up to 1024 per
//! queued thread, as if every thread were at nice 0.
//!
//! References to threads ([`ThreadRef`]) dropped here are never the last:
//! the registry holds every thread until the reaper frees it, in thread
//! context.

use alloc::vec::Vec;
use core::sync::atomic::Ordering;

use radix_mm::Kmalloc;
use radix_sched::{
    BASE_SLICE_NS, CpuLoad, DOMAIN_WEIGHT_DEFAULT, Entity, FairQueue, Level, NICE_0_WEIGHT,
    Placement, RR_QUANTUM_NS, RtEntry, RtQueue, is_cache_hot, mul_div, mul_div_unsigned,
};

use super::thread::{FAIR, NOT_QUEUED, Policy, REAL_TIME, State, Thread, ThreadRef};
use crate::domains::Domain;

/// The shortest time the kernel lets a fair thread run before it looks
/// again, so a thread at its deadline is not interrupted at once.
const MIN_SLICE_NS: u64 = 100_000;

/// How a thread joins a queue.
#[derive(Clone, Copy, Debug)]
pub(super) enum Class {
    /// The fair class, with the lag it left with (or none, if new).
    Fair {
        /// Its lag, in nanoseconds at nice 0.
        lag_ns: i64,
        /// Whether it never ran: it starts at lag 0 with half a slice.
        new: bool,
    },
    /// A real-time class at a priority.
    RealTime {
        /// The priority.
        prio: u8,
        /// Round robin rather than FIFO.
        round_robin: bool,
    },
}

impl Class {
    /// The class `state` says the thread should join.
    pub(super) fn of(state: &State, new: bool) -> Class {
        match state.rt_prio() {
            0 => Class::Fair { lag_ns: state.lag_ns, new },
            prio => Class::RealTime {
                prio,
                // A thread running on a lent priority runs it as FIFO.
                round_robin: matches!(state.policy, Policy::RoundRobin { .. })
                    && state.boost <= state.policy.rt_prio(),
            },
        }
    }
}

/// Lag in virtual time for an entity of weight `weight`, from nanoseconds
/// at nice 0.
fn to_virtual(lag_ns: i64, weight: u64) -> i64 {
    mul_div(lag_ns.into(), NICE_0_WEIGHT.into(), weight.max(1).into()) as i64
}

/// The reverse of [`to_virtual`].
fn to_real(lag: i64, weight: u64) -> i64 {
    mul_div(lag.into(), weight.into(), NICE_0_WEIGHT.into()) as i64
}

/// A CPU's run queue.
pub(super) struct Rq {
    fair: FairQueue<ThreadRef, Kmalloc>,
    rt: RtQueue<ThreadRef, Kmalloc>,
    /// The running thread.
    pub(super) current: ThreadRef,
    /// The CPU's idle thread, which is in no queue.
    pub(super) idle: ThreadRef,
    /// When the running thread was last charged for its time.
    clock_ns: u64,
    /// Threads switched to.
    pub(super) switches: u64,
    /// Threads pulled here by balancing, by level.
    pub(super) pulled: [u64; 4],
}

impl Rq {
    /// A queue with room for `room` threads, running `current`.
    pub(super) fn new(
        current: ThreadRef,
        idle: ThreadRef,
        now: u64,
        room: usize,
        alloc: Kmalloc,
        tick_ns: u64,
    ) -> Option<Rq> {
        let mut fair = FairQueue::new_in(alloc, BASE_SLICE_NS, tick_ns);
        fair.try_reserve(room).ok()?;
        let mut rt = RtQueue::new_in(alloc);
        rt.try_reserve(room).ok()?;
        Some(Rq { fair, rt, current, idle, clock_ns: now, switches: 0, pulled: [0; 4] })
    }

    /// Swaps in larger buffers, made outside the lock, and returns the old
    /// ones to be freed outside it too.
    #[allow(clippy::type_complexity)]
    pub(super) fn grow(
        &mut self,
        fair: Vec<Entity<ThreadRef>, Kmalloc>,
        rt: Vec<RtEntry<ThreadRef>, Kmalloc>,
    ) -> (Vec<Entity<ThreadRef>, Kmalloc>, Vec<RtEntry<ThreadRef>, Kmalloc>) {
        (self.fair.grow(fair), self.rt.grow(rt))
    }

    /// Queued threads, the running one included (the idle thread is not).
    pub(super) fn len(&self) -> usize {
        self.fair.len() + self.rt.len()
    }

    /// Whether the idle thread is running.
    pub(super) fn idling(&self) -> bool {
        ThreadRef::ptr_eq(&self.current, &self.idle)
    }

    /// The load to publish for placement and balancing.
    pub(super) fn load(&self) -> CpuLoad {
        let rt_prio = match self.current.queued.load(Ordering::Relaxed) {
            REAL_TIME if !self.idling() => self.current.queued_prio.load(Ordering::Relaxed),
            _ => 0,
        };
        CpuLoad { running: self.len() as u32, rt_prio }
    }

    /// Adds a ready thread.
    pub(super) fn enqueue(&mut self, thread: ThreadRef, class: Class) {
        let refused = match class {
            Class::Fair { lag_ns, new } => {
                let weight = thread.weight.load(Ordering::Relaxed);
                let placement =
                    if new { Placement::New } else { Placement::Lag(to_virtual(lag_ns, weight)) };
                thread.queued.store(FAIR, Ordering::Relaxed);
                let refused = self.fair.enqueue(thread, weight, placement).err();
                self.reweigh();
                refused
            }
            Class::RealTime { prio, round_robin } => {
                thread.queued.store(REAL_TIME, Ordering::Relaxed);
                thread.queued_prio.store(prio, Ordering::Relaxed);
                thread.round_robin.store(round_robin, Ordering::Relaxed);
                thread.quantum_left_ns.store(RR_QUANTUM_NS, Ordering::Relaxed);
                self.rt.enqueue(thread, prio, false).err()
            }
        };
        if let Some(thread) = refused {
            // `make_room` gives every queue room for every thread before
            // the thread exists, so this is a scheduler bug, not memory
            // running out.
            thread.queued.store(NOT_QUEUED, Ordering::Relaxed);
            panic!("Scheduler: a run queue had no room for thread `{}`", thread.name());
        }
    }

    /// Takes `thread` off this queue. Returns the lag it leaves with, in
    /// nanoseconds at nice 0 (0 for real-time threads), or `None` if it was
    /// not queued here.
    pub(super) fn dequeue(&mut self, thread: &Thread) -> Option<i64> {
        match thread.queued.load(Ordering::Relaxed) {
            FAIR => {
                let index = self.fair.position(|t| core::ptr::eq(&**t, thread))?;
                let weight = self.fair.entities()[index].weight();
                let (key, lag) = self.fair.dequeue(index)?;
                key.queued.store(NOT_QUEUED, Ordering::Relaxed);
                self.reweigh();
                Some(to_real(lag, weight))
            }
            REAL_TIME => {
                let index = self.rt.position(|t| core::ptr::eq(&**t, thread))?;
                let entry = self.rt.dequeue(index)?;
                entry.key().queued.store(NOT_QUEUED, Ordering::Relaxed);
                Some(0)
            }
            _ => None,
        }
    }

    /// Charges the running thread for the time since the last charge.
    /// Returns whether it should give the CPU to another thread: its fair
    /// slice or round-robin quantum ran out and others are waiting.
    pub(super) fn charge(&mut self, now: u64) -> bool {
        let delta = now.saturating_sub(self.clock_ns);
        self.clock_ns = now;
        let current = &self.current;
        current.stats.runtime_ns.fetch_add(delta, Ordering::Relaxed);
        if self.idling() {
            return false;
        }
        match current.queued.load(Ordering::Relaxed) {
            FAIR if self.fair.running().is_some() => self.fair.charge(delta) && self.fair.len() > 1,
            REAL_TIME if current.round_robin.load(Ordering::Relaxed) => {
                let left = current.quantum_left_ns.load(Ordering::Relaxed).saturating_sub(delta);
                if left > 0 {
                    current.quantum_left_ns.store(left, Ordering::Relaxed);
                    return false;
                }
                current.quantum_left_ns.store(RR_QUANTUM_NS, Ordering::Relaxed);
                let prio = current.queued_prio.load(Ordering::Relaxed);
                let peers = self.rt.entries().iter().filter(|e| e.prio() == prio).count();
                if peers > 1
                    && let Some(index) = self.rt.running()
                {
                    self.rt.to_back(index);
                }
                peers > 1
            }
            _ => false,
        }
    }

    /// The running thread gives way to others of its class.
    pub(super) fn yield_current(&mut self) {
        match self.current.queued.load(Ordering::Relaxed) {
            FAIR => self.fair.yield_running(),
            REAL_TIME => {
                if let Some(index) = self.rt.running() {
                    self.rt.to_back(index);
                }
            }
            _ => {}
        }
    }

    /// Chooses what runs next, marks it running and returns it: the most
    /// urgent real-time thread, else the fair pick, else the idle thread.
    pub(super) fn pick(&mut self) -> ThreadRef {
        if let Some(index) = self.rt.pick() {
            self.rt.set_running(Some(index));
            self.fair.set_running(None);
            if let Some(thread) = self.rt.key_of(index) {
                return thread.clone();
            }
        }
        self.rt.set_running(None);
        // The fair pick breaks ties for the running entity, so pick before
        // marking.
        let pick = self.fair.pick();
        self.fair.set_running(pick);
        match pick.and_then(|index| self.fair.key_of(index)) {
            Some(thread) => thread.clone(),
            None => self.idle.clone(),
        }
    }

    /// Marks the running thread as running in its queue again, after it
    /// was taken off and put back (a change of class or priority).
    pub(super) fn mark_current(&mut self) {
        let current = &self.current;
        let fair = self.fair.position(|t| ThreadRef::ptr_eq(t, current));
        let rt = self.rt.position(|t| ThreadRef::ptr_eq(t, current));
        self.fair.set_running(fair);
        self.rt.set_running(rt);
    }

    /// When the CPU should next look at the running thread: the end of its
    /// fair slice or round-robin quantum, if others wait for it.
    pub(super) fn slice_end(&self, now: u64) -> u64 {
        if self.idling() {
            return u64::MAX;
        }
        match self.current.queued.load(Ordering::Relaxed) {
            FAIR if self.fair.len() > 1 => {
                let left = self.fair.running_left_ns().unwrap_or(BASE_SLICE_NS);
                now.saturating_add(left.max(MIN_SLICE_NS))
            }
            REAL_TIME if self.current.round_robin.load(Ordering::Relaxed) => {
                now.saturating_add(self.current.quantum_left_ns.load(Ordering::Relaxed))
            }
            _ => u64::MAX,
        }
    }

    /// Whether the thread just queued should take the CPU from the running
    /// one.
    pub(super) fn wants_preempt(&self) -> bool {
        if self.idling() {
            return self.len() > 0;
        }
        match self.current.queued.load(Ordering::Relaxed) {
            REAL_TIME => {
                let prio = self.current.queued_prio.load(Ordering::Relaxed);
                self.rt.top_prio().is_some_and(|top| top > prio)
            }
            FAIR => !self.rt.is_empty() || self.fair.should_preempt(),
            _ => self.len() > 0,
        }
    }

    /// Takes a thread that waits here and may run on `cpu`, for balancing
    /// across `level`: one whose caches have gone cold, unless `force`.
    /// Real-time threads go first.
    pub(super) fn take_for(
        &mut self,
        cpu: usize,
        now: u64,
        level: Level,
        force: bool,
    ) -> Option<(ThreadRef, Class)> {
        let movable = |thread: &ThreadRef| {
            let since = now.saturating_sub(thread.stats.last_ran_ns.load(Ordering::Relaxed));
            thread.allowed().contains(cpu) && (force || !is_cache_hot(since, level))
        };
        let running = self.rt.running();
        let rt = (0..self.rt.len())
            .find(|&i| Some(i) != running && self.rt.key_of(i).is_some_and(movable));
        if let Some(index) = rt {
            let entry = self.rt.dequeue(index)?;
            let thread = entry.key().clone();
            thread.queued.store(NOT_QUEUED, Ordering::Relaxed);
            let class = Class::RealTime {
                prio: entry.prio(),
                round_robin: thread.round_robin.load(Ordering::Relaxed),
            };
            return Some((thread, class));
        }
        let running = self.fair.running();
        let index = (0..self.fair.len())
            .find(|&i| Some(i) != running && self.fair.key_of(i).is_some_and(movable))?;
        let weight = self.fair.entities()[index].weight();
        let (thread, lag) = self.fair.dequeue(index)?;
        thread.queued.store(NOT_QUEUED, Ordering::Relaxed);
        self.reweigh();
        Some((thread, Class::Fair { lag_ns: to_real(lag, weight), new: false }))
    }

    /// Calls `f` with every queued thread, whether it is running, and its
    /// queue weight or real-time priority.
    pub(super) fn for_each(&self, mut f: impl FnMut(&ThreadRef, bool, Weighing)) {
        for (index, entity) in self.fair.entities().iter().enumerate() {
            f(entity.key(), self.fair.running() == Some(index), Weighing::Weight(entity.weight()));
        }
        for (index, entry) in self.rt.entries().iter().enumerate() {
            f(entry.key(), self.rt.running() == Some(index), Weighing::Prio(entry.prio()));
        }
    }

    /// Checks the fair queue's books, for the self-tests.
    pub(super) fn check(&self) -> Result<(), &'static str> {
        self.fair.check()
    }

    /// Works out every fair thread's weight again from its nice level and
    /// its domains' shares (see the module comment).
    fn reweigh(&mut self) {
        let count = self.fair.len();
        if count == 0 {
            return;
        }
        let scale = NICE_0_WEIGHT as u128 * count as u128;
        let entities = self.fair.entities();
        let first: &Domain = entities[0].key().domain();
        if entities.iter().all(|entity| core::ptr::eq::<Domain>(&**entity.key().domain(), first)) {
            // The usual case: every thread in one domain. Its branch of the
            // tree has nothing beside it, so its weights cancel out of
            // `Shares::weight` and each thread gets its part of the nice
            // weights, the same answer without building the tree.
            let total: u64 =
                entities.iter().map(|entity| entity.key().weight.load(Ordering::Relaxed)).sum();
            for index in 0..count {
                let Some(thread) = self.fair.key_of(index) else { continue };
                let base = u128::from(thread.weight.load(Ordering::Relaxed));
                let weight = mul_div_unsigned(scale, base, u128::from(total.max(1)))
                    .clamp(2, u128::from(u32::MAX)) as u64;
                self.fair.reweight(index, weight);
            }
            return;
        }
        let mut shares = Shares::new();
        for entity in self.fair.entities() {
            let thread = entity.key();
            shares.add(thread.domain(), thread.weight.load(Ordering::Relaxed));
        }
        for index in 0..count {
            let Some(thread) = self.fair.key_of(index) else { continue };
            let base = thread.weight.load(Ordering::Relaxed);
            let weight = shares.weight(thread.domain(), base, scale);
            self.fair.reweight(index, weight);
        }
    }
}

/// How a queued thread is weighed, for reports.
#[derive(Clone, Copy, Debug)]
pub(super) enum Weighing {
    /// A fair thread's queue weight.
    Weight(u64),
    /// A real-time thread's priority.
    Prio(u8),
}

/// Most domains one queue tells apart; beyond that, threads keep their
/// nice weights.
const SHARE_NODES: usize = 32;

/// One domain in a queue's share tree.
#[derive(Clone, Copy, Default)]
struct Node {
    /// The domain's address, which identifies it.
    key: usize,
    parent: Option<usize>,
    /// Its CPU weight.
    weight: u64,
    /// The summed nice weights of its own queued threads.
    own: u64,
    /// The summed CPU weights of its child domains with queued threads.
    children: u64,
}

/// The domains of a queue's fair threads, and those above them.
struct Shares {
    nodes: [Node; SHARE_NODES],
    len: usize,
    full: bool,
}

impl Shares {
    fn new() -> Shares {
        Shares { nodes: [Node::default(); SHARE_NODES], len: 0, full: false }
    }

    fn find(&self, key: usize) -> Option<usize> {
        self.nodes[..self.len].iter().position(|n| n.key == key)
    }

    /// The node of `domain`, added with the domains above it if new.
    fn node(&mut self, domain: &Domain) -> Option<usize> {
        let key = domain as *const Domain as usize;
        if let Some(index) = self.find(key) {
            return Some(index);
        }
        let parent = match domain.parent() {
            Some(parent) => Some(self.node(parent)?),
            None => None,
        };
        if self.len == SHARE_NODES {
            self.full = true;
            return None;
        }
        let weight = u64::from(domain.weight());
        self.nodes[self.len] = Node { key, parent, weight, own: 0, children: 0 };
        if let Some(parent) = parent {
            self.nodes[parent].children += weight;
        }
        self.len += 1;
        Some(self.len - 1)
    }

    fn add(&mut self, domain: &Domain, weight: u64) {
        if let Some(index) = self.node(domain) {
            self.nodes[index].own += weight;
        }
    }

    /// The queue weight of a thread of nice weight `base` in `domain`, with
    /// `scale` the total to share out.
    fn weight(&self, domain: &Domain, base: u64, scale: u128) -> u64 {
        let found = self.find(domain as *const Domain as usize);
        let (Some(index), false) = (found, self.full) else { return base };
        let node = &self.nodes[index];
        let default = u128::from(DOMAIN_WEIGHT_DEFAULT);
        // Its own threads' part of the domain: one child of the default
        // weight among its child domains.
        let mut num = default * u128::from(base);
        let mut den = (u128::from(node.children) + default) * u128::from(node.own.max(1));
        let mut at = index;
        while let Some(parent) = self.nodes[at].parent {
            let above = &self.nodes[parent];
            let own_part = if above.own > 0 { default } else { 0 };
            num *= u128::from(self.nodes[at].weight);
            den *= u128::from(above.children) + own_part;
            at = parent;
        }
        mul_div_unsigned(scale, num, den.max(1)).clamp(2, u128::from(u32::MAX)) as u64
    }
}
