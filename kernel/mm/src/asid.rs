// SPDX-FileCopyrightText: 2026 The OrthrOS contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Address-space identifiers: x86-64 PCIDs.
//!
//! With PCIDs, the TLB tags each entry with the identifier of the address
//! space that made it, so switching address spaces need not flush the TLB.
//! There are only 4096 identifiers, so they are handed out in
//! *generations*, the scheme Linux uses for arm64 ASIDs: an address space
//! keeps its identifier while the generation lasts; when every identifier
//! of a generation is taken, the next generation starts, every CPU flushes
//! its TLB the next time it switches, and address spaces get identifiers
//! anew as they next run. Spaces running at that moment keep theirs, so a
//! CPU never runs with an identifier another space also holds.
//!
//! Identifier 0 is never handed out: it tags the kernel's own tables.
//!
//! Each address space keeps its identifier in an `AtomicU64` it owns
//! (generation in the high bits, identifier in the low `BITS`), zero until
//! it first runs. The allocator is generic over the identifier width and
//! the number of CPUs, so that the Kani proofs can check it with a handful
//! of each.

#![forbid(unsafe_code)]

use core::sync::atomic::{AtomicU64, Ordering};

use radix_sync::SpinLock;

use crate::platform::MAX_CPUS;

const MAP_WORDS: usize = 64;

struct State<const CPUS: usize> {
    /// Identifiers taken in this generation.
    map: [u64; MAP_WORDS],
    /// Where the search for a free identifier resumes.
    next: u64,
    /// CPUs that must flush their TLB at their next switch.
    flush_pending: [bool; CPUS],
}

/// The identifier allocator for `CPUS` CPUs.
pub struct Asids<const BITS: u32, const CPUS: usize> {
    generation: AtomicU64,
    /// The context value each CPU runs with; 0 after a rollover until the
    /// CPU switches again.
    active: [AtomicU64; CPUS],
    /// The value each CPU ran with at the last rollover.
    reserved: [AtomicU64; CPUS],
    state: SpinLock<State<CPUS>>,
}

/// The x86-64 PCID allocator: 12-bit identifiers.
pub type Pcids = Asids<12, MAX_CPUS>;

impl<const BITS: u32, const CPUS: usize> Asids<BITS, CPUS> {
    const IDS: u64 = 1 << BITS;
    const MASK: u64 = Self::IDS - 1;

    /// A fresh allocator, in generation 1.
    pub const fn new() -> Self {
        assert!(BITS >= 1 && BITS <= 12);
        let mut map = [0; MAP_WORDS];
        map[0] = 1;
        Asids {
            generation: AtomicU64::new(Self::IDS),
            active: [const { AtomicU64::new(0) }; CPUS],
            reserved: [const { AtomicU64::new(0) }; CPUS],
            state: SpinLock::new(State { map, next: 1, flush_pending: [false; CPUS] }),
        }
    }

    /// The identifier part of a context value.
    pub fn id(context: u64) -> u16 {
        (context & Self::MASK) as u16
    }

    /// The current generation (for reports and tests).
    pub fn generation(&self) -> u64 {
        self.generation.load(Ordering::Relaxed) >> BITS
    }

    fn current(&self, context: u64) -> bool {
        (context ^ self.generation.load(Ordering::Relaxed)) >> BITS == 0
    }

    /// Gives the address space whose context value is `context` an
    /// identifier valid on `cpu`, and records that `cpu` runs it. Returns
    /// the identifier and whether `cpu` must flush its whole TLB (all
    /// identifiers) before using it. Call with interrupts disabled, while
    /// switching to the space.
    pub fn switch(&self, cpu: usize, context: &AtomicU64) -> (u16, bool) {
        let Some(active) = self.active.get(cpu) else { return (0, true) };
        let value = context.load(Ordering::Relaxed);
        let old = active.load(Ordering::Relaxed);
        // Fast path: the space's identifier is current, and no rollover
        // has reset this CPU's slot meanwhile (it would be 0).
        if old != 0
            && value != 0
            && self.current(value)
            && active.compare_exchange(old, value, Ordering::Relaxed, Ordering::Relaxed).is_ok()
        {
            return (Self::id(value), false);
        }
        let mut state = self.state.lock();
        let mut value = context.load(Ordering::Relaxed);
        if value == 0 || !self.current(value) {
            value = self.new_context(&mut state, value);
            context.store(value, Ordering::Relaxed);
        }
        let flush = core::mem::take(&mut state.flush_pending[cpu]);
        active.store(value, Ordering::Relaxed);
        (Self::id(value), flush)
    }

    /// Records that `cpu` stopped running an address space with an
    /// identifier (it switched to the kernel's own tables).
    pub fn leave(&self, cpu: usize) {
        if let Some(active) = self.active.get(cpu) {
            // Keep the slot non-zero, since 0 means "rolled over"; if a
            // rollover just reset it, leave it at 0.
            let value = active.load(Ordering::Relaxed);
            if value != 0 {
                let idle = self.generation.load(Ordering::Relaxed);
                let _ = active.compare_exchange(value, idle, Ordering::Relaxed, Ordering::Relaxed);
            }
        }
    }

