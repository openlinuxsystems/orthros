<!--
SPDX-FileCopyrightText: 2026 The Dukovo contributors
SPDX-License-Identifier: Apache-2.0 OR MIT
-->

# 0034. radix-ring batches operations through shared submission and completion queues, after io_uring, and trusts nothing the program writes

- Status: Accepted
- Date: 2026-10-21

## Context

Phase 5 asks for `radix-ring`. Later phases put storage, networking and
graphics behind services; a program that makes one system call per
operation pays for an entry and an exit each time. Linux's io_uring (by
Jens Axboe) showed that a pair of queues in memory shared with the kernel
lets a program submit many operations and collect their results with one
system call, or none. The same shared memory is an attack surface: the
program can write anything into the indexes at any moment.

## Decision

- A ring is a kernel object with a capability. `ring_setup` makes one of
  2ⁿ submission entries (up to 4,096) and twice as many completion
  entries, in pages the kernel allocates, charges to the creating
  thread's resource domain and keeps mapped in its own half; it maps
  them read-write into the program. The header gives the four indexes,
  each on a cache line of its own, and the entries' offsets.
- `ring_enter` takes up to `submit` new entries and waits until
  `min_complete` completions are waiting, or a timeout. The kernel keeps
  its own submission head and completion tail and masks every index the
  program wrote, and takes an entry only after reserving room for its
  completion, so nothing the program writes makes the kernel write
  outside the ring or drop a completion. A full completion queue stops
  submission (`busy` if nothing was taken); a submission tail more than a
  queue ahead is `invalid_argument`.
- Entries are copied out of the shared pages once, in batches of 16
  under the ring's lock, carried out without it, and completed under it,
  each completion written before its tail is published with release
  ordering. The kernel reads and writes the pages through its own
  mapping with volatile accesses, never through the program's.
- Phase 5's operations are `nop`, `notify`, `wait` (a notification, with
  no thread blocked), `msg_ring` (a completion posted to another ring:
  ring-to-ring messages), `random` and `timeout`. I/O operations come
  with the services that do I/O.
- The accounting is the `radix-ipc` crate's `ring` module, proved with
  Kani for every index the program could write.

## Consequences

- Programs can batch work and wait for many things at once without one
  thread per wait; the `ring` pair of the self-tests ping-pongs with
  `msg_ring`, and the benchmarks measure it (ADR 0035).
- Rings are the one IPC object charged to a resource domain in Phase 5,
  since their memory is the largest a program can ask for.
- There is no kernel thread polling the submission queue (io_uring's
  SQPOLL); a submission always needs `ring_enter`. Polling can be added
  as a flag if the benchmarks show the system call matters.
- Entries start in submission order and nothing links one to another
  (io_uring's linked entries); links come with the I/O operations that
  need them.
