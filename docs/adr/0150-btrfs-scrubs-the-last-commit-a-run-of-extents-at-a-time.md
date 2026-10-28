<!--
SPDX-FileCopyrightText: 2026 The Dukovo contributors
SPDX-License-Identifier: Apache-2.0 OR MIT
-->

# 0150. Btrfs scrubs the last commit, a run of extents at a time

- Status: Accepted
- Date: 2026-10-28

## Context

Roadmap 10c lists scrub, and the 1.0 checklist's soak test ends with no
errors from a Btrfs scrub. A read checks the copy it reads and tries
another when it is bad, but blocks nothing reads, and the second copy of
everything (DUP metadata on one disk, RAID1 in 10e), go unchecked until
they are needed. Linux's `btrfs scrub` reads every copy of every block
in use, checks it, counts what it finds in `struct
btrfs_scrub_progress`, and writes a good copy over a bad one.

## Decision

- **What is read.** Every extent item of the extent tree as the last
  commit has it: each copy of each tree block, checked as a read checks
  one (its checksum, then its address, filesystem, level, generation and
  items, with the extent item's generation), and each copy of each data
  sector against the checksum tree of the same commit, from the disk
  past every cache. Then each copy of the superblock that fits on the
  device: its checksum, magic, address, filesystem and generation.
- **While the filesystem is in use.** The last commit's blocks are not
  taken again before the next commit, so the walk is of that commit's
  trees. On a writable mount it holds the transaction for 16 MiB of
  extents at a time and lets it go between, so no commit falls between
  finding an extent and reading it, and a change waits no longer than a
  run. Extents written after the scrub has passed their place are left
  to the next scrub, as on Linux.
- **Mending.** A bad copy of a tree block or data sector with a good
  copy is written over with it, unless the scrub only reads (Linux's
  `-r`); the device is flushed at the end. One with no good copy is
  counted as uncorrectable.
- **What is counted.** Linux's counters, in its order, a sector at a
  time as Linux counts them: a bad copy of a 16 KiB tree block is four
  checksum (or verify) errors, and four corrected or uncorrectable ones.
  A data sector with no checksum (`nodatasum`) is counted in `no_csum`.
- **The interface.** The directory interface's `scrub` scrubs the
  directory's filesystem and writes a `ScrubProgress`; it needs the read
  right, and the write right to mend. Other filesystems fail with
  `EOPNOTSUPP`. `scrub(8)` is `btrfs scrub start -B` with `-r` and `-q`,
  its summary, and its exit status 3 for uncorrectable errors.

## Alternatives

- **Scrub by device extent, as Linux does.** Linux walks each device's
  extents and scrubs a 64 KiB stripe at a time, with the extent tree for
  what is in use. On one device a walk by extent reads the same blocks;
  device order matters for RAID5/6 and for resuming, neither of which
  Radix has.
- **A scrub in the background, with `status`, `cancel` and `resume`.**
  Linux runs the scrub in a kernel thread per device and keeps progress
  in `/var/lib/btrfs`. A call that returns when the scrub is done is what
  `start -B` needs; the rest comes with a service that keeps state.
- **Hold the transaction for the whole scrub.** Simpler, but writes
  would wait for the whole disk to be read.

## Consequences

- `scrub` in radix-btrfs's host tests damages a copy of a tree block and
  of a data sector under a mount with DUP data, checks that a scrub that
  only reads counts them and leaves the disk, that one that mends makes
  the disk as it was, and that what no copy mends (single data, both
  copies of a block) and a bad superblock copy are counted.
- The `root` boot test scrubs Dukovo's root at the second boot, through
  `/users` mending and through the read-only `/system` only reading, and
  finds no errors.
- The device is read at the speed of runs of 32 sectors and single tree
  blocks, without read-ahead.
