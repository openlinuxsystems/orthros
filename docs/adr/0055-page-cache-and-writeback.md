<!--
SPDX-FileCopyrightText: 2026 The Dukovo contributors
SPDX-License-Identifier: Apache-2.0 OR MIT
-->

# 0055. File data lives in per-file page caches, metadata in a block cache per device, written back every five seconds

- Status: Accepted
- Date: 2026-10-22

## Context

A disk is thousands of times slower than memory, so every operating
system keeps file data in memory and writes it back later. How it does
so decides how many times data is copied, how much a power cut loses,
and what happens when memory runs short. Linux keeps each file's pages
in a radix tree (the XArray), its metadata in a buffer cache of device
blocks, and writes dirty data back from flusher threads after about five
to thirty seconds.

## Decision

- Each file has a page cache (`radix_vfs::cache::FileCache`): its size
  and the pages of its data that are in memory, indexed by page number in
  a radix tree of 64-slot nodes (`radix_block::pagemap`, after the
  XArray). A read copies from the pages into the caller's memory,
  fetching missing pages first; a write copies from the caller into the
  pages and marks them dirty. Disks move pages by DMA, so data is copied
  once between a program and the disk.
- A filesystem's metadata (FAT's tables and directories) goes through a
  cache of its device's sectors (`radix_block::cache::BlockCache`), which
  tracks which sectors it read and changed, writes only the changed ones,
  and can forget sectors exactly (`discard`).
- tmpfs is a page cache with nothing below it.
- Cache pages come from the frame allocator, owned by `PageCache`, so
  `/proc/meminfo` shows them.
- A writeback thread writes every filesystem with changes to its disk
  every five seconds, so a power cut loses at most that much. A sync
  (radix-directory(2), radix-file(2)) writes at once.
- Clean pages can always be dropped. The writeback thread drops some
  when free memory falls below 1/64 of all memory, and the out-of-memory
  policy asks it to before it stops a resource domain (ADR 0029). Pages
  are never dropped inside an allocation, since that would take the
  caches' locks there.
- One lock guards a file's pages and size, held while the disk works:
  one request at a time per file. Since ADR 0082, copies to and from a
  program's memory under it never wait for a page, and writes hold a
  second lock for the whole call.

## Consequences

- Repeated reads cost a copy from memory, and writes return as soon as
  the data is in memory.
- Programs that need their data on the disk sync, as on any other
  system. `fsync`'s meaning in Phase 8's POSIX layer maps onto that.
- Holding a file's lock across its I/O keeps the code simple but
  serialises readers of one file behind each other. A file with many
  concurrent readers (a database, a shared library) wants per-page
  locking, which the post-Phase-9 audit looks at with measurements.
