<!--
SPDX-FileCopyrightText: 2026 The Dukovo contributors
SPDX-License-Identifier: Apache-2.0 OR MIT
-->

# 0138. Btrfs snapshots and subvolumes, with Linux's back references

- Status: Accepted
- Date: 2026-10-28

## Context

Roadmap 10c makes subvolumes and snapshots. Phase 15's `snap` and the
update slots of Phase 13 are built on them, and the root of ADR 0137 is
already subvolumes, made by `mkfs.btrfs`. Until now Radix wrote only a
filesystem whose blocks no two trees share: a writable mount refused
one with a snapshot in it, and a new tree block had one reference, its
tree's. A snapshot shares every block of its source until one of the
two changes it, so each block must say who holds it, the way Linux and
`btrfs check` expect, or the disk is not Linux's.

## Decision

- **Back references as Linux keeps them.** A tree block's extent item
  counts its references and holds each: `TREE_BLOCK_REF` (the tree that
  owns it) or `SHARED_BLOCK_REF` (the address of a block above it); a
  data extent's holds `EXTENT_DATA_REF` (tree, inode and file offset,
  hashed as Linux hashes it) or `SHARED_DATA_REF`. They are inline in
  the item while it has room, as Linux puts them, and items of their own
  after. A block whose references are by address carries
  `BTRFS_BLOCK_FLAG_FULL_BACKREF`, and so do the references of what it
  points to.
- **Copy-on-write of a shared block.** A block that a tree with a
  snapshot (`last_snapshot`) made at or before the snapshot may be
  shared. Its copy takes its references as Linux's
  `update_ref_for_cow` does: a block with more than one reference keeps
  them, gives up the copying tree's, and its children gain the copy's
  (by tree, or by the old block's address when the tree owns it, which
  then becomes `FULL_BACKREF`); a block with one reference that is
  `FULL_BACKREF` hands its children's references to the copy and is
  freed. A block with one reference that another tree owns without the
  flag is corruption.
- **Delayed reference changes.** Those changes are made inside a tree
  walk, which holds the path it is changing. They are kept in a list and
  made after each change of the filesystem, before the next; a lookup of
  a block's count adds what the list holds for it. Linux delays its
  reference changes to the commit; Radix's commit needs them made
  before it counts the extent tree, so the list is short.
- **Snapshots.** `snapshot` commits first, so the snapshot holds what
  was written, then sets the source's `last_snapshot`, copies its root
  block for the new tree (whose children gain a reference by tree), and
  writes the new root item as Linux's `create_pending_snapshot` does:
  a new UUID, the source's as parent, the transaction's generation, and,
  for a read-only one, `BTRFS_ROOT_SUBVOL_RDONLY`; the name's
  `DIR_ITEM`, `DIR_INDEX`, `ROOT_REF` and `ROOT_BACKREF`, and the UUID
  tree's entry. It commits again. A subvolume inside the source is, in
  the snapshot, an empty directory, as Linux shows it (the stale entry
  with inode 2, `BTRFS_EMPTY_SUBVOL_DIR_OBJECTID`); `rmdir` removes it.
- **New subvolumes.** `make_subvolume` writes a new tree's leaf with its
  top directory, inode 256, and the root item, entries and references
  Linux's `create_subvol` does.
- **Removal.** `remove_subvolume`, and `rmdir` of an empty subvolume as
  Linux 4.18 allows, take the name and its references away and drop the
  tree in the same transaction, as Linux's `btrfs_drop_snapshot` does:
  a walk of its own path, not a call a level, that frees each block and
  data extent only that tree refers to and takes its references from
  the others. A block the tree shares with a snapshot made after it gave
  its blocks references by tree is first made `FULL_BACKREF`
  (Linux's `UPDATE_BACKREF` stage), so what the snapshot holds stays
  held. Its root item goes with the walk, so a power cut leaves the
  last commit: the subvolume, whole.
- **The interface.** The directory interface gains `make_subvolume`,
  `snapshot` (the subvolume that holds a directory, into another
  directory of the same filesystem, read-only if asked) and
  `remove_subvolume`. `subvolume(8)` is Linux's `btrfs subvolume` with
  `create`, `snapshot [-r]` and `delete`, and its messages.
- **What is not yet.** Removing a subvolume with a file of it open, or
  the default subvolume, fails with `EBUSY`; Linux lets open files live
  until they close and refuses the default with `EPERM`. Removing a
  subvolume that holds another fails with `ENOTEMPTY`, as on Linux.
  Reflinks (`FICLONE`), quotas, `send` and `receive`, and setting the
  default subvolume come later.

## Alternatives

- **Linux's background cleaner.** Linux removes a subvolume's name at
  once and drops its tree over many transactions, keeping its progress
  in the root item, an orphan item and the `DEAD_ROOT` list. A walk in
  one transaction needs none of that. A tree too big for one
  transaction's reserve is the case that brings the cleaner back.
- **Reference counts without back references.** Counting alone would
  be simpler, but `btrfs check` reads every back reference, Linux's
  relocation and scrub need them, and a disk without them is not
  Linux's format.
- **Delay reference changes to the commit, as Linux does.** The commit
  would have to merge them while it counts the extent tree, and a
  lookup would have to search a tree of them; after each change, the
  list holds a walk's worth.
- **Snapshot only what is synced.** Linux's snapshot holds what was
  written to its source's files and not yet synced (it flushes them).
  So does Radix's, by committing first.

## Consequences

- A writable mount writes a filesystem with snapshots; the test
  writing one and removing every subvolume again, against a model, and
  `btrfs check` after each, is `snapshots` in radix-btrfs's host tests.
- The `root` boot test snapshots `@users` read-only into `@snapshots`,
  writes to both, makes and removes a subvolume, and removes the
  snapshot at the second boot; `btrfs check` checks the partition after
  each boot.
- Removing a large subvolume takes as long as a walk over every block
  it alone holds, in one call.
