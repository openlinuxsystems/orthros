<!--
SPDX-FileCopyrightText: 2026 The Dukovo contributors
SPDX-License-Identifier: Apache-2.0 OR MIT
-->

# 0134. Btrfs compresses writes with zstd level 1 on the disks that ask for it

- Status: Accepted
- Date: 2026-10-28

## Context

Btrfs reads compressed extents since Phase 10b.3 (ADR 0128), and since
10c.1b writes files (ADR 0130), but only as they are. Phase 10c asks for
zstd compression on write, with an `ob bench` row for it. Linux
compresses when a mount says `compress=zstd:N` (or `compress-force`), or
when a file has the compression flag (`chattr +c`) or property; it
compresses each range of up to 128 KiB of a file's dirty pages into one
extent, and writes the range as it is when that would not save a sector.
Distributions differ: Fedora mounts its root with `compress=zstd:1`, and
leaves other disks to their owners.

The compartment has no `unsafe` and allocates nothing under its locks
it cannot fail; Linux's zstd is GPL-2.0 in the kernel (BSD-3-Clause
upstream), and Dukovo's decoder is its own (ADR 0128).

## Decision

- **An encoder of its own** (`kernel/fs/btrfs/src/zstd/encode.rs`),
  written from RFC 8878 and modelled on the "fast" strategy of zstd's
  level 1: a table of 2^14 positions hashed on six bytes, matches of four
  bytes or more, the last offset tried one byte on before the table,
  matches extended backwards, the offset before last tried at once after
  each match, and steps that grow by one every 256 bytes without a
  match. Each extent is one frame of one block, with its content size
  and no checksum (Btrfs checksums the sectors). Literals are raw, one
  repeated byte, or Huffman coded with weights written directly when no
  byte is above 127, in one stream below 256 literals and four above;
  sequences use the predefined tables, a single code, or tables fitted
  to their counts when 64 or more are coded, whichever the reference
  picks. Every buffer is allocated with the encoder; nothing allocates
  per call. On text in 128 KiB frames its output is 0.3% larger than the
  reference `zstd -1`, and it runs as fast (the reference's table has
  2^13 positions for frames this small; one of 2^14 finds more matches
  for the same time and makes up for the rest of the encoder).
- **Compressed extents**: write-back compresses a run of a file's dirty
  pages in chunks of up to 128 KiB, only the bytes before the end of the
  file, and writes a chunk compressed when the frame saves at least one
  of the sectors those bytes fill, as Linux has it. The extent's
  `EXTENT_DATA` gives the chunk's pages as its length and the frame's
  sectors as its size on the disk, with compression type 3; checksums
  are of the sectors written. The frames of a run go to the disk one
  after the other, in one request, and a request now holds up to
  128 KiB (it held 64 KiB). Two packers, an encoder and the frame's
  pages each, are made at a filesystem's first compressed write and
  dropped by its shrinker; a run compresses two chunks at a time, the
  second on another CPU when one is free (`radix_vfs::join`, ADR 0133),
  before it takes the transaction lock, so reads go on meanwhile; the
  page cache hands write-back runs of up to 64 pages (it handed 32), so
  that a run is two chunks. The superblock gains `COMPRESS_ZSTD` at the
  commit of the first compressed extent.
- **Which files**: a mount compresses when it is made with the new
  `compress` flag of `directory.mount` (refused with `invalid_argument`
  on a filesystem other than Btrfs). A file with Btrfs's `COMPRESS` flag
  is compressed on any mount, one with `NOCOMPRESS` or `NODATASUM` never,
  as on Linux. Small files written inline stay as they are.
- **Like Fedora: the root only** (Chase, 2026-10-28): Dukovo's own root
  compresses once it is Btrfs (Phase 10c.3); another disk compresses only
  when Spark's command line names it (`--compress <disk>`, spark(8)).
- **The bench row** `posix_btrfs_write_zstd_mib`: 32 files of 1 MiB of
  text written in writes of 128 KiB and synced (`syncfs` on Linux, an
  `fsync` of their directory on Radix, which syncs its filesystem), per
  file, against Linux's Btrfs mounted `compress=zstd:1`. Both sides
  mount the bench's Btrfs disk compressed.

Rejected:

- **Porting Linux's or the reference zstd**: the reference library is
  BSD-3-Clause and would fit, but it is C, with `unsafe` throughout, in a
  compartment that has none; the decoder is already the crate's own.
- **Compressing on every Btrfs disk by default**: a disk shared with
  another system, or one holding media that does not compress, would pay
  the CPU for nothing; Chase chose the root only.
- **Marking a file `NOCOMPRESS` when its first chunk does not shrink**,
  as Linux does without `compress-force`: every chunk is tried instead,
  so a file whose start is already compressed and whose rest is text
  still compresses. A chunk that does not shrink costs one encoder pass.

## Consequences

- Text and other data that compresses takes about a third of its room on
  the root, and reads of it are as fast as Linux's (ADRs 0132 and 0133).
- Literals with bytes above 127 are written raw, so binary data
  compresses less well than on Linux, where their weights are FSE coded;
  coding them so is a later change of the encoder alone.
- Small files are not compressed inline, where Linux compresses them.
- `COMPRESS_ZSTD` is set at the first compressed commit rather than at
  the mount, so a compressed mount that writes nothing leaves the disk's
  features as they were; older kernels that lack zstd still refuse the
  disk once it holds a compressed extent, as with Linux's.
- A packer holds about 1 MiB, its encoder's buffers and 64 pages, so a
  filesystem that compresses keeps about 2 MiB until memory runs short.
