<!--
SPDX-FileCopyrightText: 2026 The Dukovo contributors
SPDX-License-Identifier: Apache-2.0 OR MIT
-->

# 0154. Btrfs writes a log of fsyncs, as Linux does

- Status: Accepted
- Date: 2026-10-28

## Context

An `fsync` on Radix's Btrfs was a whole commit (ADRs 0129 and 0151): every
tree the transaction changed got its path of new blocks, and the extent,
free-space and root trees theirs. ADR 0151 kept it so until the bench
row `posix_btrfs_fsync` (4 KiB written over a file and made durable)
lost to Linux. It passed on two runner types (0.98 and 0.99), but lost
on CI's AMD EPYC 9V74 runners: 1.69 times Linux on #80's run, 1.86 on
#82's. There Linux took 520 to 580 µs and Radix about 970.

On the bench's disk, a 4 KiB `fsync` wrote five metadata blocks, each
twice because metadata is DUP, plus the data and the superblock with its
mirror: 172 KiB in six writes. Linux's log writes two blocks, each
twice, plus the data and the superblock alone: 72 KiB. Both wait on the
disk about five times. Where the disk is slower, the bytes are what cost.

## Decision

- **An fsync of a regular file writes a log**, in Linux's format
  (`tree-log.c`). Each subvolume gets a log tree, owner
  `BTRFS_TREE_LOG_OBJECTID` (-6). A log root tree holds a root item for
  each log tree, keyed `(-6, ROOT_ITEM, subvolume)`, as
  `btrfs_add_log_tree` makes it. All their blocks are of the running
  transaction (the last commit's generation plus one). The superblock
  the last commit wrote is written again with `log_root` and
  `log_root_level` naming the log root tree, the primary copy alone,
  as Linux's `btrfs_sync_log` writes it.
- **What the log holds of a file:** its inode item, names and extended
  attributes; its extents in the byte ranges the transaction changed,
  with a hole item for each part of those ranges no extent covers, so a
  replay drops what the commit had there; and the checksums of the data
  the transaction wrote that those extents name. The checksums cover the
  whole extent if it is compressed, and the part its item names if not,
  as Linux's `replay_one_extent` looks them up. The ranges are noted
  where every change to a file's extents passes (`Files::drop_extents`,
  and the inline extent of a link).
- **The log holds every file fsynced since the last commit**, and each
  fsync writes it again whole, in new blocks. Once the new superblock is
  durable, the blocks of the log before are free again. An fsync of a
  file whose log items are already on the disk writes nothing.
- **A commit instead**, when:
  - the file is new since the last commit;
  - its names differ from the commit's (a rename, a link or an unlink);
  - the fsync is of a directory or anything other than a regular file;
  - its items are not ones the log knows;
  - the log would pass four leaves of items;
  - the transaction made chunks or set incompatible features;
  - more than 4,096 ranges changed;
  - the log has no room.

  Linux logs more of these itself (new files and their directories,
  renames), and commits for others.
- **Space.** The log's blocks are in no tree, so they come from the free
  space with no change the free-space tree records
  (`Space::take_unrecorded`). They are held until a newer log is
  durable, or until the commit, which drops the log, is. From the
  transaction's first log on, data the transaction wrote and frees again
  waits for the commit, as other freed data does, because a log on the
  disk may name it. A crash leaves free space the commit has as free,
  and the replay (ADR 0151) keeps the log's blocks from use and takes
  its data.
- **The replay drops a hole's range** on a filesystem without hole items
  (`no-holes`, which every disk Radix writes has), as Linux's replay
  does, rather than inserting the hole item.

## Alternatives

- **Linux's incremental log.** Linux copies a log tree's changed paths
  on each fsync, as a commit does, and logs new files, their
  directories and renames, with rules for when an unlink or rename
  forces a commit (some 7,500 lines). Writing the log whole is far less
  code. A log of four leaves at most costs no more to write than an
  incremental one in the common case of a file or a few, and it commits
  past that.
- **Trimming the commit.** Leaving out the mirror superblock, or the
  free-space tree's changes, would still write the extent tree's and the
  root tree's paths: about twice Linux's bytes.
- **Waiting for the data before writing the log**, as Linux does
  (`filemap_fdatawait_range`). Radix writes the data first and the log
  blocks after, and then one flush makes both durable.

## Consequences

- The fsync row writes 72 KiB: the data, two log blocks twice each, and
  the superblock. That is one flush and one write through, as Linux's
  log does. A host test holds it to that.
- A power cut at every flush and write through, and at random writes
  between them, keeps what each fsync made durable. The host test covers
  files overwritten, grown, cut, grown with holes, in a directory,
  changed and not synced, and then a new file and a renamed one, whose
  fsyncs commit. Linux's `btrfs check` passes before and after the
  replay.
- `ob fixture btrfs-radix-log` has Linux replay a log Radix wrote. A
  host test writes files, changes them with an fsync after each change
  and cuts the power. Linux 6.8 lists the disk before and after its
  replay, and `btrfs check` checks what its replay left. A host test
  checks that Radix's replay of the same disk lists as Linux's does.
- A file's fsync no longer commits the transaction's other changes:
  what is in other files' pages, and the names made since the last
  commit, wait for their own fsync, a sync or the commit every 30
  seconds, as on Linux.
- ADR 0151's "`fsync` stays a commit" is replaced by this one.
