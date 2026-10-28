<!--
SPDX-FileCopyrightText: 2026 The Dukovo contributors
SPDX-License-Identifier: Apache-2.0 OR MIT
-->

# 0141. ext4 is our own implementation from its documentation, journaling whole blocks at each sync

- Status: Accepted
- Date: 2026-10-28

## Context

ext4 is Linux's default filesystem. Most Linux installs use it, and so
do many servers, external disks and the disks a Dukovo machine will be
asked to read when it replaces a Linux one. Phase 10d.3 reads and
writes it, with its journal, so that a power cut leaves the volume
whole.

Linux's ext4 and jbd2 code and e2fsprogs are GPL, and are not read
(the rule for every GPL source in this project). The on-disk format is
described in Linux's documentation, "ext4 Data Structures and
Algorithms" (docs.kernel.org/filesystems/ext4), as LZO's was for the
Btrfs decoder: the layout of the superblock, group descriptors, inodes,
extent trees, directories and their hash index, checksums, and the
journal's blocks. e2fsprogs is used only as tools on the host.

A journal can be written as Linux's jbd2 writes it, transaction by
transaction, with revoke records so that a replay does not overwrite a
block freed and reused since. That bookkeeping is the subtle part of
jbd2. Radix syncs a volume's metadata every few seconds, as it does for
FAT and Btrfs, so a simpler journal is enough.

## Decision

The `radix-ext4` crate, a compartment without `unsafe` code, reads and
writes ext4 (and ext2 and ext3) volumes:

- **Metadata goes through a block cache** and every change waits there
  for the next commit. A commit writes every block the transaction
  changed to the journal (descriptor blocks, the copies, a flush, then
  the commit block written through), then the blocks to their homes,
  then flushes. Each transaction starts at the journal's first block,
  and its journal superblock, written with the descriptors, says so: the
  journal holds one transaction at most, its homes are written before
  the next starts, and no revoke records are needed. A replay of a
  transaction already at home writes the same bytes again. A
  transaction that grows past a quarter of the journal is committed
  before the change that would grow it.
- **File data is ordered**, as Linux's `data=ordered`: blocks are taken
  for a file's pages only when they are written back (delayed
  allocation), and the data reaches the disk before the transaction
  that maps it commits. Blocks freed by a transaction are not taken
  again until it commits. Writing promises room first, so a full volume
  fails the write, not the write-back.
- **Mounting** replays the journal Linux left: v2 and v3 checksums,
  64-bit block numbers, revoke records, escaped blocks. A read-only
  mount replays it into memory and writes nothing; a writable one
  replays it to the disk. A writable mount marks the volume as needing
  recovery (or, without a journal, as not cleanly unmounted), and frees
  the inodes on the orphan list; a clean unmount clears both.
- **Old forms are changed as they are touched**: a file in an ext3 block
  map gets extents at its first change, and a directory with a hash index
  loses the index when a name is added (removing a name keeps it, as on
  Linux). Linux reads both, and `e2fsck -D` indexes again.
- **A file removed while open** goes on the orphan list and keeps its
  blocks until its last reference goes; the next commit frees it, or
  the next mount after a power cut.
- **Features Radix reads but does not write** (no extents, quotas,
  inline data, encryption, case folding, `EA_INODE`, MMP, `sparse_super2`,
  an orphan file in use, an external or fast-commit journal) fail a
  writable mount with `NotSupported`, and Spark mounts the volume
  read-only, as for Btrfs (ADR 0130). Clusters of several blocks
  (`bigalloc`), blocks larger than 4 KiB and inodes larger than 1 KiB are
  not read at all. Files kept in their inode (`inline_data`) read as
  `NotSupported`.
- A change that fails halfway gives the volume up: later writes fail
  with `Io`, and nothing more reaches the disk, which holds what the
  last commit left.

## Consequences

- The tests make volumes with e2fsprogs' `mkfs.ext4` in 13 kinds (block
  sizes of 1, 2 and 4 KiB, with and without `metadata_csum`, 64-bit,
  journal, `flex_bg` and `meta_bg`, small inodes and small groups), and
  `e2fsck -fn` must find nothing wrong after every change; `debugfs`
  reads back what Radix wrote. A volume Linux's kernel wrote, with
  transactions in its journal, is replayed (`src/fixtures`); and a power
  cut at points of a workload, a disk that stops taking writes, must
  leave a volume that both Radix and `e2fsck` replay to the same files.
  A model test, damaged volumes, fuzzed metadata and two Kani proofs
  complete them.
- The boot tests `ext4-write` and `ext4-power-cut` check the same on
  disks in QEMU.
- Every sync with changes writes their blocks twice, as jbd2 does, and
  waits for two flushes. Batching several syncs into one transaction,
  and a log of more than one, can come later without changing the
  format on the disk.
- ext4 has no bench rows yet: the disk rows measure Btrfs (ADR 0132).
