<!--
SPDX-FileCopyrightText: 2026 The Dukovo contributors
SPDX-License-Identifier: Apache-2.0 OR MIT
-->

# 0156. A disk request's thread is woken from its slot, and a lending takes nothing from the heap

- Status: Accepted
- Date: 2026-10-29

## Context

On an Intel Xeon Platinum 8573C runner the Benchmarks job of #86 stayed
over the gate after 9 runs: a 4 KiB raw write to a virtio disk took
61.0 µs against Linux's 59.8 (1.02), and the 4 KiB read was 0.97. On an
AMD EPYC 7763 runner the same rows were 0.92 and 0.96. Most of each
operation is the device's time; the rest is the kernel's path to the
doorbell and back from the interrupt, which a profile under QEMU's
instruction counting put at 9,347 instructions a write and 9,172 a
read.

A good part of that went on waking threads no one needed woken:

- Each request woke through the scheduler's table of waiters (ADR
  0027) three times: its completion woke the thread on the slot's key
  and the threads waiting for room on the lane's key, and freeing the
  slot woke the lane's key again. Each wake-up takes the table's lock
  and looks through every waiter in the machine, as does the waiting
  thread when it leaves the table after waking. With no thread waiting
  for room, two of the three found no one.
- Each lending of a program's pages (ADR 0111) listed them on the
  heap, and its end woke a fork that might wait for it, through the
  table, whether one did or not.
- Each raw write asked whether the disk holds a swap area that is on
  (ADR 0152), under a lock, with no swap area on.

## Decision

- The thread waiting for a request is named in a slot of its own,
  under a lock of its own, for the completion to wake directly: the
  completion sets the slot's state, then takes the lock and wakes the
  thread named there. The thread names itself under that lock before it
  looks at the state, so either it sees the state set, or the
  completion finds it named. A wake-up that comes before the thread
  sleeps makes its sleep return at once (`sched::block`).
- Each lane counts the threads waiting for room. A completion or a slot
  freed moves the lane's count of events and wakes through the table
  only when that count is above zero; both sides use sequentially
  consistent operations, so either the waiter sees the move or the
  waker sees it counted.
- A lending of up to 16 pages (64 KiB) lists them on the stack; a
  longer one on the heap, as before. A fork that waits for lendings
  sets the top bit of the space's count of them first, and only the
  last lending to end with that bit set wakes it.
- The swap areas that are on are counted, and with none the question
  is answered without the lock.

## Alternatives

- **Hash the table of waiters by key.** Every wait would gain, not only
  a disk's, but it changes the scheduler's waiting, which
  `kernel/sched/tla/Scheduler.tla` models, and a request's thread would
  still go through a lock and a list for a wake-up that names one
  thread.
- **Poll for the completion instead of halting.** ADR 0116 found that a
  device's interrupt reaches a halted CPU sooner in the CI runners'
  nested virtual machines.

## Consequences

- Under QEMU's instruction counting a 4 KiB raw write to a virtio disk
  takes 8,008 instructions and a read 7,930, 14% and 13.5% fewer; the
  scheduler's table is no longer touched on a request's way.
- A disk holds a lock and a thread reference for each of its slots, on
  the heap (64 for each lane).
- A thread woken from its slot after it stopped waiting (the completion
  found it named just before it left) has its next sleep return at
  once, as any early wake-up does; every sleep is in a loop that looks
  again.
