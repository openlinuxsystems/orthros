<!--
SPDX-FileCopyrightText: 2026 The Dukovo contributors
SPDX-License-Identifier: Apache-2.0 OR MIT
-->

# 0118. Memory written in order gets frames in order, and virtio merges buffers that follow each other

- Status: Accepted
- Date: 2026-10-26

## Context

After ADRs 0116 and 0117, one CI runner type (AMD EPYC 9V45, KVM inside
an Azure virtual machine) still had the 1 MiB write rows of `ob bench`
over Linux: 1.02 times for virtio and 1.09 for NVMe. The thread's own CPU
time per transfer (`CLOCK_THREAD_CPUTIME_ID`, which counts the exits to
the hypervisor it makes) was a third of Linux's; the rest of each
transfer was the wait for the virtual disk, and that wait grew with the
transfer's size faster than Linux's.

QEMU's traces under KVM showed where: QEMU's worker thread took 55.6 µs
to write a 512 KiB NVMe command's data from Radix's buffer to its file,
and 41.9 µs from Linux's, on the same disk image; reads, where the worker
writes into the buffer, took 41.0 and 45.0 µs. The difference between the
two systems' requests was where their buffers lay. A program's buffer is
faulted in a page at a time, and Radix's frame allocator gives each fault
the frame its CPU freed last (the per-CPU cache, `docs/design/memory.md`),
so after a few programs have come and gone a 1 MiB buffer lies in frames
scattered over megabytes. Linux's lay mostly in order: its 1 MiB virtio
write was one request, so its pages had merged into at most 254 segments.
A host copying from scattered guest pages under nested paging pays more
for each.

## Decision

- The frame allocator gives each CPU a *run*: a block of 32 contiguous
  frames (128 KiB), taken from its zone in one go and handed out lowest
  first, for allocations of memory filled in order
  (`Frames::alloc_sequential`). Frames of a run not yet handed out are
  cached frames, counted as free, and go back to the zone when the caches
  are emptied. With no free block that large, any frame does.
- An anonymous page fault takes its frame from the run when it makes a
  zeroed page of a region of at least a run's size, and the page before
  it in the region is already there: a program writing a fresh buffer
  from its start gets it in order after its first page. Any other fault
  takes the frame freed last, as before, still warm in the caches; the
  small regions a program's start fills keep reusing what the last
  program freed, which a `fork` and `exec` would otherwise pay for in
  frames given back to the zone (about 5% more instructions when every
  region took part, 1.6% this way).
- A virtio request merges data buffers that follow each other in memory
  into one descriptor, up to the device's `size_max`, as Linux merges a
  request's pages into segments; `seg_max` bounds the descriptors after
  merging.

## Alternatives

- **Map 16 pages at a fault (fault-around for anonymous memory, as
  Linux's multi-size transparent huge pages do).** Fewer faults too, but
  a program is charged for pages it never touched, and Linux leaves it
  off by default.
- **A smaller per-CPU cache, so that more frames come fresh from the
  zone.** Every allocation would take the zone's lock more often, and
  freed frames would still come first.
- **Take exactly the frame after the page before's.** A look in the zone
  at every fault, and a buffer's start is still wherever the cache's head
  was.
- **Reservations (FreeBSD's superpage reservations).** A fault takes its
  frame from a 2 MiB block set aside for its part of the region, which is
  also the way to huge pages; much more bookkeeping, and blocks to break
  up under memory pressure. Worth it with huge pages, later.

## Consequences

- A buffer written in order lies in runs of 128 KiB, mostly one after
  another as the zone splits larger blocks in order. A virtio request for
  it is a handful of descriptors rather than one for each page, and a
  hypervisor copies it in long stretches.
- Frames a program frees still go to the cache, but faults in order no
  longer take them, so a cache passes its limit and gives a batch back to
  its zone more often: a buddy merge for each frame it gives back.
- NVMe takes one PRP entry per page whatever the frames are; scatter
  gather lists could take whole runs later.
- A CPU may hold up to 32 frames of its run beyond its cache, until the
  caches are emptied.
