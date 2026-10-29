<!--
SPDX-FileCopyrightText: 2026 The Dukovo contributors
SPDX-License-Identifier: Apache-2.0 OR MIT
-->

# 0170. Btrfs keeps simple quotas, each extent counted to the subvolume that wrote it

- Status: Accepted
- Date: 2026-10-29

## Context

Phase 10e.3 asks for Btrfs quotas: how much each subvolume takes, and
limits on it. Linux has two kinds. Its quota groups (qgroups) count, for
each subvolume, the bytes it refers to and the bytes only it refers to;
keeping those right means walking the backrefs of every extent a
transaction changes, which is slow, and a rescan of the whole
filesystem when they go wrong. Linux 6.7's simple quotas (`btrfs quota
enable --simple`, incompat flag `SIMPLE_QUOTA`) count each extent once,
to the subvolume that wrote it, for as long as it lives, whoever shares
it since: a snapshot costs its source nothing, and a subvolume removed
leaves what its snapshots still use counted to it. Data extents name
their owner in an inline `EXTENT_OWNER_REF`; a tree block's owner is in
its header. Only extents written since quotas were turned on (the
status item's `enable_gen`) count. `btrfs check` checks the counts.

## Decision

- Radix keeps simple quotas, and not Linux's full quota groups:
  radix-directory(2)'s `quota` fails with not_supported on a filesystem
  that has those on, and the first commit that changes it marks their
  counts inconsistent, as Linux does when it cannot keep them, so
  `btrfs quota rescan` mends them.
- radix-directory(2) gains `quota(enable)`, `qgroups(buffer) -> count`
  and `limit_qgroup(id, max_referenced, max_exclusive)`, and the file
  error `quota_exceeded`, which is EDQUOT. Turning quotas on makes the
  quota tree with its status item (on, simple, `enable_gen` the next
  transaction, as Linux's: the running one may hold extents that name
  no owner), a quota group of level 0 for each subvolume, at zero, and
  sets `SIMPLE_QUOTA`; turning them off removes the quota tree and
  clears the flag, and data extents keep naming their owner, as on
  Linux.
- While they are on, a data extent's item names its owner first, and
  each data extent and subvolume tree block made with a generation
  from `enable_gen` on adds its bytes to its owner's group, and takes
  them away when it is freed, as Linux's `btrfs_record_squota_delta`
  does, groups above level 0 that a filesystem has included; the
  commit writes the changed groups' info items before the extent tree
  catches up, which changes no subvolume. A new subvolume or snapshot
  gets its group with its first block.
- A write whose dirty pages would take a group with a limit past it
  fails with quota_exceeded as the pages are dirtied, before anything
  is written, as a write short of room fails with no_space (ADR 0131);
  removing files never does.
- A balance (ADR 0159) gives a data extent it moves its old owner.
- `quota`(8) is the systools front end, with btrfs-progs' `quota
  enable`, `quota disable`, `qgroup show` and `qgroup limit`.

## Alternatives

- **Linux's full quota groups.** Their counts are what most users
  mean by "how much this snapshot frees", but each change walks
  backrefs, and the counts go inconsistent on any mistake. Simple
  quotas are what Linux recommends for new filesystems with many
  snapshots.
- **Count in memory only, from a walk at the mount.** No format to
  keep, but nothing Linux reads, and a walk of every extent at each
  mount.

## Consequences

- A subvolume's group counts what it wrote, not what it holds: a
  snapshot's group starts at zero, and the extents it shares stay
  counted to the source until they are freed.
- Extents written before quotas were turned on count nowhere, as on
  Linux.
- The host tests turn quotas on, write, snapshot, clone, remove and
  balance, and check each group's counts against `btrfs check` and the
  limits against the writes they stop; the btrfs-raid boot test turns
  them on before it replaces, adds and removes disks and balances, and
  `btrfs check` counts every group again after it.