    fn new_context(&self, state: &mut State<CPUS>, old: u64) -> u64 {
        let generation = self.generation.load(Ordering::Relaxed);
        if old != 0 {
            let fresh = generation | (old & Self::MASK);
            // A CPU was running the space at the rollover: it keeps its
            // identifier.
            if self.update_reserved(old, fresh) {
                return fresh;
            }
            // Otherwise reuse the old identifier if it is still free.
            if !test_and_set(&mut state.map, old & Self::MASK) {
                return fresh;
            }
        }
        let id = match find_free(&state.map, state.next, Self::IDS) {
            Some(id) => id,
            None => {
                self.rollover(state);
                // At most one identifier per CPU survives a rollover, and
                // there are more identifiers than CPUs... unless BITS is
                // tiny; then some may be shared, which `reserved` handles.
                find_free(&state.map, 1, Self::IDS).unwrap_or(0)
            }
        };
        if id == 0 {
            // No identifier at all (only possible with tiny BITS and many
            // CPUs): run untagged, flushing on every switch.
            return self.generation.load(Ordering::Relaxed);
        }
        set(&mut state.map, id);
        state.next = id;
        self.generation.load(Ordering::Relaxed) | id
    }

    fn update_reserved(&self, old: u64, fresh: u64) -> bool {
        let mut hit = false;
        for reserved in &self.reserved {
            if reserved.load(Ordering::Relaxed) == old {
                reserved.store(fresh, Ordering::Relaxed);
                hit = true;
            }
        }
        hit
    }

    /// Starts a new generation: only the identifiers CPUs are running stay
    /// taken, and every CPU flushes at its next switch.
    fn rollover(&self, state: &mut State<CPUS>) {
        let generation = self.generation.load(Ordering::Relaxed) + Self::IDS;
        self.generation.store(generation, Ordering::Relaxed);
        state.map = [0; MAP_WORDS];
        state.map[0] = 1;
        for (active, reserved) in self.active.iter().zip(&self.reserved) {
            let mut value = active.swap(0, Ordering::Relaxed);
            if value == 0 {
                // The CPU has not switched since the last rollover.
                value = reserved.load(Ordering::Relaxed);
            }
            if value & Self::MASK != 0 {
                set(&mut state.map, value & Self::MASK);
            }
            reserved.store(value, Ordering::Relaxed);
        }
        state.flush_pending = [true; CPUS];
        state.next = 1;
    }
}

impl<const BITS: u32, const CPUS: usize> Default for Asids<BITS, CPUS> {
    fn default() -> Self {
        Self::new()
    }
}

fn set(map: &mut [u64; MAP_WORDS], id: u64) {
    map[(id / 64) as usize] |= 1 << (id % 64);
}

fn test_and_set(map: &mut [u64; MAP_WORDS], id: u64) -> bool {
    let (word, bit) = ((id / 64) as usize, 1 << (id % 64));
    let was = map[word] & bit != 0;
    map[word] |= bit;
    was
}

/// The first identifier below `ids` not in `map`, searching from `from`
/// and wrapping around once.
fn find_free(map: &[u64; MAP_WORDS], from: u64, ids: u64) -> Option<u64> {
    let free = |id: u64| map[(id / 64) as usize] & (1 << (id % 64)) == 0;
    (from..ids).chain(1..from.min(ids)).find(|&id| free(id))
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    #[test]
    fn spaces_keep_their_identifier_until_a_rollover() {
        let asids = Asids::<2, 4>::new();
        let spaces: Vec<AtomicU64> = (0..4).map(|_| AtomicU64::new(0)).collect();
        let (a, flush) = asids.switch(0, &spaces[0]);
        assert_eq!((a, flush), (1, false));
        assert_eq!(asids.switch(0, &spaces[1]).0, 2);
        assert_eq!(asids.switch(0, &spaces[0]), (1, false));
        assert_eq!(asids.switch(1, &spaces[2]).0, 3);
        // Identifiers 1-3 are taken: the fourth space forces a rollover.
        // CPU 0 runs space 0 (identifier 1) and CPU 1 space 2 (3), so
        // those stay reserved and space 3 gets 2.
        asids.switch(0, &spaces[0]);
        let (id, flush) = asids.switch(0, &spaces[3]);
        assert_eq!((id, flush), (2, true));
        assert_eq!(asids.generation(), 2);
        // Space 2 still runs on CPU 1 and keeps 3; CPU 1 flushes once.
        assert_eq!(asids.switch(1, &spaces[2]), (3, true));
        assert_eq!(asids.switch(1, &spaces[2]), (3, false));
    }

    proptest! {
        /// However spaces are switched, two spaces never run with the same
        /// identifier in the same generation, and a CPU flushes before it
        /// uses an identifier that was another space's in an earlier
        /// generation.
        #[test]
        fn running_spaces_never_share_an_identifier(
            switches in proptest::collection::vec((0usize..4, 0usize..6), 1..300),
        ) {
            let asids = Asids::<3, 4>::new();
            let spaces: Vec<AtomicU64> = (0..6).map(|_| AtomicU64::new(0)).collect();
            // What each CPU's TLB may hold: identifier -> space.
            let mut tlb: Vec<std::collections::HashMap<u16, usize>> = vec![Default::default(); 4];
            let mut running = [None; 4];
            for (cpu, space) in switches {
                let (id, flush) = asids.switch(cpu, &spaces[space]);
                prop_assert!(id != 0);
                if flush {
                    tlb[cpu].clear();
                }
                // No stale entries of another space under this identifier.
                if let Some(&other) = tlb[cpu].get(&id) {
                    prop_assert_eq!(other, space);
                }
                tlb[cpu].insert(id, space);
                running[cpu] = Some((space, id));
                for (other_cpu, other) in running.iter().enumerate() {
                    if let Some((other_space, other_id)) = *other
                        && other_space != space
                    {
                        prop_assert!(other_id != id, "cpu{cpu} and cpu{other_cpu} share {id}");
                    }
                }
            }
        }
    }
}
