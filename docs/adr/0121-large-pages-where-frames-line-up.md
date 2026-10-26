<!--
SPDX-FileCopyrightText: 2026 The Dukovo contributors
SPDX-License-Identifier: Apache-2.0 OR MIT
-->

# 0121. A frame's pages get 2 MiB pages where they line up

- Status: Accepted
- Date: 2026-10-26

## Context

The `ob bench` row for memory copies tied Linux: both copy 32 MiB with
the same `rep movsb` over 4 KiB pages, at the speed of the memory.
Ties fail a gate of 1.00 at random, and they did: 1.00 on an Intel Xeon
8573C runner, 1.01 on two AMD EPYC 7763 runners, 0.92 to 0.98 on others.
Radix's own time was the same as before in each.

Address translation is part of the time. Two 32 MiB buffers are 16,384
pages, eight times what the second-level TLB of those CPUs holds, so
each pass misses the TLB at every page of both buffers. Under KVM in a
cloud virtual machine each miss walks the guest's tables and, for each
of their levels, the host's. Linux outside any virtual machine copies at
49.0 µs a MiB on the same runner where it takes 49.9 µs in one.

Every user page of Radix was a 4 KiB one. A frame object's pages follow
one another in physical memory, from a boundary of its own size:
retyping carves 2^order pages at a multiple of 2^order.

## Decision

A frame mapped into a space goes in as one run (`UserMode::map_pages`,
`AddressSpace::map_run`). Each 2 MiB of it that starts at a 2 MiB
boundary in both its virtual and its physical address, where no page
table is yet, gets one entry of the page directory: a large page. The
rest gets 4 KiB pages, as before. Anonymous memory and rings keep 4 KiB
pages.

A large page changes only as a whole:

- An unmap that cuts through one splits it first (`split_pages`, from
  the regions' `cut`): it becomes a page table of the same 512 pages.
  As Linux does, the large page is unmapped and shot down before the
  table takes its place, since a CPU that caches one page as a large
  page and a 4 KiB one at once may take a machine check (Intel's erratum
  on page-size changes; AMD's erratum 383). A program touching it in
  between faults. Its fault waits for the regions' lock, which the split
  holds. It then finds the page mapped and tries again; a frame's page
  that faults while it is mapped as asked now counts as resolved.
- Walks of a range (`edit`, `copy_into`) ask about a large page once,
  as its first page, and do the same to all of it. A fork's copy gets
  the same large page. Unmapping clears the entry and hands back each of
  its 512 frames after the flush.
- Lookups (`query`, `entry`, `frames`) give the 4 KiB page within it.

Unmapping a frame's or ring's region is now one walk of the page tables,
which flushes the TLB once at the end and once for every 64 pages it
clears, where it was one shootdown for each page.

## Alternatives

- **Transparent huge pages for anonymous memory, as Linux has.** More
  programs would gain, but it needs 2 MiB of zeroed memory at a fault,
  splitting at copy on write, `mprotect` and partial `munmap`, folding
  4 KiB pages together later, and reclaim that breaks them up again
  (ADR 0118 left it for later). The page-table walks now know large
  pages, which is a first step towards it.
- **1 GiB pages.** Frames that large are rare, and the walks would need
  them at a third level.
- **Non-temporal or wider copies.** Both systems must run the same
  instructions for the row to compare them.
- **Accepting the tie.** The gate stays at 1.00 and the row stays.

## Consequences

- A copy over a frame needs one TLB entry for 512 pages. For the 4 MiB
  frames of the copy benchmark, the 32 MiB buffers take 16 entries each.
- A page table left empty in a 2 MiB stretch, after an unmap and before
  the space's tables are freed, keeps 4 KiB pages there.
- An unmap that cuts through a large page needs a page table, so it can
  fail for want of memory, as splitting a region already could.
- In a large page's entry, bit 12 is the PAT bit and bit 7 is the size.
  All user memory is write-back, with the PAT bits clear. A cache mode
  for user mappings would have to place them.
- The `tlb` self-test maps, reads on every CPU, copies, splits, changes
  and unmaps a large page. The `user` boot test's probe unmaps pages
  from the middle of a large frame and across two of its large pages.
