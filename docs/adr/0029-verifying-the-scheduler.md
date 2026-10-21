# 0029. The scheduler is verified with a TLA+ model of its switch protocol, Kani, and algebra backed by tests

- Status: Accepted
- Date: 2026-10-21

## Context

Spec 15 asks for a semi-formal correctness argument for the scheduler, as
for the memory manager (ADR 0019) and the capability core (ADR 0023).
Its hardest parts are concurrent: switching without a lock across the
switch, and blocking and waking without losing a wake-up. Its policy
properties are arithmetic: that a fair queue always has an eligible
entity, and that balancing never undoes itself. Kani, which uses SAT
solving, handles the policy's data structures but takes impractically
long on products of symbolic 64-bit values.

## Decision

- The block, wake and switch protocol is modelled in TLA+
  (`kernel/sched/tla/Scheduler.tla`) and checked with TLC (ADR 0024): its
  invariants say a thread's stack is used by one CPU at most, the run
  queues are consistent and no wake-up is lost, and its liveness property
  says every wait ends under a fair scheduler. Two configurations, one
  for safety with symmetry and one smaller for liveness, each finish in
  about two minutes on 4 CPUs.
- Kani proves the properties of the policy that are not arithmetic: the
  nice weight table, CPU set operations, and the real-time pick.
- The arithmetic properties are argued algebraically in
  `docs/design/scheduler.md` and checked by host tests: exhaustively for
  small values (balancing), and by property tests that audit the fair
  queue after every random operation.
- The design note's argument names, for each step, the check that backs
  it, and the boot tests exercise each step on 4 CPUs.

## Consequences

- A change to the switch, `block` or `wake` should be mirrored in the
  model, whose mapping to the code is in the design note.
- The algebraic steps rest on the argument and on tests rather than on a
  proof tool; a checker that handles nonlinear integer arithmetic (an SMT
  backend for Kani, for instance) could take them over.
- Timing properties (real-time latency, fairness over wall-clock time)
  are measured by the boot tests, not proved.
