<!--
SPDX-FileCopyrightText: 2026 The Dukovo contributors
SPDX-License-Identifier: Apache-2.0 OR MIT
-->

# 0158. Btrfs is trimmed under the transaction, a batch at a time

- Status: Accepted
- Date: 2026-10-29

## Context

Phase 10e.2 asks for fstrim(8). A solid state disk that is told which
of its blocks hold nothing erases them ahead of time, and writes faster
and wears less; Linux's fstrim asks a mounted filesystem with FITRIM to
discard what it does not use, and most distributions run it weekly.

What Btrfs does not use is of two kinds: the free space of its block
groups, and each disk's bytes no chunk takes. A discard makes what it
covers read as zeros or as anything, so a trim that reaches one block a
tree or a file holds loses it, and one that reaches a block the last
commit holds loses it after a power cut, though the running transaction
freed it. Linux's btrfs trims each block group's free space while the
group's cache marks the extent being trimmed, and the unallocated space
of each device under the chunk mutex.

Radix keeps a block group's free space in memory (ADR 0129), as what the
running transaction may take now: what a commit freed but still holds
is pinned apart until the commit is durable, and the blocks of the log
of fsyncs (ADR 0154) and of a log being replayed (ADR 0151) are kept
out. Chunks are made under the transaction too (ADR 0131).

## Decision

- radix-directory(2) gains `trim(minimum)`, which needs the control
  right, as FITRIM needs CAP_SYS_ADMIN, and returns the bytes it
  discarded, each copy counted, as Linux counts them. Other filesystems
  fail with not_supported, as does a Btrfs on disks that discard nothing.
- Btrfs finds a batch of at most 128 MiB of what it may discard and
  discards it while holding the running transaction, so that no change
  takes those bytes meanwhile, then lets it go and finds the next batch.
  A change waits for one batch at most.
- What it may discard is the free space the transaction may take now,
  ranges of at least `minimum` bytes, on every copy of each, through the
  chunk map as reads go (missing disks passed over); then each disk's
  bytes past its first MiB that no chunk takes, the chunks made since
  the mount included.
- Whatever the free space says, a disk's first MiB and 64 KiB at each
  copy of its superblock are never discarded.
- A disk that fails a discard is passed over; the trim goes on with the
  others and fails with io at the end.
- fstrim(8) is the systools front end, with util-linux's `-v`, `-m`,
  `-a` (every writable mount of a disk) and exit statuses.

## Alternatives

- **Take each batch out of the free space and discard it without the
  transaction**, as Linux's trimming state does. Changes would not wait,
  but one that needs room meanwhile could fail with no_space on a full
  filesystem, which a trim must never cause.
- **Discard as blocks are freed**, as Linux's `discard=async` mount
  option does. It is the better default on many disks, and can come
  later as a mount option; a periodic trim is what distributions run
  and works on every kernel.
- **Trim only the free space of block groups.** A new filesystem's
  space is mostly unallocated, and would never be trimmed.

## Consequences

- A trim of a large, mostly empty filesystem holds the transaction for
  as many short spans as it has batches; a sync waits for one.
- Bytes freed by a commit that is not yet durable are not trimmed until
  the next trim.
- The host tests check that a trim keeps every file and every block
  `btrfs check` reads, on one disk and on a RAID1 with a disk missing,
  across a power cut right after it, and while files change during it;
  the storage-tools boot test trims a Btrfs disk that the host's
  `btrfs check` then checks.
