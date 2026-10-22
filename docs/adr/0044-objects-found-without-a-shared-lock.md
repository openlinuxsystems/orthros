<!--
SPDX-FileCopyrightText: 2026 The Dukovo contributors
SPDX-License-Identifier: Apache-2.0 OR MIT
-->

# 0044. Objects are found through a per-CPU cache of weak references, and endpoint calls on both CPUs at once are held to Linux

- Status: Accepted
- Date: 2026-10-21

## Context

Every IPC system call finds the object its capability names: the
endpoint of a `cap_call` or `reply_recv`, the notification of a
`notify` or `wait`. The capability itself comes from a per-CPU cache
that takes no lock while the store has not changed (ADR 0042), but the
object came from the object registry, a table behind one global mutex.
ADR 0042 noted the cost on one CPU (about 90 instructions per call) and
left an RCU registry for when it mattered. On several CPUs it matters
more than the instructions: every IPC call on every CPU took and let go
of the same lock, so programs calling different endpoints on different
CPUs bounced one cache line between them, which Linux, whose file
table lookups take no lock, does not do. The benchmarks did not show
it: their only parallel rows were system calls without objects.

## Decision

- `user::objects::get` keeps, on each CPU, the objects it found, with the
  store's version when it found them, in a 16-entry table indexed by
  object. While the version is the same the registry has not changed (it
  changes only with the store's lock held, which moves the version
  before it goes), so a hit takes no lock. `objects::insert` takes the
  store as an argument, to show that the lock is held.
- The cache keeps weak references (`Weak`), which keep an object's memory
  but not the object: a cache never keeps an object alive, nor delays
  closing it, after the registry lets go. A hit takes a strong reference
  again, which fails once the object is gone, and the lookup falls back
  to the registry. So a hit writes nothing shared but the object's own
  reference count, and no unsafe code is needed: the version says the
  object was still registered, and the weak reference says its memory is
  still there.
- `ob bench` gains `endpoint_call_parallel`: a pair of programs on each
  CPU at once, each pair calling its own echo server on its own CPU,
  next to `pipe_roundtrip_parallel` from `linux-bench`, a pair of threads
  on each CPU passing a message through pipes. The gate holds it to 1.05
  times Linux like every other row.

## Consequences

- IPC on different CPUs shares no lock to reach its objects. Under QEMU's
  instruction counting (ADR 0045) a same-CPU endpoint round trip is
  about as long as before; the gain is the shared line, which only the
  parallel row under KVM measures.
- A CPU's cache can hold the memory of up to 16 objects that are gone
  until it looks up another object in the same entry; that memory is at
  most a few kilobytes per CPU.
- Any change to the capability store still empties every CPU's caches of
  capabilities and objects, and changes still take the store's one lock.
  A per-space store with RCU over the slots remains the fix if Phase 6's
  processes, which change their capabilities more often, show it matters
  (`docs/design/ipc.md`, Known limits).
