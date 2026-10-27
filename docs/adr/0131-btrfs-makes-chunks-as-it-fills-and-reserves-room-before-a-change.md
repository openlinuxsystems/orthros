<!--
SPDX-FileCopyrightText: 2026 The Dukovo contributors
SPDX-License-Identifier: Apache-2.0 OR MIT
-->

# 0131. Btrfs makes chunks as it fills, and reserves room before a change

- Status: Accepted
- Date: 2026-10-27

## Context

ADRs 0129 and 0130 write Btrfs's trees and files into the block groups
`mkfs.btrfs` made. Those are small: on a 128 MiB disk, 8 MiB for data and
32 MiB, kept twice, for metadata; on a large disk, 1 GiB each at most.
The rest of the device is unallocated until the filesystem needs it, as
Linux leaves it.

- A chunk is a range of logical addresses and the stripes of the device
  that hold it, once (single) or twice (DUP). The chunk tree names every
  chunk; those that hold the chunk tree itself are also in the
  superblock's system chunk array, which is where reading starts. The
  device tree has an extent for each stripe; the device item counts the
  bytes they take; each chunk has a block group item and free-space info.
- A change copies blocks before it knows how many: a split may copy a
  path to the root. ADR 0129's writer refuses every change after one that
  fails half made, so a change that runs out of room in the middle leaves
  the filesystem read-only until it is mounted again.
- ADR 0130 writes data at write-back. A `write` that the page cache took
  but write-back has no room for would lose its data at a sync, where
  Linux's `write` fails with `ENOSPC`.

## Decision

- **Chunks are made when a kind has no room**: when no block group of
  data, metadata or the chunk tree has room for a block or an extent, a
  chunk of that kind is made with the profile of the groups of its kind.
  It is made in memory first: the first free range of the device that
  fits (from 1 MiB, as Linux leaves the start of a device to boot
  loaders), its logical addresses after every other chunk, and its block
  group's free space, all free but around the superblock's copies. Its
  space may be taken at once, even by the changes that record it. The
  commit records it, as Linux does at the end of the transaction that
  made it: its chunk item (and, for the chunk tree's chunks, its copy in
  the system chunk array), a device extent per stripe, the device item's
  bytes used (in the chunk tree and the superblock), its block group item
  and its free-space info.
- **Their size is Linux's**: 1 GiB for data, 256 MiB for metadata (1 GiB
  on a device over 50 GiB) and 32 MiB for the chunk tree, but at most a
  tenth of the device, rounded up to 16 MiB; and the largest free range's
  worth, in 64 KiB units, when no range is that large, but no less than 1
  MiB. Readers find chunks without a lock: the chunk map is replaced
  whole, under RCU, when a chunk is made.
- **Metadata room before a change**: a change starts only when metadata
  has room for the most blocks it and the commit after it may copy: two
  blocks (a copy and a split) at each of the eight levels, for eight
  paths of the change and eight of the commit (the extent, free-space,
  block group, root, chunk, device and checksum trees, and a subvolume's),
  and a block for each 64 items the commit adds to the extent tree. With
  16 KiB nodes that is 4 MiB. Without it, metadata chunks are made, and
  then a commit frees what the last one pinned; failing all of those, the
  change fails with `NoSpace` before it starts, so it fails whole.
  Removing and cutting files need only the commit's room, so a full
  filesystem can always be made less full, as on Linux.
- **Data room at `write`**: a page about to become dirty takes room for
  its data first (`Backing::reserve_pages`), and gives it back once it is
  written or dropped (`Backing::release_pages`). The room is data's free
  bytes, those the last commit freed, and the unallocated bytes new data
  chunks could hold. A write that finds none fails with `NoSpace`, having
  written what fit, as Linux's does. Each mount counts the room again
  under the transaction's lock after each change, and a reservation takes
  the lock only when the count it has falls short. Write-back that finds
  no data free commits first when the last commit freed enough.
- **Mixed block groups** (`mkfs.btrfs --mixed`), where data and metadata
  share space: data's room leaves metadata room for two changes and a
  32nd of the data, for the items its write-back adds.
- **Free space**: `statfs` reports data's room less what dirty pages
  have taken, as Linux's `df` reports what is available.
- **Mounting to write**: with this, the kernel mounts Btrfs to write
  unless asked for read-only. A Btrfs Radix cannot write yet (one without
  `no-holes`, or with quotas or a feature it only reads) fails with
  `invalid_argument`, as Linux's mount of one with features it cannot
  write fails with `EINVAL`; Spark then mounts it read-only, as it does
  one on a disk that refuses writes (`read_only`, Linux's `EROFS`).

Rejected:

- **Making a chunk only at the commit**: the transaction would have to
  fit in the groups it started with, and a large write would fail with
  room to spare on the device.
- **Reserving metadata per item, as Linux does**: Linux counts each
  change's items and gives back what it did not use at the end. One room
  check per change, sized for the most a change copies, is simpler and
  keeps every change whole; it holds back at most 4 MiB of metadata.
- **Taking data room at write-back**: the program would see its `write`
  succeed and its data lost at the sync.

## Consequences

- A filesystem grows into its device as Linux's does, and a disk that
  fills fails the program's `write` or `create`, never a sync.
- Metadata keeps 4 MiB free with 16 KiB nodes (1 MiB with 4 KiB). On a
  device whose unallocated space data has taken, a create may fail with
  `NoSpace` while data still has room, as on Linux.
- A writable mount reads every block group under the mount call, which
  went past the 16 KiB kernel stack in the first boot test: each
  filesystem's mount now has its own frame, the volume is on the heap,
  each of the mount's reads has a frame of its own, and with the lock
  checker on the scheduler warns, with a backtrace, when a thread sleeps
  with less than 4 KiB of stack left, which fails a boot test
  (docs/design/storage.md, Kernel stacks).
- Empty block groups stay, where Linux removes them in the background;
  that comes with the work on block groups that relocation needs.
- Host tests make chunks of each kind and check them with `btrfs check`,
  fill filesystems through files past their first chunks (single, DUP and
  mixed), and fill them until a write fails, then empty and fill them
  again.
- Boot tests: `btrfs` mounts a disk without `no-holes` and a read-only
  one, both read-only; `btrfs-write` runs the storage test on two Btrfs disks,
  past their first data chunk, with Linux's `btrfs check` after each boot
  and `btrfs restore` reading back what was written; `btrfs-power-cut`
  kills QEMU while files are written, synced, renamed and removed, and
  checks that every synced file is whole and every removed one gone, by
  `btrfs restore` and after a reboot.
