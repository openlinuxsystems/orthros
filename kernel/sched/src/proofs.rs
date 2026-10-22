// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Kani proof harnesses for the scheduling policy.
//!
//! Each harness holds for every value its `kani::any()` inputs can take.
//! `ob test kani` runs them; `docs/design/scheduler.md` lists what each
//! establishes and how the argument uses it.
//!
//! The policy's arithmetic properties (that a fair queue always has an
//! eligible entity, that balancing never reverses an imbalance) multiply
//! symbolic 64-bit values, which SAT-based model checking cannot do in
//! reasonable time; they are argued algebraically in the design note and
//! checked by the host tests, exhaustively for small values and with
//! property tests beyond.

use alloc::alloc::Global;

use crate::cpuset::{CpuSet, MAX_CPUS};
use crate::rt::RtQueue;
use crate::weight::{NICE_MAX, NICE_MIN, WEIGHTS, weight};

/// Each nice level weighs 1.25 times the next, to within rounding, and
/// weights strictly fall as nice rises.
#[kani::proof]
fn nice_levels_are_a_quarter_apart() {
    let nice: i8 = kani::any_where(|&n: &i8| (NICE_MIN..NICE_MAX).contains(&n));
    let (more, less) = (weight(nice), weight(nice + 1));
    assert!(more > less && less > 0);
    assert!((more * 4).abs_diff(less * 5) <= 5);
    assert_eq!(WEIGHTS[20], 1024);
}

/// Inserting a CPU puts exactly that CPU in the set, and removing it takes
/// exactly that CPU out.
#[kani::proof]
fn cpu_sets_insert_and_remove_one_cpu() {
    let words: [u64; MAX_CPUS / 64] = kani::any();
    let cpu: usize = kani::any_where(|&c| c < MAX_CPUS);
    let other: usize = kani::any_where(|&c| c < MAX_CPUS && c != cpu);
    let set = CpuSet::from_words(words);
    let mut added = set;
    added.insert(cpu);
    assert!(added.contains(cpu));
    assert_eq!(added.contains(other), set.contains(other));
    assert!(set.is_subset(&added));
    let mut removed = set;
    removed.remove(cpu);
    assert!(!removed.contains(cpu));
    assert_eq!(removed.contains(other), set.contains(other));
    assert!(removed.is_subset(&set));
    assert!(set.and(&added) == set);
}

/// The real-time pick has the highest priority queued, and among equals
/// the one queued first.
#[kani::proof]
#[kani::unwind(5)]
fn real_time_picks_the_most_urgent_first_comer() {
    let mut queue: RtQueue<u8, Global> = RtQueue::new_in(Global);
    queue.try_reserve(3).unwrap();
    let prios: [u8; 3] = [
        kani::any_where(|&p| (1..=99).contains(&p)),
        kani::any_where(|&p| (1..=99).contains(&p)),
        kani::any_where(|&p| (1..=99).contains(&p)),
    ];
    for (key, &prio) in prios.iter().enumerate() {
        queue.enqueue(key as u8, prio, false).unwrap();
    }
    let pick = queue.pick().unwrap();
    let key = *queue.key_of(pick).unwrap() as usize;
    for (other, &prio) in prios.iter().enumerate() {
        assert!(prio <= prios[key]);
        if prio == prios[key] {
            assert!(key <= other);
        }
    }
}
