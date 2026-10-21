// SPDX-FileCopyrightText: 2026 The OrthrOS contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! The fair class: an EEVDF run queue.
//!
//! EEVDF (Earliest Eligible Virtual Deadline First, Stoica and Abdel-Wahab
//! 1995; Linux's fair class since 6.6) gives every runnable entity a share
//! of the CPU in proportion to its weight, and bounds how far any entity
//! can fall behind or run ahead of that share.
//!
//! Each entity has a *virtual runtime* `v`, which advances by
//! `real time * 1024 / weight` while it runs, and the queue has a virtual
//! time `V`, the weight-averaged `v` of its entities. An entity's *lag*,
//! `V - v`, is the service it is owed (positive) or has had in advance
//! (negative). An entity is *eligible* when its lag is not negative. Each
//! entity also has a *virtual deadline*, `v` plus its slice in virtual
//! time, and the queue runs the eligible entity with the earliest deadline.
//! The running entity keeps the CPU until its slice is used up (the
//! deadline passes) or an entity with an earlier deadline wakes up.
//!
//! An entity that leaves the queue (to sleep, or to move to another CPU)
//! takes its lag along, clamped to a little more than a slice; it comes back
//! at that lag, so sleeping neither earns credit nor costs any, and a
//! thread that moves between CPUs keeps its place. A new entity starts at
//! lag 0 with half a slice, so it runs soon without crowding the others.
//!
//! The queue keeps `Σ w` and `Σ w (v - v0)` for a base `v0` (the smallest
//! `v`), so `V` and eligibility are exact integer arithmetic: an entity is
//! eligible when `(v - v0) Σ w <= Σ w (v - v0)`. Some entity always is (the
//! one with the smallest `v`), so picking never fails on a non-empty queue
//! (proved in `proofs.rs`). Virtual times are `u64` that may wrap; only
//! differences, as `i64`, are compared.
//!
//! Entities live in a vector and picking scans it: the queues are short in
//! Phase 4. The vector never grows on its own. The kernel gives it room
//! ahead of time, outside its locks ([`FairQueue::grow`]), so enqueueing
//! never allocates.

use alloc::vec::Vec;
use core::alloc::Allocator;

use crate::weight::NICE_0_WEIGHT;

/// The default base slice: how long an entity runs before others get a
/// turn, in nanoseconds.
pub const BASE_SLICE_NS: u64 = 3_000_000;

/// How an entity joins a queue.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Placement {
    /// A new entity: lag 0, half a slice to its first deadline.
    New,
    /// An entity coming back with the lag it left with.
    Lag(i64),
}

/// One runnable entity.
#[derive(Clone, Debug)]
pub struct Entity<K> {
    key: K,
    weight: u64,
    vruntime: u64,
    deadline: u64,
}

impl<K> Entity<K> {
    /// What the entity stands for.
    pub fn key(&self) -> &K {
        &self.key
    }

    /// Its weight.
    pub fn weight(&self) -> u64 {
        self.weight
    }

    /// Its virtual runtime.
    pub fn vruntime(&self) -> u64 {
        self.vruntime
    }

    /// Its virtual deadline.
    pub fn deadline(&self) -> u64 {
        self.deadline
    }
}

/// `ns` of real time for an entity of weight `weight`, in virtual time.
pub fn virtual_ns(ns: u64, weight: u64) -> u64 {
    (u128::from(ns) * u128::from(NICE_0_WEIGHT) / u128::from(weight.max(1))) as u64
}

/// An EEVDF run queue of entities named by `K`.
pub struct FairQueue<K, A: Allocator> {
    entities: Vec<Entity<K>, A>,
    /// The entity on the CPU, if it is from this queue.
    running: Option<usize>,
    /// The base of the virtual times: the smallest `vruntime`.
    zero: u64,
    /// `Σ w`.
    load: u64,
    /// `Σ w (v - zero)`.
    sum: i128,
    slice_ns: u64,
    tick_ns: u64,
}

impl<K, A: Allocator> FairQueue<K, A> {
    /// An empty queue whose entities get `slice_ns` of real time per turn,
    /// for a CPU whose timer ticks every `tick_ns`.
    pub fn new_in(alloc: A, slice_ns: u64, tick_ns: u64) -> Self {
        Self {
            entities: Vec::new_in(alloc),
            running: None,
            zero: 0,
            load: 0,
            sum: 0,
            slice_ns: slice_ns.max(1),
            tick_ns,
        }
    }

