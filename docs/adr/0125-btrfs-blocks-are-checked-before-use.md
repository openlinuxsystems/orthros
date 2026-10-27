<!--
SPDX-FileCopyrightText: 2026 The Dukovo contributors
SPDX-License-Identifier: Apache-2.0 OR MIT
-->

# 0125. Btrfs is read by checking every block before use

- Status: Accepted
- Date: 2026-10-27

## Context

Btrfs is Dukovo's native filesystem (ADR 0004), and Radix gets its own
implementation of it, compatible with Linux's on-disk format. Phase 10b
reads it; 10c writes it. Reading comes first because everything after
it rests on it: the superblock names the root tree and the chunk tree,
the chunk tree says where every logical address lies on the disks, and
every file, directory and subvolume is an item in a tree found from the
root tree.

A filesystem is read from a disk that may be damaged, written by another
system, or made to attack the reader, and Radix's filesystems run in the
kernel. Linux checks each tree block as it reads it: its checksum, then
its "tree checker", which holds the block's header and items to what the
format allows. A block that fails is read again from another copy, when
the chunk keeps one (the DUP and RAID1 profiles).

Btrfs has four checksums: CRC-32C, the default, xxHash64, SHA-256 and
BLAKE2b. The kernel is built without SIMD registers, so each must be in
portable code.

## Decision

The `radix-btrfs` crate reads Btrfs, from its published documentation
(btrfs.readthedocs.io, "On-disk format" and "Tree checker"), with no
code taken from Linux or btrfs-progs. Its first part, 10b.1, reads the
superblock, the chunks and the trees:

- **The superblock** is read from its first copy, at 64 KiB, as Linux
  mounts. Its checksum must match, its own address must be 64 KiB, its
  sector size a power of two from 4 KiB to 64 KiB, its node size a power
  of two from the sector size to 64 KiB, its roots aligned and their
  levels at most 7. A disk smaller than its device item says has lost
  its end, and is refused.
- **Features.** Radix reads every `incompat` feature but four. Zoned
  devices, the second extent tree and the RAID stripe tree change where
  things are; RAID5 and RAID6 are never offered by Dukovo, as they are
  unsafe even on Linux (roadmap, Phase 10e). A filesystem with one of
  them, or with a feature Radix does not know, is refused, not read
  wrongly. Two more wait for later sub-phases: a filesystem on more than
  one device (10e), and one with a log of fsyncs not yet replayed (10c,
  which writes; Linux replays the log when it mounts, so reading without
  replaying it would show files as they were before the fsyncs).
- **All four checksums**, each Radix's own code: CRC-32C by tables of
  eight bytes at a time, xxHash64 by its specification, SHA-256 by FIPS
  180-4 and BLAKE2b by RFC 7693, as `radix-random` has BLAKE2s. They need
  no outside crate, and so no license or advisory to follow; what they
  hash is on the disk for anyone to read, so they need not run in
  constant time. Tests check them against their specifications' examples,
  Python's `hashlib`, and filesystems `mkfs.btrfs` made with each.
- **Chunks** come from the superblock's system chunk array, which holds
  the chunks of the chunk tree, and then from the chunk tree itself,
  whose chunks must agree with the array's. Each chunk is checked as the
  tree checker does: its size and start aligned, a power-of-two stripe
  length, one profile at most, and the stripes the profile needs. A
  logical address maps to every copy on the device: one for single, two
  for DUP, and for the striped profiles by their stripe arithmetic.
- **Every tree block is checked before anything reads it**: its
  checksum, its address, the filesystem's metadata UUID, the level and
  transaction its parent (or the superblock, or a root item) expects,
  its item count within the block, its items' data packed from the
  block's end without gaps or overlaps, its keys in order, its children's
  addresses aligned, and its keys within its parent's bounds: its first
  key the parent's key pointer, its last before the parent's next. A
  block from another transaction is a write the disk lost. A block that
  fails is read from the next copy; when none passes, the read fails with
  `Corrupt`.
- **Checked blocks are cached**, up to 8 MiB of them (512 blocks of the
  default 16 KiB), by address, so that walking a tree again neither reads
  nor checks a block twice. A clock picks which
  to drop, never one in use, and the cache gives its blocks up under
  memory pressure.
- **Trees are searched** with a cursor holding a block at each level:
  seeking finds the first item at a key or after it, and advancing
  moves through the leaves in order.

The tests make filesystems with `mkfs.btrfs` on the host (btrfs-progs,
which CI installs and `ob doctor` checks for) and read them back: each
checksum with nodes of 4, 16 and 64 KiB; trees of three levels, on disks
of 512-byte and 4 KiB sectors; the single, DUP and mixed profiles; a
damaged first copy read from the second; and superblocks with bad
fields or unknown features. A fuzz test damages tree blocks and then
puts their checksums right, so only the block checks stand between the
damage and the code that reads it: every read ends in an answer or an
error, never a panic.

## Consequences

- Linux-made filesystems on one disk read in Radix, whatever their
  checksum, node size and single-disk profile.
- A damaged or hostile disk can make a read fail, but cannot make the
  kernel read past a block or loop: every bound is checked first, and a
  cursor only goes down a tree, level by level.
- No outside code is in the crate, so nothing in it needs a license
  check, and its checksums are as fast as its own code makes them.
- Files, directories, compression and mounting come in 10b.2; the log
  replay in 10c; more than one device in 10e.
