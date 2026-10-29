<!--
SPDX-FileCopyrightText: 2026 The Dukovo contributors
SPDX-License-Identifier: Apache-2.0 OR MIT
-->

# 0151. Btrfs replays the log of fsyncs Linux leaves, at mount

- Status: Accepted
- Date: 2026-10-28

## Context

Linux makes an `fsync` on Btrfs durable without a commit: it writes what
the file (and, for a directory, its entries) changed since the last
commit into a log tree per subvolume, under a log root tree the
superblock names (`log_root`, `log_root_level`), and the next commit
drops the log. A machine that stops between the two leaves a disk whose
last commit lacks those changes; Linux's mount replays the log before it
goes on, and with `rescue=nologreplay` refuses to, mounting read-only.
Radix refused any such disk (ADRs 0125 and 0130), so a disk Linux used
last, as on a machine that starts both, would not mount after a crash.

Radix's own `fsync` is a whole commit (ADR 0129). Writing a log tree is
the larger half of the work, and is only worth it if a commit loses to
Linux's log on the same `fsync`.

## Decision

- **Replay at mount.** A mount of a disk whose superblock names a log
  replays it in the first transaction, before anything else, and commits
  it with `log_root` cleared. A read-only mount replays too, as Linux's
  does, and then writes nothing more; a disk that cannot be written
  fails with `ReadOnly`, as Linux fails without `nologreplay`.
- **What is replayed**, as Linux's `replay_one_buffer` does, for each
  subvolume's log in order of inode:
  1. Each logged inode gets the log's inode item. A logged directory
     loses the entries in the ranges its `DIR_LOG_INDEX` items say the
     log holds whole but that the log lacks, and those entries' inodes
     lose that name; a logged file loses its extents past the logged
     size; extended attributes the log lacks go.
  2. The log's `DIR_INDEX` entries are linked, with their `DIR_ITEM`s.
  3. Then its `INODE_REF`s and `INODE_EXTREF`s, its file extents with
     their checksums (an extent the extent tree lacks is made, one it
     has gains a reference; an item as the subvolume already has it is
     left), and its extended attributes.
  4. Each inode the replay touched is counted again: its links from its
     names, a directory's size from its entries, a file's bytes from its
     extents. One left with no name is deleted.
- **The free-space tree.** The log's own blocks and the data its extents
  name are in no commit, so the free-space tree has them free. The log's
  blocks are kept from use for the mount's life (they are read first,
  whole); its data is taken before anything else is.
- **`fsync` stays a commit.** `ob bench` gains `posix_btrfs_fsync`: 4
  KiB written over a file and made durable with `fsync`, 200 times,
  which on Linux writes the log. It is held to the gate as every row is.
  A log writer comes only if that row loses.
- **The test data is Linux's.** `ob fixture btrfs-log` boots Linux with
  `linux-bench --btrfs-log` as process 1: it commits a set of files,
  changes them with an fsync after each change (new, overwritten,
  grown, cut and compressed files, an unlinked name, a hard link, a new
  directory with an inline file, extended attributes added, changed and
  removed, a symbolic link), and powers off; then, on a copy, lists the
  disk as its last commit has it and as Linux replays it. The disk, kept
  as its blocks that are not zeros, and the lists are radix-btrfs's
  fixtures.

## Alternatives

- **Write the log too, as Linux does.** `fsync` would write a file's
  changes and their path of log blocks instead of a commit's paths of
  every changed tree. It is most of Linux's `tree-log.c` (some 7,500
  lines) and its rules for when a rename or unlink forces a commit; the
  bench row decides whether it is needed.
- **Mount read-only without replaying.** Simpler, but files would show
  as they were before the fsyncs, and the next writable mount would have
  to replay anyway.
- **Refuse the disk, as before.** A crash on Linux would leave a disk
  Dukovo cannot mount until Linux mounts it again.

## Consequences

- radix-btrfs's host tests mount the fixture and check that every file,
  directory and link lists as Linux lists it after its replay (mode,
  owner, links, size, contents, extended attributes), that `btrfs check`
  passes after the replay and after more changes, that the replayed disk
  mounts again the same, and that a read-only disk is refused.
- `ob fixture btrfs-log` needs a Linux kernel with Btrfs, as `ob bench`
  does; the fixtures are checked in, so tests need no Linux.
- The replay writes, so a disk Radix does not write (without
  `no-holes`, with quotas, ADR 0130) and with a log is refused, on a
  read-only mount too, until Linux mounts it once.
- Directory entries that name another subvolume are left as they are:
  Linux commits when one is made or taken, so its log never holds them.
- The row's first run in CI lost, at 5.27 times Linux: the commit was
  not the cost, the block cache was. It wrote a commit's tree blocks a
  page per request, waiting for each, and looked at every page it held
  to find the dirty ones. It now writes the runs of dirty sectors that
  follow one another in one request, and notes its dirty pages, so a
  4 KiB `fsync` waits on the disk 9 times rather than about 70, and its
  kernel work fell from about 1.5 to 0.57 million instructions. That
  made 1.45 times Linux: each request still waited for the one before.
  A disk now takes several writes at once (`BlockDevice::write_all`,
  the device told of them together, as Linux plugs its bios), so a
  commit's blocks cost one wait, and the superblock goes with its copies
  in another, as Linux's `write_all_supers` sends them: the data, the
  blocks, a flush, the superblocks and a flush, five waits, where
  Linux's log takes four.
- A rename into another directory followed by an `fsync` commits on
  Linux 6.8, so the fixture cannot hold one; the replay links and
  unlinks such names as it does any other.
- ADR 0154 replaced "`fsync` stays a commit": the bench row lost on CI's
  AMD EPYC 9V74 runners, so Radix writes the log too.
