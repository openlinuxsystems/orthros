// SPDX-FileCopyrightText: 2026 The OrthrOS contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! The real-time classes: FIFO and round-robin queues by priority.
//!
//! A real-time thread runs ahead of every fair thread. Among real-time
//! threads the highest priority (1 to 99) runs; among equals, the one that
//! has waited longest. A FIFO thread keeps the CPU until it blocks, yields
//! or a higher priority wakes up; a round-robin thread also goes to the back
//! of its priority after each quantum ([`RR_QUANTUM_NS`]). A thread that is
//! preempted by a higher priority keeps its place at the front.
//!
//! As in the fair queue, the running thread stays in the queue, entries
//! live in a vector the kernel grows ahead of time, and picking scans it.

use alloc::vec::Vec;
use core::alloc::Allocator;

/// How long a round-robin thread runs before others of its priority get a
/// turn.
pub const RR_QUANTUM_NS: u64 = 100_000_000;

/// One queued real-time thread.
#[derive(Clone, Debug)]
pub struct RtEntry<K> {
    key: K,
    prio: u8,
    /// Order within a priority: lower runs first.
    order: i64,
}

impl<K> RtEntry<K> {
    /// What the entry stands for.
    pub fn key(&self) -> &K {
        &self.key
    }

    /// Its priority.
    pub fn prio(&self) -> u8 {
        self.prio
    }
}

/// A run queue of real-time entries named by `K`.
pub struct RtQueue<K, A: Allocator> {
    entries: Vec<RtEntry<K>, A>,
    running: Option<usize>,
    /// The next order for the back of a priority.
    back: i64,
    /// The next order for the front.
    front: i64,
}

impl<K, A: Allocator> RtQueue<K, A> {
    /// An empty queue.
    pub fn new_in(alloc: A) -> Self {
        Self { entries: Vec::new_in(alloc), running: None, back: 1, front: 0 }
    }

    /// How many entries are queued, the running one included.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether the queue is empty.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// How many entries fit without growing.
    pub fn capacity(&self) -> usize {
        self.entries.capacity()
    }

    /// As [`crate::FairQueue::grow`].
    pub fn grow(&mut self, mut buffer: Vec<RtEntry<K>, A>) -> Vec<RtEntry<K>, A> {
        if !buffer.is_empty() || buffer.capacity() <= self.entries.capacity() {
            return buffer;
        }
        while let Some(item) = self.entries.pop() {
            // The buffer is larger, so this always fits.
            let _ = buffer.push_within_capacity(item);
        }
        buffer.reverse();
        core::mem::replace(&mut self.entries, buffer)
    }

    /// Makes room for `additional` more entries, allocating here.
    pub fn try_reserve(
        &mut self,
        additional: usize,
    ) -> Result<(), alloc::collections::TryReserveError> {
        self.entries.try_reserve(additional)
    }

    /// The entries, in no particular order.
    pub fn entries(&self) -> &[RtEntry<K>] {
        &self.entries
    }

    /// Adds an entry at priority `prio`: at the front of that priority
    /// (`front`, for a preempted thread put back) or at the back. Returns
    /// its index, or gives `key` back when the queue is full.
    pub fn enqueue(&mut self, key: K, prio: u8, front: bool) -> Result<usize, K> {
        let order = if front {
            self.front -= 1;
            self.front
        } else {
            self.back += 1;
            self.back
        };
        self.entries.push_within_capacity(RtEntry { key, prio, order }).map_err(|e| e.key)?;
        Ok(self.entries.len() - 1)
    }

    /// Removes entry `index`.
    pub fn dequeue(&mut self, index: usize) -> Option<RtEntry<K>> {
        if index >= self.entries.len() {
            return None;
        }
        let entry = self.entries.swap_remove(index);
        match self.running {
            Some(running) if running == index => self.running = None,
            Some(running) if running == self.entries.len() => self.running = Some(index),
            _ => {}
        }
        Some(entry)
    }

    /// The index of the first entry whose key satisfies `matches`.
    pub fn position(&self, mut matches: impl FnMut(&K) -> bool) -> Option<usize> {
        self.entries.iter().position(|e| matches(&e.key))
    }

    /// Marks entry `index` as the one on the CPU, or none.
    pub fn set_running(&mut self, index: Option<usize>) {
        self.running = index.filter(|&i| i < self.entries.len());
    }

