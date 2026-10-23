<!--
SPDX-FileCopyrightText: 2026 The Dukovo contributors
SPDX-License-Identifier: Apache-2.0 OR MIT
-->

# 0082. Copies under a file's page cache lock never wait for a page

- Status: Accepted
- Date: 2026-10-23

## Context

A file's page cache has one lock over its pages and size (ADR 0055).
Until Phase 9 it was held while a read copied to the caller's memory and
while a write copied from it. A copy that touches a page the program has
not used yet faults, and the fault makes the page while holding its
address space's lock. If the page belongs to a private mapping of a
file, making it reads that file through its cache.

That is two locks in both orders: the cache, then the address space for
a copy; the address space, then a cache for a fault. The lock checker
found it on the first boot that ran uutils' utilities, which read large
files into new memory while other programs fault in their own code. It
is a real deadlock:

- two threads of one process, one reading a file into new memory, the
  other faulting on a mapping of the same file, can each wait for the
  other's lock;
- one thread reading a file into a private mapping of that same file
  that it has not touched yet waits for a lock it already holds;
- a write from a mapping of one file into a second file, while another
  thread does the opposite, waits on the two caches.

Linux has the same problem and solves it the same way in both
directions. `filemap_read` copies with no page lock held, and
`generic_perform_write` copies with page faults turned off under the
folio lock, faults the user pages in with the lock let go, and tries
again.

## Decision

A copy between a file's cache and a program's memory, while the cache's
lock is held, never waits for a page to be made.

- `Out` gains `put_now` and `fault_in`, and `In` gains `get_now` and
  `fault_in`. The `_now` copies run with preemption off. A fault on the
  copy then ends it instead of calling the page handler, and it reports
  that it stopped. `fault_in` makes the pages with no lock held, as
  faults on them would. Kernel buffers keep the default methods, which
  never stop.
- `FileCache::read` and `FileCache::write` copy with `put_now` and
  `get_now`. When a copy stops, they let go of the lock and fault in up
  to 64 KiB of what is left. Then they go on from where they stopped,
  until the copy is done or a page cannot be made.
- A write, or a change of size, also holds a second lock, `writing`, for
  the whole call, including while pages are made. Faults never take it.
  So a write that stopped partway is not split by another write or by a
  cut, and an `O_APPEND` write stays in one piece.
- A write whose copy stops partway keeps the cache honest. A page made
  to be written whole, which was never read from the disk, is dropped.
  On any other page, the bytes past the file's end are zeroed again, and
  the page is marked dirty if the copy may have changed bytes inside the
  file.

The lock order becomes: a file's write lock, then an address space's
regions lock, then a file's page cache.

## Consequences

- The fast path costs one more pair of preemption-count changes on each
  page copied, and an uncontended mutex on each write. Both are well
  inside the `file_read_4k` and `file_write_4k` budgets.
- A read or write into memory not touched yet lets go of the cache's
  lock about once for each 64 KiB. Each fault also makes a page, so the
  extra cost is small.
- A copy that stopped partway may already have written some of the new
  bytes into the file's pages. If the program's buffer then turns out to
  be bad, the write still fails as before, with those bytes kept.
  Linux keeps them too, and counts them.
- The host tests drive the cache with model buffers whose pages are
  missing at random. The posix boot test reads a file into a private
  mapping of itself, and writes it from one. Both of these used to
  deadlock.
- Other copies under locks that a fault never takes, such as pipes,
  terminals and `readlink`, are unchanged.
