<!--
SPDX-FileCopyrightText: 2026 The Dukovo contributors
SPDX-License-Identifier: Apache-2.0 OR MIT
-->

# 0153. Btrfs on several disks mounts with one missing, and warns

- Status: Accepted
- Date: 2026-10-28

## Context

Roadmap 10e.1 is Btrfs across several disks: RAID1 and RAID10 for
servers, with mounts that go on when a disk is lost, and rebuilds. Until
now Radix refused any Btrfs whose superblock counts more than one disk,
with `NotSupported` (ADR 0125). Linux's Btrfs keeps several copies of each
chunk on different disks (RAID1, RAID1C3, RAID1C4), stripes it (RAID0),
or both (RAID10). Its RAID5 and RAID6 still lose data on a power cut
mid-write, and Dukovo never offers them (roadmap, risks).

Linux will not mount a Btrfs with a disk missing unless the mount says
`degraded`. A server whose disk died then stops at boot, at the very
moment its copies were meant to keep it going. Chase chose on
2026-10-28 that Dukovo mounts it anyway, and says so loudly.

## Decision

- **Disks found by their filesystem's ID.** A mount names one disk. The
  kernel offers every other disk and partition it found, and the volume
  keeps those whose superblock has the same `fsid`, matched to the chunk
  tree's device items by device ID and UUID. When two disks have one ID,
  as a copied disk does, the newer one is kept. A filesystem may have up
  to 16 disks; more is `NotSupported`.
- **Every profile but the parity ones.** Radix reads and writes single,
  DUP, RAID0, RAID1, RAID1C3, RAID1C4 and RAID10. RAID5 and RAID6 stay
  `NotSupported`.
- **Chunks spread as Linux spreads them.** A new chunk takes the disks
  with the most unallocated room first, in the numbers each profile
  needs, sized as Linux's `btrfs_alloc_chunk` sizes them. Free room is
  counted as `btrfs_calc_avail_data_space` counts it.
  - When too few disks are left for the profile (a two-disk RAID1 with
    one missing), no new chunk is made. Writes fail with `NoSpace` once
    the chunks there are fill up. Linux makes single chunks then, and
    their data has no copy.
- **Mounted with a disk missing, without being asked.** If a disk is
  missing and the others still hold a copy of every byte of every chunk,
  the filesystem mounts, to read and write. There is no `degraded`
  option.
  - The kernel log warns at the mount, naming the filesystem's disk and
    how many of its disks are missing. The `btrfs` admin command and the
    web console show it as well (10e.2).
  - If some chunk has no copy left, the mount fails with `NoDevice`, as
    Linux's does.
- **A disk that fails under a mount is left out.** A write error on one
  disk of several drops that disk from then on, and the write is done if
  the others still hold every chunk. Otherwise it fails with `Io`. On a
  filesystem of one disk, the error is returned as before.
  - The kernel log warns, as at a mount with a disk missing. The
    writeback thread looks after each of its rounds, every five seconds,
    so the warning comes within seconds and is written under no lock.
- **A disk that comes back is read last.** Its superblock is older than
  the others'. Its copies are read only after theirs until a scrub
  catches it up: the scrub writes the good copy over each block of the
  disk that differs, checked by checksum, or by its bytes where data has
  none. Then the scrub writes every disk its superblock; until then,
  commits and logs leave the disk's own superblock as it came back, so
  it is behind at the next mount too. Linux reads such a disk like any
  other, and data without checksums can come back stale.
  - The kernel log warns at the mount of how many disks missed changes,
    until a scrub catches them up.
- **One superblock a disk.** Each disk's superblock has its own device
  item from the chunk tree. A commit first makes the blocks durable on
  every disk, then writes each disk its superblock and the copies it has
  room for, all at once (ADR 0151). An fsync's log writes each disk its
  superblock alone, as Linux's `btrfs_sync_log` does (ADR 0154), so a
  mount from any disk that holds every chunk replays it. A scrub that
  catches a disk up while a log is on the disks commits instead of
  writing the superblocks, since the commit's would not name the log.
- **One mount a filesystem.** Mounting a disk of a Btrfs already mounted
  fails with `InUse`, unless it is the disk the mount was made from.
  That mount gets the same filesystem, as on Linux, when both are
  read-only or both are writable. Spark leaves such a disk alone, so a
  RAID1 of `vda` and `vdb` appears once, at `/volumes/vda`.

## Consequences

- A two-disk RAID1 boots with either disk pulled out, and warns.
- Writing a filesystem with a disk missing is allowed. Once the disk
  returns, nothing of it is trusted until a scrub has caught it up.
- `btrfs check` from Linux's btrfs-progs checks every image of each
  profile in the host tests. Each test also mounts with each disk
  missing, and with a disk that fails during writes.
- The `btrfs-raid-pull` boot test has QEMU fail one disk of a two-disk
  RAID1 at a random write while the storage test writes it, as a disk
  pulled out fails: every read, write and flush from then on. The writes
  go on to the other disk, and the kernel warns. Linux's `btrfs check`
  then passes from each disk alone: the one lost holds the filesystem as
  it was when it failed. With the disk back, the mount warns that it
  missed changes, every byte reads back, and after a scrub `btrfs check`
  passes from both disks and from each alone.
- Replacing a disk and rebuilding onto it (Linux's `btrfs replace`) is
  ADR 0155. Adding and removing disks come later.
