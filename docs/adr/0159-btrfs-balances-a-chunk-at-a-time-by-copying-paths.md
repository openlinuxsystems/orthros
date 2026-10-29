<!--
SPDX-FileCopyrightText: 2026 The Dukovo contributors
SPDX-License-Identifier: Apache-2.0 OR MIT
-->

# 0159. Btrfs balances a chunk at a time, copying the paths to what it moves

- Status: Accepted
- Date: 2026-10-29

## Context

Phase 10e.3 asks for Linux's `btrfs device add`, `btrfs device remove`
and `btrfs balance`, with `convert`. All three come down to one thing:
moving what a chunk holds to other chunks, then taking the chunk out.
A disk added holds nothing until chunks move to it; a disk removed must
have every chunk moved off it; a convert moves each chunk to a new one
of another profile.

Linux's relocation builds a reloc tree for each subvolume, a copy of
its tree that points at the moved blocks, and merges it back, so that
blocks snapshots share stay shared; data goes through the page cache of
an inode of its own. It is the most intricate code in Btrfs, and much
of it serves sharing that Radix's writer keeps differently: a block's
backrefs (ADR 0138) tell every tree that reaches it, and a change below
a shared block already copies the path down to it in each tree that
changes it (ADR 0138's `update_ref_for_cow`).

## Decision

- radix-directory(2) gains `add_disk(device) -> devid`,
  `remove_disk(devid)` and `balance(kinds, convert_data,
  convert_metadata, convert_system) -> (considered, moved)`, each
  needing the control right. The profiles are Linux's balance argument
  bits, with `1 << 48` for single.
- A balance moves one block group at a time. The group is closed first:
  nothing new goes in it, and its free space counts as no room. A
  commit makes what the running transaction put in it the disk's, then
  batches of up to 64 MiB of its extents move, each batch committed and
  the transaction let go between them, so changes wait one batch at
  most. Once the extent tree names nothing in it and the commits have
  settled what it freed, its block group item, free-space items, device
  extents, chunk item and any entry in the superblock's system chunk
  array go, and a commit makes that durable. Chunk ranges are never
  reused, so a reader that saw the old map finds no chunk rather than
  other data. A move that fails opens the group again.
- A tree block moves as a change below it would move it: the trees that
  reach it are found through its backrefs (a tree's own reference, or a
  shared parent's, followed up), and in one of them the path down to it,
  by its first key, is copied, it included (`copy_block`). Its
  references change as any copy changes them, so the next round finds
  the trees left, until the last copy frees it.
- A data extent's bytes are copied to a new extent, each sector from a
  copy that reads and whose checksum matches, or, with no checksums,
  from a disk that missed no commit. The checksums are copied with it.
  Each file extent item that names it, found through its references (a
  file's offset in a tree, or a shared leaf followed up to a tree), is
  pointed at the new extent, and its reference moves there; the last
  one moved frees the old extent and its checksums at the commit.
- New chunks take the profile a convert names for their kind while the
  balance runs, chosen from the disks as every chunk is. A profile that
  needs more disks than are there is refused, as are RAID5 and RAID6,
  which Radix does not write, and a convert of chunks that mix data and
  metadata. The `soft` filter leaves chunks of the target profile.
- A disk is added into one of the volume's places kept at the mount for
  disks added, sixteen in all: its old superblocks are wiped, its device
  item (another disk's, with its own ID, size and UUID) goes in the
  chunk tree, the superblock counts it and its bytes, and the commit
  writes it its superblock. A disk is removed by letting no chunk go on
  it, moving each chunk with a stripe on it, then deleting its device
  item and error counts; it is no longer written, the superblock stops
  counting it, and once that commit is durable its superblocks, if it
  is there, are wiped.
- balance(8) and device(8) are the systools front ends, with
  btrfs-progs' options and messages.

## Alternatives

- **Linux's reloc trees.** They keep shared blocks shared, but they are
  large, and their merge is where Linux's relocation bugs have been.
  Radix's writer already copies paths correctly under sharing, and
  `btrfs check` checks the result.
- **Relocate by reading every file**, as a copy of the filesystem
  would. It loses sharing in the same way, and is slower, since it moves
  what the chunk does not hold too.
- **Remove a disk by shrinking it**, as Linux's `btrfs_shrink_device`
  does. Moving the chunks that have a stripe on it is the same move as a
  balance's, with one place to test.

## Consequences

- Blocks and extents that snapshots or clones share are copied once for
  each tree that reaches them along the paths a balance copies: a
  filesystem with many snapshots takes more room after a balance, where
  Linux's takes none. man balance(8) says so.
- A balance holds the transaction for one batch at a time; a sync waits
  for one.
- A disk removed while mounted leaves its place used until the next
  mount, so sixteen disks at most can be added and removed between two.
- The host tests balance every chunk of a filesystem with snapshots,
  clones, a read-only snapshot and a tree of two levels, compressed
  extents, a one-disk filesystem given a second disk and converted to
  RAID1 and back, and a disk removed, there or missing, each read back
  and checked by `btrfs check`; the posix suite runs both tools on a
  filesystem that refuses them.