    /// The index of the running entry.
    pub fn running(&self) -> Option<usize> {
        self.running
    }

    /// Moves entry `index` to the back of its priority: a yield, or a
    /// round-robin quantum running out.
    pub fn to_back(&mut self, index: usize) {
        self.back += 1;
        let back = self.back;
        if let Some(entry) = self.entries.get_mut(index) {
            entry.order = back;
        }
    }

    /// Changes the priority of entry `index`, putting it at the back of the
    /// new priority (or the front, for `front`).
    pub fn set_prio(&mut self, index: usize, prio: u8, front: bool) {
        let order = if front {
            self.front -= 1;
            self.front
        } else {
            self.back += 1;
            self.back
        };
        if let Some(entry) = self.entries.get_mut(index) {
            entry.prio = prio;
            entry.order = order;
        }
    }

    /// The entry that should run: the highest priority, then the lowest
    /// order.
    pub fn pick(&self) -> Option<usize> {
        let mut best: Option<usize> = None;
        for (index, entry) in self.entries.iter().enumerate() {
            best = match best {
                Some(b)
                    if (self.entries[b].prio, -self.entries[b].order)
                        >= (entry.prio, -entry.order) =>
                {
                    Some(b)
                }
                _ => Some(index),
            };
        }
        best
    }

    /// The highest queued priority.
    pub fn top_prio(&self) -> Option<u8> {
        self.entries.iter().map(|e| e.prio).max()
    }

    /// The key of entry `index`.
    pub fn key_of(&self, index: usize) -> Option<&K> {
        self.entries.get(index).map(|e| &e.key)
    }

    /// The priority of entry `index`.
    pub fn prio_of(&self, index: usize) -> Option<u8> {
        self.entries.get(index).map(|e| e.prio)
    }
}

#[cfg(test)]
mod tests {
    use std::alloc::Global;

    use proptest::prelude::*;

    use super::*;

    fn queue() -> RtQueue<u32, Global> {
        let mut queue = RtQueue::new_in(Global);
        queue.try_reserve(64).unwrap();
        queue
    }

    #[test]
    fn higher_priorities_run_first_then_first_come() {
        let mut queue = queue();
        queue.enqueue(1, 10, false).unwrap();
        queue.enqueue(2, 50, false).unwrap();
        queue.enqueue(3, 50, false).unwrap();
        queue.enqueue(4, 10, true).unwrap();
        let mut order = Vec::new();
        while let Some(index) = queue.pick() {
            order.push(*queue.key_of(index).unwrap());
            queue.dequeue(index);
        }
        assert_eq!(order, [2, 3, 4, 1]);
    }

    #[test]
    fn round_robin_goes_to_the_back() {
        let mut queue = queue();
        for key in 0..3 {
            queue.enqueue(key, 20, false).unwrap();
        }
        let mut order = Vec::new();
        for _ in 0..6 {
            let index = queue.pick().unwrap();
            order.push(*queue.key_of(index).unwrap());
            queue.to_back(index);
        }
        assert_eq!(order, [0, 1, 2, 0, 1, 2]);
    }

    #[test]
    fn boosting_moves_an_entry_up() {
        let mut queue = queue();
        let low = queue.enqueue(1, 5, false).unwrap();
        queue.enqueue(2, 30, false).unwrap();
        queue.set_prio(low, 60, true);
        assert_eq!(queue.key_of(queue.pick().unwrap()), Some(&1));
        assert_eq!(queue.top_prio(), Some(60));
    }

    proptest! {
        /// The pick always has the highest priority, and among those the
        /// earliest arrival.
        #[test]
        fn picks_follow_priority_then_arrival(prios in prop::collection::vec(1u8..100, 1..40)) {
            let mut queue = queue();
            for (key, &prio) in prios.iter().enumerate() {
                queue.enqueue(key as u32, prio, false).unwrap();
            }
            let mut expected: Vec<(u8, usize)> = prios.iter().enumerate().map(|(k, &p)| (p, k)).collect();
            expected.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
            let mut got = Vec::new();
            while let Some(index) = queue.pick() {
                got.push((queue.prio_of(index).unwrap(), *queue.key_of(index).unwrap() as usize));
                queue.dequeue(index);
            }
            prop_assert_eq!(got, expected);
        }
    }
}
