<!--
SPDX-FileCopyrightText: 2026 The Dukovo contributors
SPDX-License-Identifier: Apache-2.0 OR MIT
-->

# 0129. Btrfs writes by copying on write, and a commit flushes its blocks before its superblock

- Status: Accepted
- Date: 2026-10-27

## Context

ADRs 0125, 0127 and 0128 read Btrfs and mount it read-only. Phase 10c
writes it, and its root filesystem moves onto Btrfs, so a power cut at
any moment must leave a filesystem that mounts, that Linux's
`btrfs check` passes, and that holds every change a finished `fsync` or
`sync` promised.

- Btrfs is a forest of B-trees. The superblock names the root tree and
  the chunk tree; the root tree names the others (the extent, checksum,
  free space, device and subvolume trees). Each pointer to a block
  carries the block's generation, which the read path already checks
  ("parent transid", ADR 0125).
- Btrfs never changes a block where it is once that block is on the
  disk for a committed generation. A change copies the block to a free
  place, and its parent with it, up to the root.
- A disk keeps writes in a cache and makes them durable in any order. A
  flush makes every write it has completed durable. A write with FUA is
  durable when it completes, and orders nothing before it.
- Linux lets the next transaction start while a commit is still writing
  its blocks, and writes a running transaction's blocks early when memory
  runs short. A block written early carries a flag that says so, and is
  copied again before it changes.

Getting the order wrong does not show in ordinary tests: it shows only
when power is cut between two writes. So the order is checked by a model
first, and the code is tested by cutting power at every point it can.

## Decision

- **Every change copies on write**: tree blocks and file data alike. A
  block of the running transaction that has not been written may change
  in place; any other is copied to a new place first, and its parent
  with it, up to the root. A data write goes to a new extent, and the
  file's extent item points to it only once the data's write completes,
  so a tree never points to data that is not written.
- **Freed places are pinned**: a place a copy leaves, and an extent a
  file stops using, is free to take only once the superblock of the
  commit that stopped using it is durable. Until then the tree on the
  disk may still need it.
- **A commit, in order**:
  1. The running transaction stops taking new changes, and the changes
     it already took finish.
  2. The extent tree's and free space tree's changes, queued per
     transaction, are applied. Applying them copies blocks of those trees,
     which queues more changes; this repeats until nothing is queued.
  3. Each changed tree's root goes into the root tree, and the root tree's
     and chunk tree's roots into the new superblock. The next transaction
     starts here.
  4. Every block of the commit is written, and the commit waits for the
     writes.
  5. The disk's cache is flushed.
  6. The superblock is written to every copy the device has room for
     (64 KiB, 64 MiB and 256 GiB), the first with FUA, and the commit
     waits for them.
  7. The places the commit's transaction pinned are freed.
- **Commits come** every 30 seconds, as Linux's default `commit=30`, which
  the mount option changes; on `sync` and `syncfs`; on `fsync` until the
  log tree (10c.2); and when the reserved space of the running
  transaction is used up.
- **Space is reserved before a change starts**, for the most blocks it
  can copy, so a change that has started never fails for space in the
  middle. A part of the metadata space is kept back for deleting files,
  directories and snapshots, and for the commit itself, so a full disk
  can always be made less full.
- **Free space** comes from the free space tree, kept by every commit. A
  filesystem with no free space tree gets one at its first read-write
  mount, as Linux makes one when mounted with `space_cache=v2`. A
  filesystem with an incompatible or read-only-compatible feature
  Radix does not write mounts read-only, as on Linux.
- **The model**: `kernel/fs/btrfs/tla/Commit.tla` holds a tree of a root
  and two leaves on a disk with a write cache, and lets power be cut in
  any state. `ob test tla` checks it in two configurations (six places
  with one value, five places with two) for: the superblock on the disk
  leads to a whole tree, each block where its parent says and of the
  generation it says; that tree holds the values of the transaction the
  superblock committed; and a commit known to be done is on the disk, or
  a later one is. Each of the three rules above is needed: without the
  flush before the superblock, TLC finds a superblock durable before its
  blocks; without pinning, a place the durable tree uses taken by the
  next transaction; without copying a block written early, a change the
  commit never writes.
- **Tests**:
  - Host tests record every write and flush the filesystem makes. Each
    point between two flushes, with any subset of the writes after the
    last flush lost, makes a disk image, which must mount, read back the
    last durable commit, and pass `btrfs check`.
  - Boot tests write, delete and `fsync` in a loop, and QEMU is killed at
    a random point; the disk must then pass `btrfs check` and hold every
    file whose `fsync` returned.
  - CI runs `btrfs check` on every image Radix writes.

Rejected:

- **Writing the superblock with FUA alone, without the flush before it**:
  FUA makes the superblock durable, not the blocks it leads to.
- **Freeing a place as the transaction that copied it ends**: the
  superblock on the disk may still lead to it until the next one is
  durable.
- **Starting the next transaction only once a commit is durable**: simpler,
  but every writer would wait out the commit's writes and flush, up to
  the disk's whole cache, where Linux's do not.

## Consequences

- A power cut loses at most the changes since the last durable commit,
  and never the filesystem.
- `fsync` costs a whole commit until the log tree comes in 10c.2. The
  `ob bench` rows for `fsync` on Btrfs come with the log tree, and are
  held to the same gate as every row.
- Every change may copy a path of blocks to the root, and the extent
  tree's changes add more; queuing those changes per transaction keeps
  the copies to one per block per transaction.
- The model checks the order of writes, not the B-tree code. The
  power-cut tests and `btrfs check` check the code.
