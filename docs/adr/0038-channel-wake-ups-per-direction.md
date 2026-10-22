<!--
SPDX-FileCopyrightText: 2026 The Dukovo contributors
SPDX-License-Identifier: Apache-2.0 OR MIT
-->

# 0038. A channel signals the other end only when it says it may sleep, with one word for each way of waiting

- Status: Accepted
- Date: 2026-10-21

## Context

A channel (ADR 0032) is two single-producer, single-consumer queues in a
shared frame, with a notification for each end. The writer signalled the
reader's notification after every message, whether or not the reader was
waiting, and a reader that found its queue empty waited on its own
notification. A signal nobody waits for is a system call that does
nothing useful, and a signal left over from an earlier message ends the
next wait at once, only for the reader to find nothing and wait again.
On one CPU a round trip took four context switches instead of two, and
it was the one round trip where Radix was slower than Linux's futex
ping-pong, which only calls into the kernel when a waiter has said it is
there. A first fix, one "sleeping" word for each end, still woke the
wrong waiter: an end that was waiting for room to send was signalled
when a message arrived for it, and the other way round.

## Decision

- Each queue has two words beside its indexes: the reader's (next to the
  head) and the writer's (next to the tail, on its own cache line). An
  end that finds its queue empty (to receive) or full (to send) sets the
  matching word, looks at the indexes again, and only then waits on its
  notification; it clears the word once it is done waiting.
- After moving an index, an end signals the other only if the other's
  word for what it just did is set: the reader's word after writing a
  message, the writer's after reading one.
- The two stores and the two loads this rests on (the word's store and
  the second look; the index's store and the word's load) are
  sequentially consistent, which is the Dekker pattern Linux's futexes
  rely on: either the waiter sees the new index, or the other end sees
  the word and signals.
- The TLA+ model `userspace/libradix/tla/Channel.tla` checks the
  protocol with one-message queues, for ping-pong, bursts that fill the
  queue and both ends sending first: no deadlock, and both ends always
  finish. Leaving out the second look breaks both. `ob test tla` runs
  it.

## Consequences

- A round trip on one CPU takes two context switches, and an end that
  never has to wait makes no system call at all.
- The layout of the shared frame changes: the words sit at the head plus
  8 and at the tail plus 8, and messages start at 128. Both ends come
  from the same `libradix`, so nothing else needs to change.
- Phase 6's services, which build their own queues on shared memory,
  follow the same pattern.
