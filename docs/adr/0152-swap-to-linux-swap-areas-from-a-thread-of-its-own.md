<!--
SPDX-FileCopyrightText: 2026 The Dukovo contributors
SPDX-License-Identifier: Apache-2.0 OR MIT
-->

# 0152. Swap to Linux's swap areas, from a thread of its own

- Status: Accepted
- Date: 2026-10-28

## Context

Roadmap 10d.1 is swap. Until now a machine whose programs wanted more
memory than it has could only drop clean pages of the page cache and
then stop a resource domain (spec 4.1); free(1) and vmstat(8) showed no
swap. A disk Linux set up has a swap partition `mkswap` made, and a
machine that starts both should share it, as it shares the root's
Btrfs.

## Decision

- **Linux's swap areas.** An area is a disk or partition `mkswap` made:
  version 1 of Linux's `union swap_header`, for pages of 4 KiB, with its
  bad pages left out (`radix_block::swap`). Page 0 is the header; every
  other page is a slot. Swap files are not taken yet.
- **Turned on and off by the device.** radix-block(2) gains `SWAP_ON`,
  which reads Linux's swapon(2) flags (a priority with `SWAP_PREFER`, or
  the next default one, -2, -3 and on), and `SWAP_OFF`; both need the
  write right on the device. They are Dukovo's own requests, as Linux has
  system calls for them. While an area is on, the device's raw writes
  and mounts fail with `in_use`, as what programs swapped out is on it.
  At most eight areas are on; they fill highest priority first.
- **Slots.** Each area has a byte a slot: how many page table entries
  hold it (a fork shares them), or that it is the header or a bad page.
  A slot is numbered `area << 32 | page`, which a page table entry that
  is not present keeps in bits 12 to 51, with bit 10 saying it is a
  slot. Slots are taken in runs of up to 16 that follow one another, so
  that one write takes a run, with nothing allocated for it.
- **What is swapped out.** Pages of private anonymous memory that one
  address space alone maps, that is lent to no device (ADR 0111), whose
  region allows access, in spaces a thread has run in: until then exec
  writes a new program's pages by their frames. Shared memory, frames
  and rings stay in memory; the page cache is dropped, not swapped, as
  before.
- **Which pages.** A second chance, from the CPU's accessed bit: a look
  at a page clears its bit, and a page whose bit is still clear at the
  next look goes. The thread goes round the address spaces, and through
  each from where it stopped. A space whose regions are locked is passed
  over, so the thread never waits for a program that is waiting for
  memory.
- **When.** The `swap` thread wakes every second while an area is on:
  while free memory is under 1/32 of RAM, it swaps out until it is over
  1/16. The out-of-memory policy asks it too, after the page cache and
  before it stops a domain (step 0), and waits up to 200 ms for pages to
  go: up to 1/16 of RAM free, and at least 256 pages. Swapping out writes
  to disks, so it is never done inside an allocation.
- **A fault lets go first.** A fault holds its space's regions locked,
  which the thread would pass over, so with an area on a fault's frame is
  taken without reclaiming; if none is free above the reserve, the fault
  lets go of the lock, asks the thread, and tries again, up to 16 times,
  the last waiting as any allocation does. Linux's faults do as much
  with `VM_FAULT_RETRY`. So a program that fills memory by itself has
  its own pages swapped out, instead of its domain stopped. A fork
  copies its space's tables with the regions locked too, so with an area
  on it first has the thread make room for the child's tables: a page
  for each 512 of the space's pages, and 64 more. Without it, a fork
  that found memory short had nothing swapped out and failed with
  `no_memory` (seen in CI on 2026-10-28). It makes room for 1/16 of RAM
  more, the thread's mark: until the child execs or one of them writes
  a page, the pages they share cannot be swapped out, and exec fills the
  new program's space before it lets go of the old one. Without that, a
  program whose pages filled memory forked to run another, and the
  child's exec found nothing to swap out and was stopped (seen in CI on
  2026-10-29).
- **Coming back.** The next fault at a swapped-out page reads it back,
  one page at a time, and counts a major fault. Freeing a page frees its
  slot, and swapoff reads back every page of the area before it lets it
  go, failing with `no_memory` if they do not fit: at once if more is out
  on it than free memory, the page cache and the other areas' free slots
  have room for.
- **Charges.** A page swapped out stays charged to its domain: a
  domain's limit counts its memory in RAM and in swap, as cgroup v1's
  `memsw` does, so swap never lets a domain past its limit.
- **What shows it.** `/processes/meminfo` gains `swap_total` and
  `swap_free`, `/processes/stat` gains `swap_ins` and `swap_outs`, and
  `/processes/swaps` lists the areas as Linux's `/proc/swaps` does.
  systools gains mkswap(8), swapon(8) and swapoff(8), with util-linux's
  options and messages, and free(1) and vmstat(8) show swap.

## Alternatives

- **Linux's way, from inside allocations.** Linux reclaims in the
  allocating thread (direct reclaim) as well as in `kswapd`. Writing to
  a disk from inside an allocation, with whatever locks the caller holds,
  is how kernels deadlock; one thread, and the out-of-memory policy's
  wait, keep it out.
- **An LRU of every anonymous page.** Linux keeps active and inactive
  lists of pages. The page tables already hold what the second chance
  needs, without a list to keep up on every fault.
- **Swap files.** They need the file's blocks found once and kept where
  they are, which Btrfs only allows for files without copy-on-write or
  compression; they come later.
- **Compressing pages in memory (zram, zswap).** Worth it on machines
  without a fast disk; it can sit in front of an area later.

## Consequences

- The `swap` boot test runs Radix in 256 MiB with a 1 GiB area that the
  host's `mkswap` made: it turns it on, fills 384 MiB, more memory than
  the machine has, reads it all back and checks it, forks with pages
  swapped out, fails to turn the area off while more is out on it than
  memory has room for, turns it off with pages on it, and checks that the
  device refuses raw writes while it is on.
- radix-block's host tests read headers as Linux reads them, and those
  `mkswap` makes.
- A space no thread has run in is never swapped out; one whose regions
  stay locked is not either, until they are let go.
- Pages come back one at a time, with no read-ahead of their neighbours.
- No discard: Linux's discard flags fail with `not_supported`.
- Hibernation, which writes all of memory to an area, is not done.
- Spark turns no area on at boot yet: the guided layout's swap
  partition comes with the installer (Phase 14), which is when Spark
  will turn on the swap partitions of the root's disk by their GPT type,
  as systemd's `gpt-auto` does.
