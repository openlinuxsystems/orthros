<!--
SPDX-FileCopyrightText: 2026 The Dukovo contributors
SPDX-License-Identifier: Apache-2.0 OR MIT
-->

# 0077. One per-CPU cache finds a capability and its object, and empties only when something it holds goes

- Status: Accepted
- Date: 2026-10-22

## Context

Phase 8's pull request failed the performance gate (ADR 0039) on CI's
newer AMD runners (EPYC 9V45, Zen 5), where Linux in the VM is about
30% faster than on the Zen 4 runners that Phase 7 passed on: opening
and closing a file took 1.08 times Linux's time, and reading 4 KiB
1.07. Instruction traces of one operation each (as for ADR 0040)
showed:

- Each CPU kept two caches, of capabilities by slot (ADR 0042) and of
  objects by name (ADR 0044), both checked against the store's version
  number, which moved on every change to the store. Opening a file
  puts a capability in a slot and closing it deletes one, so every
  `openat` and `close` emptied every CPU's caches, and the next lookups
  took the store's and the registry's locks again.
- A file read looked in a cache three times: the capability for the
  system call, again for the method, and the object by its name. Each
  look held preemption off and set and cleared the cache's busy flag,
  and the 56-byte capability was copied five times on the way.
- Each heap free checked its pointer with two divisions, by the slab's
  object size, and a binary search of the memory regions for its
  frame.

## Decision

- The version moves only when something a cache may hold is taken
  away. A lookup that fills a cache marks the capability's slot
  (`Store::get_to_cache`) and the object's registry entry; deleting,
  moving or revoking a marked capability, or destroying its space,
  makes the store stale (`Store::take_stale`), as does taking a marked
  object out of the registry (`caps::mark_stale`), and `caps::with`
  moves the version before the store's lock goes. A capability put in
  an empty slot, or one nobody looked up deleted, leaves every cache as
  it is. The argument that a hit is still right is the old one: a
  capability stays in its slot, unchanged, until something empties the
  slot, and that now moves the version as every change did before.
  Unit tests compare a cache kept fresh this way with the store after
  random sequences of every operation.
- One cache, in `user::objects`, keeps each capability with a weak
  reference to its object (`objects::find`). A system call looks once
  and gets both; the file and directory methods and endpoint calls
  borrow what it found. `caps::get` and `objects::get` look under their
  locks, for the few lookups too rare to be worth a place in the cache.
- The lookup writes what it finds into a place the caller gives it,
  rather than returning it: a capability and an object are 88 bytes,
  and the kernel, built without SSE, copies anything over 64 bytes with
  a string move, whose start-up costs more than the whole lookup. A
  per-CPU value can be held through a guard (`PerCpu::own`) as well as
  a closure, which the compiler would not inline into the system call
  handler.
- The slab layer finds an object's index by multiplying by the class's
  reciprocal, 2^32 / size + 1, which is exact for every offset below
  2^15, beyond the largest slab (an exhaustive unit test checks every
  offset of every class). The frame table remembers the region its last
  lookup found and looks there first.

## Consequences

- Opening and closing files no longer empties the caches.
  Instructions per operation under QEMU's counting (ADR 0045), on one
  machine, before this work and after:

  | Operation | Before | After |
  | --- | ---: | ---: |
  | File opened and closed | 4,150 | 3,693 |
  | 4 KiB read | 5,053 | 4,896 |
  | 4 KiB written | 5,217 | 5,061 |
  | File status | 1,618 | 1,430 |
  | Capability method | 444 | 439 |
  | Endpoint call and reply | 5,561 | 5,303 |
  | Notification ping-pong | 5,676 | 5,110 |
  | Fresh zeroed page | 8,966 | 8,769 |

  A 4 KiB read's 4,096-byte copy counts as 4,097 instructions here but
  is one string move: the rest of a read went from about 960
  instructions to 800.
- Each CPU's cache is 16 entries of about 96 bytes, smaller than the
  two caches it replaces. ADR 0042's capability cache and ADR 0044's
  object cache are this one now; ADR 0044's weak references and its
  parallel benchmark row stay as they were.
- A capability found by a system call now marks its slot, so deleting
  it later moves the version and empties every CPU's cache, as every
  deletion did before. Programs that open, use and close files in a
  loop still pay that on each close; a per-space version, or RCU over
  the slots, is the step after this one if they show it matters.
