<!--
SPDX-FileCopyrightText: 2026 The Dukovo contributors
SPDX-License-Identifier: Apache-2.0 OR MIT
-->

# 0110. A shared mapping keeps its pages in one place, which also names the futex words in it

- Status: Accepted
- Date: 2026-10-25

## Context

ADR 0060 made anonymous memory when it is first touched, and kept a
`MAP_SHARED` region's pages writable in both processes after a fork.
The pages lived only in each space's page tables, though. A fork gave
the child the pages the parent had made, but a page that neither had
touched yet was made separately by whichever side touched it, so each
process got a page of its own and never saw the other's writes. A
program that maps shared memory, forks its workers and lets them fill
it, as PostgreSQL's shared buffers are filled, could not work.

Futex words were named by their address space and address. Two
processes waiting and waking on the same word of shared memory used two
names, so a wake in one never reached a waiter in the other. mlibc
refused process-shared semaphores with ENOSYS, and process-shared
mutexes and condition variables, which it accepted, slept through their
wake-ups. The roadmap lists cross-process futexes as a risk for
PostgreSQL, whose semaphores live in shared memory.

## Decision

- A shared anonymous mapping gets its pages' own object,
  `SharedPages`, when it is made. Every region that maps it holds it:
  the parent's, each forked child's, and both parts of a region an
  `munmap` or `mprotect` splits. A fault on a page with no entry takes
  the page from it, made and zeroed if no space has touched it yet, so
  every space finds the same page. Page `n` is the one at the region's
  offset plus `n`, which splits keep.
- It keeps its pages' frames in chunks of 512, each made when the first
  of its pages is touched and kept in order, so a large mapping touched
  in few places costs little.
- It counts as one more sharer of each page it made. A page stays while
  any region holds the mapping, whether or not a space maps the page at
  the moment, and goes back to the allocator when the last region goes.
- Each space that maps a page is charged for it, as ADR 0060 has it for
  every page. The mapping also charges its pages to the domain of the
  space that made it, until it goes, so a page no space maps any more
  is still counted somewhere and cannot escape a domain's limit.
- A futex word in memory only one space maps is named, as before, by
  the space and its address. A word in a shared mapping, a frame or a
  ring, which other spaces may map at other addresses, is named by that
  object and the word's place in it, so every space that maps it names
  it the same. A waiter holds the object while it waits, so a new
  object made at the same address in kernel memory cannot take its
  name, even if every space unmaps the memory meanwhile.
- A space that has never mapped such memory (`may_share`) names its
  words without looking at its regions: an ordinary program's futexes
  cost one more atomic load. Otherwise a wait or wake finds the word's
  region under the regions lock, as a page fault does.
- mlibc's `sem_init` accepts shared semaphores (patch 0046). Its
  mutexes, condition variables, barriers and read-write locks already
  accepted `PTHREAD_PROCESS_SHARED`, and sleep on the same futexes.

Linux decides the other way round: a futex is shared unless the C
library passes `FUTEX_PRIVATE_FLAG`, and a shared one is named by the
page's file or memory object. Radix decides from the memory itself, so
the Radix C library passes no flag, and a word in shared memory is
shared whatever the caller says. A program that maps shared memory pays
the region lookup for its private words too; the benchmarks below show
what it costs.

Two alternatives were set aside:

- Naming a shared word by its physical frame. A page not yet touched
  has none, so a wait would first have to make it, and swap (Phase 10)
  will give a page a new frame when it comes back; the mapping and the
  word's place in it stay the same.
- Making all of a shared mapping's pages when it is made. A large
  mapping used in few places would cost all its memory at once, and
  Linux makes them as they are touched too.

## Consequences

- A page of a shared mapping first touched after a fork is the same
  page in every process, and process-shared semaphores, mutexes and
  condition variables wake across processes. The `posix` boot test's
  `fork` program checks the pages, semaphores and mutexes.
- `ob bench` has two more POSIX rows, a turn passed there and back
  between two processes through shared semaphores, on one CPU and on
  two (`posix_futex_shared_same_cpu`, `posix_futex_shared_cross_cpu`),
  with an instruction budget for the first.
- A page of a shared mapping counts once more against the domain of
  the space that made the mapping than ADR 0060's rule alone would
  count it. Like the rule itself, this counts too much rather than too
  little.
- Shared writable mappings of files still fail with ENODEV: they need
  the page cache to map its own pages. System V shared memory is not
  there yet either.
- Swap (Phase 10) gives a page of a shared mapping one place to point
  to: the mapping's chunk keeps where the page went, and every space's
  entry for it can be dropped.
