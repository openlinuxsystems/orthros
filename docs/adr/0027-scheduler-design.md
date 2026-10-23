<!--
SPDX-FileCopyrightText: 2026 The Dukovo contributors
SPDX-License-Identifier: Apache-2.0 OR MIT
-->

# 0027. The scheduler is EEVDF with FIFO and round-robin classes, per-CPU queues and no lock across a switch

- Status: Accepted
- Date: 2026-10-21

## Context

The roadmap's Phase 4 asks for a context switch with eager XSAVE, kernel
threads, per-CPU run queues, idle with MWAIT or HLT, an EEVDF fair class,
FIFO and round-robin real-time classes, priority inheritance, affinity,
and load balancing that respects caches and NUMA distance. Resource
domains (spec 4.1) need CPU weights. The open questions were how the
policy and the mechanism are split, how a switch avoids racing a wake-up
on another CPU, how domain weights enter the fair class, and how the
kernel's own helper threads are scheduled.

## Decision

- The policy is a crate of its own, `radix-sched`: the fair queue, the
  real-time queue, nice weights, CPU sets and the topology with its
  placement and balancing decisions, as plain safe data structures that
  run and are tested on the host and checked by Kani. The kernel's
  `sched` module is the mechanism: threads, locks, the switch, waiting,
  timers and balancing.
- Every CPU has one run queue under one spin lock, with a real-time queue
  ahead of an EEVDF fair queue. The running thread stays in its queue.
  Queues are vectors with room made ahead of time, outside every lock,
  so nothing in the scheduler allocates under a lock.
- No lock is held across a switch. Each context has an `on_cpu` word: the
  switch lets go of the outgoing one before it takes the incoming one,
  waiting if another CPU still holds it, and touches no stack in
  between. Blocking and waking meet under the thread's lock with a
  `wake_pending` flag set before that lock is taken. The TLA+ model in
  `kernel/sched/tla` checks this protocol (ADR 0030).
- Extended state is switched eagerly on every switch, with XSAVEOPT where
  available.
- Domain CPU weights scale each thread's weight in its CPU's fair queue:
  a domain's share on a CPU is split between its own threads and its
  child domains by weight. This is one flat queue per CPU, not a queue
  per domain per CPU.
- Mutexes hand off to the most urgent waiter. Real-time waiters lend
  their priority to the owner and along the chain of owners (up to 16);
  fair waiters lend nothing.
- Because a handoff favours real-time waiters, the kernel's `work/N`
  workers run in the fair class at nice -20, not as real-time threads: a
  real-time worker taking a mutex in a loop would starve a fair thread
  waiting for it (the `caps` self-test's revoker did starve that way).
- Idle CPUs use MWAIT where CPUID offers it, so a waker only writes the
  monitored line, and HLT with a reschedule IPI otherwise.

## Consequences

- The scheduling decisions have host tests, property tests and Kani
  proofs, without hardware or locks in the way.
- Wake-ups never wait for a remote CPU to finish switching, except for
  the few instructions of the switch's first half.
- Flat domain weights give exact shares among the threads runnable on one
  CPU, but do not move time between CPUs to even out domains. A queue per
  domain per CPU replaces them if containers (Phase 20) need exact
  machine-wide shares.
- Picking scans the queue, which is fine for Phase 4's short queues and
  becomes a tree when queues grow.
- Revisit priority inheritance for fair threads if Phase 6 services show
  inversions between fair threads that the fair class does not bound well
  enough.
