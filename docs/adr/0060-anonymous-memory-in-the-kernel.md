<!--
SPDX-FileCopyrightText: 2026 The Dukovo contributors
SPDX-License-Identifier: Apache-2.0 OR MIT
-->

# 0060. Anonymous memory is the kernel's: made when touched, charged to the domain, and shared copy-on-write after a fork

- Status: Accepted
- Date: 2026-10-22

## Context

Until Phase 7 a program's memory was frames: it retyped untyped memory
into frames and mapped them with `map` (ADR 0047). That is enough for
radix-rt, which takes its heap in 64 KiB chunks, but not for a C
library. POSIX's `mmap` hands out zeroed memory at an address the kernel
chooses, `mprotect` changes its access page by page, `munmap` gives back
any part of it, and `fork` must give the child a copy of all of it
without copying it. A program's stack, its heap through `malloc` and the
segments of the program `exec` starts are all memory of this kind. The
kernel must also keep its promise that a resource domain's limit bounds
its programs' memory (ADR 0029).

## Decision

- An address space holds **anonymous regions** besides frames: runs of
  addresses with an access and nothing behind them yet
  (radix-address-space(2)'s `map_anonymous`, `protect`, `discard`). A
  page is made the first time the program touches it, from the frame
  allocator with the owner `Anon`, zeroed or filled from the file the
  region maps (`map_file`), and charged to the domain of the process the
  space belongs to. The fault handler and the kernel's copies to and
  from user memory both resolve faults this way; one that cannot be
  resolved is SIGSEGV for the thread (ADR 0062).
- The page allocator's record of each page counts the spaces that map
  it. A **fork** gives the child's space an entry for every page and
  counts it shared; a private page is mapped read-only in both, and the
  first write copies it for the writer, or makes it writable in place if
  no other space maps it any more. A `MAP_SHARED` region stays writable
  in both. Every page a space maps is charged to its domain once, so a
  fork charges the child's domain for all of the parent's pages at once.
- The kernel chooses where unplaced memory goes: the highest free range
  below the hint, or below `ANONYMOUS_TOP`. `fixed` replaces what is
  there and `fixed_no_replace` refuses to.
- A file's pages are mapped as the process's **own copy**, filled when
  touched; a shared writable mapping of a file is refused (ENODEV) until
  the page cache can map its pages (docs/design/posix.md).

## Consequences

- mlibc's `mmap`, `mprotect`, `munmap` and its allocator work unchanged,
  and `exec` maps a program's segments as regions filled from its file,
  so a program starts as fast whatever its size.
- Copy-on-write makes `fork` cost one page-table walk of the parent's
  memory, not a copy of it; the `posix` boot test forks from programs
  with large heaps.
- Charging the whole of a fork's memory up front can refuse a fork that
  Linux's overcommit would allow; a domain's limit stays a limit.
- Words in shared memory are not futexes across processes yet (a futex
  is named by its address in one space); process-shared mutexes wait
  for a later phase.