    /// How many entities are queued, the running one included.
    pub fn len(&self) -> usize {
        self.entities.len()
    }

    /// Whether the queue is empty.
    pub fn is_empty(&self) -> bool {
        self.entities.is_empty()
    }

    /// The sum of the entities' weights.
    pub fn load(&self) -> u64 {
        self.load
    }

    /// How many entities fit without growing.
    pub fn capacity(&self) -> usize {
        self.entities.capacity()
    }

    /// Moves the entities into `buffer`, an empty vector with more room,
    /// and returns the old one, emptied. The kernel allocates `buffer`
    /// before taking the queue's lock and frees what comes back after
    /// letting go, so the queue never allocates under its lock. Returns
    /// `buffer` itself if it is not larger, or not empty.
    pub fn grow(&mut self, mut buffer: Vec<Entity<K>, A>) -> Vec<Entity<K>, A> {
        if !buffer.is_empty() || buffer.capacity() <= self.entities.capacity() {
            return buffer;
        }
        while let Some(item) = self.entities.pop() {
            // The buffer is larger, so this always fits.
            let _ = buffer.push_within_capacity(item);
        }
        buffer.reverse();
        core::mem::replace(&mut self.entities, buffer)
    }

    /// Makes room for `additional` more entities, allocating here: for
    /// callers that hold no lock (and the host tests).
    pub fn try_reserve(
        &mut self,
        additional: usize,
    ) -> Result<(), alloc::collections::TryReserveError> {
        self.entities.try_reserve(additional)
    }

    /// The entities, in no particular order.
    pub fn entities(&self) -> &[Entity<K>] {
        &self.entities
    }

    /// The queue's virtual time `V`: the weighted average virtual runtime.
    pub fn avg_vruntime(&self) -> u64 {
        if self.load == 0 {
            return self.zero;
        }
        // Round towards minus infinity, so `V` never passes an entity
        // that is exactly on it.
        let avg = self.sum.div_euclid(i128::from(self.load));
        self.zero.wrapping_add(avg as i64 as u64)
    }

    /// Whether entity `index` is eligible: not ahead of the queue's
    /// virtual time.
    pub fn is_eligible(&self, index: usize) -> bool {
        self.entities.get(index).is_some_and(|e| self.eligible(e))
    }

    fn key(&self, vruntime: u64) -> i64 {
        vruntime.wrapping_sub(self.zero) as i64
    }

    fn eligible(&self, entity: &Entity<K>) -> bool {
        i128::from(self.key(entity.vruntime)) * i128::from(self.load) <= self.sum
    }

    /// The lag of entity `index`: `V - v`.
    pub fn lag(&self, index: usize) -> Option<i64> {
        let entity = self.entities.get(index)?;
        Some(self.avg_vruntime().wrapping_sub(entity.vruntime) as i64)
    }

    /// The largest lag an entity of weight `weight` keeps when it leaves.
    pub fn lag_limit(&self, weight: u64) -> i64 {
        virtual_ns((2 * self.slice_ns).max(self.tick_ns), weight) as i64
    }

    /// Moves `zero` to the smallest `vruntime`, keeping `sum` in step.
    fn rebase(&mut self) {
        let Some(min) = self.entities.iter().map(|e| self.key(e.vruntime)).min() else { return };
        if min != 0 {
            self.zero = self.zero.wrapping_add(min as u64);
            self.sum -= i128::from(self.load) * i128::from(min);
        }
    }

    fn add(&mut self, entity: Entity<K>) -> Result<usize, Entity<K>> {
        let (weight, key) = (entity.weight, self.key(entity.vruntime));
        self.entities.push_within_capacity(entity)?;
        self.load += weight;
        self.sum += i128::from(weight) * i128::from(key);
        Ok(self.entities.len() - 1)
    }

    fn take(&mut self, index: usize) -> Entity<K> {
        let entity = self.entities.swap_remove(index);
        self.load -= entity.weight;
        self.sum -= i128::from(entity.weight) * i128::from(self.key(entity.vruntime));
        match self.running {
            Some(running) if running == index => self.running = None,
            Some(running) if running == self.entities.len() => self.running = Some(index),
            _ => {}
        }
        entity
    }

