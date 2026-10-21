// SPDX-FileCopyrightText: 2026 The OrthrOS contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Load balancing: CPUs with less to do pull waiting threads from busier
//! ones.
//!
//! Each CPU looks at the levels of the topology from the closest out
//! (`radix_sched::Topology`): the hardware threads of its core, its
//! package, its NUMA node, the machine. At each level it compares its
//! group of CPUs with the others there and, if one is busier by enough
//! that moving a thread makes things more even (`should_pull`), takes a
//! waiting thread from that group's busiest CPU. A busy CPU tries every
//! [`interval_ticks`] ticks of a level, so farther levels are balanced less
//! often; an idle CPU tries every level before it sleeps. Threads whose
//! caches are probably still warm are left where they are, unless
//! attempts at that level have failed several times in a row.

use core::sync::atomic::Ordering;

use radix_sched::{Level, interval_ticks};

use super::{CPUS, TOPOLOGY, adopt, loads};

/// Failed attempts at a level after which a cache-hot thread may move.
const FORCE_AFTER: u32 = 4;

fn level_index(level: Level) -> usize {
    match level {
        Level::Core => 0,
        Level::Package => 1,
        Level::Node => 2,
        Level::Machine => 3,
    }
}

/// From the timer tick on CPU `cpu`, with interrupts off.
pub(super) fn tick(cpu: usize, now: u64) {
    let Some(topology) = TOPOLOGY.get() else { return };
    let ticks = CPUS.get(cpu).ticks.load(Ordering::Relaxed);
    for level in topology.levels(cpu) {
        if ticks.is_multiple_of(interval_ticks(level)) && pull(cpu, level, now) {
            return;
        }
    }
}

/// From the idle loop of CPU `cpu`, with interrupts off: tries every
/// level, closest first, until a thread comes.
pub(super) fn idle(cpu: usize, now: u64) -> bool {
    let Some(topology) = TOPOLOGY.get() else { return false };
    topology.levels(cpu).any(|level| pull(cpu, level, now))
}

/// Tries to pull one thread to `cpu` across `level`.
fn pull(cpu: usize, level: Level, now: u64) -> bool {
    let Some(topology) = TOPOLOGY.get() else { return false };
    let loads = loads();
    let Some(busiest) = topology.find_busiest(cpu, level, &loads) else { return false };
    if busiest == cpu {
        return false;
    }
    let failures = &CPUS.get(cpu).balance_failures[level_index(level)];
    let force = failures.load(Ordering::Relaxed) >= FORCE_AFTER;
    // Both queues, in CPU order.
    let (first, second) = (cpu.min(busiest), cpu.max(busiest));
    let mut low = CPUS.get(first).rq.lock();
    let mut high = CPUS.get(second).rq.lock_nested(1);
    let (Some(low_rq), Some(high_rq)) = (low.as_mut(), high.as_mut()) else { return false };
    let (here, there) = if first == cpu { (low_rq, high_rq) } else { (high_rq, low_rq) };
    match there.take_for(cpu, now, level, force) {
        Some((thread, class)) => {
            adopt(here, cpu, thread, class);
            here.pulled[level_index(level)] += 1;
            CPUS.get(cpu).publish(here);
            CPUS.get(busiest).publish(there);
            failures.store(0, Ordering::Relaxed);
            let preempt = here.wants_preempt();
            drop(high);
            drop(low);
            if preempt {
                CPUS.get(cpu).need_resched.store(true, Ordering::Relaxed);
            }
            true
        }
        None => {
            failures.fetch_add(1, Ordering::Relaxed);
            false
        }
    }
}
