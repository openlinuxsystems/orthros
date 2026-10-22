<!--
SPDX-FileCopyrightText: 2026 The Dukovo contributors
SPDX-License-Identifier: Apache-2.0 OR MIT
-->

# 0059. A path with no `..` is walked without a chain, each mount filters its mount points, and a file method looks its capability up once

- Status: Accepted
- Date: 2026-10-22

## Context

Phase 7 added four file rows to the performance gate (ADR 0039), next to
Linux's `openat`, `fstatat`, `pread` and `pwrite` on a tmpfs. The first
run on the CI runners put three of them behind Linux in the same virtual
machine: opening and closing a file by name at 1.41 times Linux, its
status by name at 1.10 and a 4 KiB read of a page in memory at 1.17.
Tracing one operation of each, instruction by instruction (the method of
ADR 0040), showed where the time went:

- Every path walk allocated a vector for its chain of places, the one
  `..` steps back along (ADR 0054), freed it at the end, and took and
  dropped a reference to each place on it: a heap allocation and a free
  were 500 instructions of an open's 4,600.
- Every name looked up took the mount table's lock and searched the whole
  table for a mount on the node it found, since the table holds a dozen
  mounts once Spark has started; nothing is mounted on nearly every node.
- `open` asked the node for its whole status twice, to learn whether it
  was a directory, and a tmpfs file's status took its page cache's lock
  three times.
- The system call looked the capability up to find its kind, and the
  file or directory method looked it up again.
- An `open` that changes nothing still looked at the destination slot
  first, and after any change to the capability store (every open and
  close is one) that look misses the CPU's cache and takes the store's
  lock.
- Making the file's object took the object registry's lock twice, a
  read looked its page up twice, and splitting a path on `/` went
  through the standard library's general string searcher.

## Decision

- A path is walked without a chain until its first `..`: the walk holds
  only where it is, borrowing the starting place until it leaves it, so
  it allocates nothing and takes one reference per name. At a `..` it
  starts again from the start with the chain, which still decides where
  `..` may go.
- Each mount keeps a 128-bit filter of the nodes something is mounted on,
  one bit each, chosen by the node's address with Fibonacci hashing, as a
  Bloom filter with one hash; Linux marks mount points in its directory
  cache (`DCACHE_MOUNTED`) for the same reason. A walk past a node whose
  bit is clear does not look at the table. `mount` sets the bit, under
  the table's lock, before the mount is in the table; `unmount`
  recomputes the filter of the mount below, word by word, after the
  mount has left it. A walk that misses a bit has missed the mount, as
  if it had run before the mount; no bit a mount still needs is ever
  clear.
- `Node::kind` says what a node is without the rest of its status, and
  a tmpfs or FAT file's status reads its page cache under one lock
  (`FileCache::status`).
- The system call hands the capability it looked up to the directory
  and file methods, which check its kind and rights themselves.
- `open` looks at the destination slot first only when it creates or
  cuts a file; otherwise putting the capability there finds an occupied
  slot, and the object it made is dropped once the store's lock has gone
  (`process::install_with_rights`). An occupied slot is still
  `occupied`; only which error comes first, when a call has two, can
  differ.
- A new object is made in the store and recorded in the registry under
  one hold of the registry's lock (`objects::create_with_rights`); a
  read looks a page in memory up once; `path::components` finds each
  `/` itself.

## Consequences

- An open and close of a file in a tmpfs takes 4,200 instructions, from
  6,049; its status by name 1,600, from 2,872; a 4 KiB read 880 besides
  the copy, from 1,160.
- Paths with `..` cost what they did. A mount on a filesystem whose
  filter bit for a node is already taken by another mount point sends
  walks past that node to the table; the root, with a dozen mounts, has
  about one bit in ten set.
- The proof that a path stays beneath its directory (docs/design/
  storage.md) covers both walks: the one without a chain only ever looks
  names up.