    /// Adds an entity of weight `weight`, placed as `placement` says.
    /// Returns its index, or gives `key` back when the queue is full (the
    /// kernel makes room first, so it never is).
    pub fn enqueue(&mut self, key: K, weight: u64, placement: Placement) -> Result<usize, K> {
        let weight = weight.max(1);
        let avg = self.avg_vruntime();
        let lag = match placement {
            Placement::New => 0,
            // Joining raises `Σ w`, which pulls `V` towards the newcomer;
            // scaling the lag by (W + w) / W makes the lag it has once it
            // is in the queue the one it left with.
            Placement::Lag(lag) if self.load > 0 => {
                (i128::from(lag) * i128::from(self.load + weight) / i128::from(self.load)) as i64
            }
            Placement::Lag(lag) => lag,
        };
        let vruntime = avg.wrapping_sub(lag as u64);
        let slice = virtual_ns(self.slice_ns, weight);
        let deadline =
            vruntime.wrapping_add(if placement == Placement::New { slice / 2 } else { slice });
        let index = self.add(Entity { key, weight, vruntime, deadline }).map_err(|e| e.key)?;
        self.rebase();
        Ok(index)
    }

    /// Removes the entity at `index` and returns it with the lag it takes
    /// along. If it is the running entity, charge it first.
    pub fn dequeue(&mut self, index: usize) -> Option<(K, i64)> {
        if index >= self.entities.len() {
            return None;
        }
        let avg = self.avg_vruntime();
        let entity = self.take(index);
        let limit = self.lag_limit(entity.weight);
        let lag = (avg.wrapping_sub(entity.vruntime) as i64).clamp(-limit, limit);
        self.rebase();
        Some((entity.key, lag))
    }

    /// The index of the first entity whose key satisfies `matches`.
    pub fn position(&self, mut matches: impl FnMut(&K) -> bool) -> Option<usize> {
        self.entities.iter().position(|e| matches(&e.key))
    }

    /// Marks entity `index` as the one on the CPU, or none.
    pub fn set_running(&mut self, index: Option<usize>) {
        self.running = index.filter(|&i| i < self.entities.len());
    }

    /// The index of the running entity.
    pub fn running(&self) -> Option<usize> {
        self.running
    }

    /// Charges the running entity for `ns` of real time. Returns whether
    /// its slice ran out, in which case it got a new deadline and the CPU
    /// should pick again.
    pub fn charge(&mut self, ns: u64) -> bool {
        let Some(index) = self.running else { return false };
        let slice_ns = self.slice_ns;
        let entity = &mut self.entities[index];
        let delta = virtual_ns(ns, entity.weight).max(u64::from(ns > 0));
        entity.vruntime = entity.vruntime.wrapping_add(delta);
        let weight = entity.weight;
        let expired = entity.vruntime.wrapping_sub(entity.deadline) as i64 >= 0;
        if expired {
            entity.deadline = entity.vruntime.wrapping_add(virtual_ns(slice_ns, weight));
        }
        self.sum += i128::from(weight) * i128::from(delta);
        self.rebase();
        expired
    }

    /// How much real time the running entity has left before its deadline
    /// passes and others get a turn: when the kernel should look again.
    pub fn running_left_ns(&self) -> Option<u64> {
        let entity = &self.entities[self.running?];
        let left = entity.deadline.wrapping_sub(entity.vruntime) as i64;
        if left <= 0 {
            return Some(0);
        }
        Some(
            (u128::from(left as u64) * u128::from(entity.weight) / u128::from(NICE_0_WEIGHT))
                as u64,
        )
    }

    /// The running entity gives way: its deadline moves one slice later,
    /// so every other eligible entity with an earlier deadline runs first
    /// (as Linux's `sched_yield` does for EEVDF). Its virtual runtime, and
    /// so its share, is unchanged.
    pub fn yield_running(&mut self) {
        let Some(index) = self.running else { return };
        let slice = virtual_ns(self.slice_ns, self.entities[index].weight);
        let entity = &mut self.entities[index];
        entity.deadline = entity.deadline.wrapping_add(slice);
    }

    /// Changes the weight of entity `index`, keeping its lag and the time
    /// to its deadline in proportion, so the change neither rewards nor
    /// punishes it. `V` stays where it was.
    pub fn reweight(&mut self, index: usize, weight: u64) {
        let weight = weight.max(1);
        let Some(entity) = self.entities.get(index) else { return };
        let old = entity.weight;
        if old == weight {
            return;
        }
        let avg = self.avg_vruntime();
        let lag = avg.wrapping_sub(entity.vruntime) as i64;
        let to_deadline = entity.deadline.wrapping_sub(entity.vruntime) as i64;
        let key = self.key(entity.vruntime);
        self.load -= old;
        self.sum -= i128::from(old) * i128::from(key);
        let scale = |value: i64| (i128::from(value) * i128::from(old) / i128::from(weight)) as i64;
        let vruntime = avg.wrapping_sub(scale(lag) as u64);
        let entity = &mut self.entities[index];
        entity.weight = weight;
        entity.vruntime = vruntime;
        entity.deadline = vruntime.wrapping_add(scale(to_deadline) as u64);
        let key = self.key(vruntime);
        self.load += weight;
        self.sum += i128::from(weight) * i128::from(key);
        self.rebase();
    }

    /// The eligible entity with the earliest deadline. Ties go to the
    /// running entity, then to the lower index.
    pub fn pick(&self) -> Option<usize> {
        let mut best: Option<usize> = None;
        for (index, entity) in self.entities.iter().enumerate() {
            if !self.eligible(entity) {
                continue;
            }
            best = match best {
                None => Some(index),
                Some(b) => {
                    let order = (entity.deadline.wrapping_sub(self.entities[b].deadline)) as i64;
                    if order < 0 || (order == 0 && self.running == Some(index)) {
                        Some(index)
                    } else {
                        Some(b)
                    }
                }
            };
        }
        best
    }

    /// Whether a just-woken entity should take the CPU from the running
    /// one: it is the pick and its deadline is earlier.
    pub fn should_preempt(&self) -> bool {
        let Some(running) = self.running else { return !self.entities.is_empty() };
        match self.pick() {
            Some(pick) if pick != running => {
                (self.entities[pick].deadline.wrapping_sub(self.entities[running].deadline) as i64)
                    < 0
            }
            _ => false,
        }
    }

    /// The key of entity `index`.
    pub fn key_of(&self, index: usize) -> Option<&K> {
        self.entities.get(index).map(|e| &e.key)
    }

    /// The key of entity `index`, mutably.
    pub fn key_of_mut(&mut self, index: usize) -> Option<&mut K> {
        self.entities.get_mut(index).map(|e| &mut e.key)
    }

    /// Checks the queue's bookkeeping against its entities: for the tests
    /// and the debugger.
    pub fn check(&self) -> Result<(), &'static str> {
        let load: u64 = self.entities.iter().map(|e| e.weight).sum();
        let sum: i128 = self
            .entities
            .iter()
            .map(|e| i128::from(e.weight) * i128::from(self.key(e.vruntime)))
            .sum();
        if load != self.load {
            return Err("the load does not match the entities' weights");
        }
        if sum != self.sum {
            return Err("the weighted virtual-time sum does not match the entities");
        }
        if self.entities.iter().any(|e| self.key(e.vruntime) < 0) {
            return Err("an entity is below the base virtual time");
        }
        if self.running.is_some_and(|r| r >= self.entities.len()) {
            return Err("the running entity is out of range");
        }
        if !self.entities.is_empty() && self.pick().is_none() {
            return Err("no entity is eligible");
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::alloc::Global;

    use proptest::prelude::*;

    use super::*;
    use crate::weight::weight;

    const TICK: u64 = 10_000_000;

    fn queue() -> FairQueue<u32, Global> {
        let mut queue = FairQueue::new_in(Global, BASE_SLICE_NS, TICK);
        queue.try_reserve(64).unwrap();
        queue
    }

    /// Runs the queue for `steps` of `step_ns` each and returns the real
    /// time each key got.
    fn run(queue: &mut FairQueue<u32, Global>, steps: usize, step_ns: u64) -> Vec<u64> {
        let mut service = vec![0u64; 64];
        let mut running = queue.pick();
        queue.set_running(running);
        for _ in 0..steps {
            let index = running.unwrap();
            service[*queue.key_of(index).unwrap() as usize] += step_ns;
            if queue.charge(step_ns) {
                running = queue.pick();
                queue.set_running(running);
            }
            queue.check().unwrap();
        }
        service
    }

    #[test]
    fn equal_weights_share_equally() {
        let mut queue = queue();
        for key in 0..3 {
            queue.enqueue(key, weight(0), Placement::New).unwrap();
        }
        let service = run(&mut queue, 3_000, 1_000_000);
        for &got in &service[..3] {
            assert!(got.abs_diff(1_000_000_000) <= 10_000_000, "{service:?}");
        }
    }

    #[test]
    fn weights_set_the_shares() {
        let mut queue = queue();
        let nices = [0i8, 5, -5];
        for (key, &nice) in nices.iter().enumerate() {
            queue.enqueue(key as u32, weight(nice), Placement::New).unwrap();
        }
        let total_weight: u64 = nices.iter().map(|&n| weight(n)).sum();
        let total_ns = 4_000_000_000u64;
        let service = run(&mut queue, 4_000, 1_000_000);
        for (key, &nice) in nices.iter().enumerate() {
            let expected = total_ns as f64 * weight(nice) as f64 / total_weight as f64;
            let got = service[key] as f64;
            assert!((got - expected).abs() / expected < 0.03, "nice {nice}: {got} vs {expected}");
        }
    }

    #[test]
    fn a_waking_entity_with_an_earlier_deadline_preempts() {
        let mut queue = queue();
        let first = queue.enqueue(0, weight(0), Placement::New).unwrap();
        queue.set_running(Some(first));
        // Past its first deadline, so it has a whole slice ahead now.
        assert!(queue.charge(2_000_000));
        // A newcomer gets half a slice, so its deadline comes first.
        queue.enqueue(1, weight(0), Placement::New).unwrap();
        assert!(queue.should_preempt());
        // One that slept with negative lag must wait its turn.
        let mut queue2 = self::queue();
        let a = queue2.enqueue(0, weight(0), Placement::New).unwrap();
        queue2.set_running(Some(a));
        queue2.charge(500_000);
        queue2.enqueue(1, weight(0), Placement::Lag(-2_000_000)).unwrap();
        assert!(!queue2.should_preempt());
    }

    #[test]
    fn the_running_entity_knows_its_time_left() {
        let mut queue = queue();
        let a = queue.enqueue(1, 2048, Placement::Lag(0)).unwrap();
        queue.set_running(Some(a));
        // A whole slice of real time, whatever the weight.
        assert_eq!(queue.running_left_ns(), Some(BASE_SLICE_NS));
        queue.charge(1_000_000);
        assert_eq!(queue.running_left_ns(), Some(BASE_SLICE_NS - 1_000_000));
    }

    #[test]
    fn yielding_lets_the_others_go_first() {
        let mut queue = queue();
        let a = queue.enqueue(1, 1024, Placement::New).unwrap();
        queue.enqueue(2, 1024, Placement::New).unwrap();
        queue.set_running(Some(a));
        assert_eq!(queue.pick(), Some(a));
        queue.yield_running();
        let pick = queue.pick().unwrap();
        assert_eq!(*queue.key_of(pick).unwrap(), 2);
        queue.check().unwrap();
    }

    #[test]
    fn lag_survives_leaving_and_coming_back() {
        let mut queue = queue();
        for key in 0..3 {
            queue.enqueue(key, weight(0), Placement::New).unwrap();
        }
        run(&mut queue, 7, 1_000_000);
        let running = queue.running().unwrap();
        let lag = queue.lag(running).unwrap();
        let (key, kept) = queue.dequeue(running).unwrap();
        assert_eq!(kept, lag.clamp(-queue.lag_limit(1024), queue.lag_limit(1024)));
        let index = queue.enqueue(key, weight(0), Placement::Lag(kept)).unwrap();
        let back = queue.lag(index).unwrap();
        assert!(back.abs_diff(kept) <= 2, "left with {kept}, back with {back}");
        queue.check().unwrap();
    }

    #[test]
    fn lag_is_clamped() {
        let mut queue = queue();
        let a = queue.enqueue(0, weight(0), Placement::New).unwrap();
        queue.enqueue(1, weight(0), Placement::New).unwrap();
        queue.set_running(Some(a));
        // Run one entity far ahead without ever picking again.
        for _ in 0..100 {
            queue.charge(1_000_000);
        }
        let (_, lag) = queue.dequeue(queue.running().unwrap()).unwrap();
        assert_eq!(lag, -queue.lag_limit(weight(0)));
    }

    #[test]
    fn reweighting_keeps_the_average() {
        let mut queue = queue();
        for key in 0..4 {
            queue.enqueue(key, weight(0), Placement::New).unwrap();
        }
        run(&mut queue, 13, 700_000);
        let before = queue.avg_vruntime();
        queue.reweight(2, weight(-3));
        let after = queue.avg_vruntime();
        assert!(before.abs_diff(after) <= 1, "{before} {after}");
        queue.check().unwrap();
    }

    #[test]
    fn growing_keeps_the_entities() {
        let mut queue: FairQueue<u32, Global> = FairQueue::new_in(Global, BASE_SLICE_NS, TICK);
        assert_eq!(queue.enqueue(7, 1024, Placement::New), Err(7), "no room yet");
        let old = queue.grow(Vec::with_capacity_in(4, Global));
        assert_eq!(old.capacity(), 0);
        for key in 0..4 {
            queue.enqueue(key, 1024, Placement::New).unwrap();
        }
        assert_eq!(queue.enqueue(9, 1024, Placement::New), Err(9));
        let old = queue.grow(Vec::with_capacity_in(8, Global));
        assert_eq!(old.capacity(), 4);
        assert!(old.is_empty());
        assert_eq!(queue.len(), 4);
        queue.enqueue(9, 1024, Placement::New).unwrap();
        queue.check().unwrap();
    }

    #[derive(Clone, Debug)]
    enum Op {
        Enqueue(i8, i64),
        Dequeue(usize),
        Run(u64),
        Reweight(usize, i8),
    }

    fn op() -> impl Strategy<Value = Op> {
        prop_oneof![
            (-20i8..20, -20_000_000i64..20_000_000).prop_map(|(n, l)| Op::Enqueue(n, l)),
            (0usize..16).prop_map(Op::Dequeue),
            (1u64..20_000_000).prop_map(Op::Run),
            (0usize..16, -20i8..20).prop_map(|(i, n)| Op::Reweight(i, n)),
        ]
    }

    proptest! {
        /// Whatever happens, the bookkeeping stays exact, some entity is
        /// always eligible, and nobody's lag grows past what the slices
        /// and the clamp allow.
        #[test]
        fn the_queue_stays_consistent(ops in prop::collection::vec(op(), 1..200)) {
            let mut queue = queue();
            let mut next = 0u32;
            for op in ops {
                match op {
                    Op::Enqueue(nice, lag) => {
                        if queue.len() < 16 {
                            queue.enqueue(next, weight(nice), Placement::Lag(lag)).unwrap();
                            next += 1;
                        }
                    }
                    Op::Dequeue(i) => {
                        if i < queue.len() {
                            if queue.running() == Some(i) {
                                queue.set_running(None);
                            }
                            queue.dequeue(i);
                        }
                    }
                    Op::Run(ns) => {
                        if queue.running().is_none() {
                            let pick = queue.pick();
                            queue.set_running(pick);
                        }
                        if queue.charge(ns) {
                            let pick = queue.pick();
                            queue.set_running(pick);
                        }
                    }
                    Op::Reweight(i, nice) => {
                        if i < queue.len() {
                            queue.reweight(i, weight(nice));
                        }
                    }
                }
                prop_assert_eq!(queue.check(), Ok(()));
                if !queue.is_empty() {
                    prop_assert!(queue.pick().is_some());
                }
            }
        }

        /// Entities that only run (no sleeping) get service in proportion
        /// to their weights, to within a few slices.
        #[test]
        fn service_follows_the_weights(nices in prop::collection::vec(-10i8..10, 2..6)) {
            let mut queue = queue();
            for (key, &nice) in nices.iter().enumerate() {
                queue.enqueue(key as u32, weight(nice), Placement::New).unwrap();
            }
            let steps = 20_000;
            let service = run(&mut queue, steps, 250_000);
            let total: u64 = nices.iter().map(|&n| weight(n)).sum();
            for (key, &nice) in nices.iter().enumerate() {
                let expected = (steps as u64 * 250_000) as f64 * weight(nice) as f64 / total as f64;
                let slack = 8.0 * BASE_SLICE_NS as f64;
                prop_assert!((service[key] as f64 - expected).abs() <= slack,
                    "nice {}: {} vs {}", nice, service[key], expected);
            }
        }
    }
}
