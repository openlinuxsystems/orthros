<!--
SPDX-FileCopyrightText: 2026 The Dukovo contributors
SPDX-License-Identifier: Apache-2.0 OR MIT
-->

# 0128. Btrfs's compressed extents are read by decoders of its own, written from the formats' specifications

- Status: Accepted
- Date: 2026-10-27

## Context

ADR 0127 reads Btrfs files and fails a compressed extent with
`NotSupported`. Linux compresses extents with zlib, LZO or zstd when a
filesystem is mounted with `compress=`, when a file or directory has the
`compression` property or `chattr +c`, and when `mkfs.btrfs --compress`
makes it. Fedora, which installs on Btrfs, mounts it with
`compress=zstd:1`, so most of such a system's files are compressed.

- A compressed extent holds up to 128 KiB of a file's data in up to
  128 KiB of the disk. The checksums of its sectors are of the
  compressed bytes. An inline extent may be compressed too.
- zlib is RFC 1950 around DEFLATE (RFC 1951). zstd is one Zstandard
  frame (RFC 8878), without a dictionary. LZO is Btrfs's own layout of
  segments, each one LZO1X stream of at most a sector of data.
- The kernel is built with `no_global_oom_handling`: every allocation
  can fail and is handled, so code that allocates through Rust's
  infallible collections does not build in it. Its kernel stacks are
  small, and every function's frame must stay within 2 KiB (ADR 0058).
- The Rust crates for these formats do not fit as they are. `ruzstd`
  allocates through `Vec` and so does not build for the kernel;
  `miniz_oxide` could inflate without allocating, but would bring a
  vendored crate for what one specification describes in full. LZO1X's
  reference implementation, Markus Oberhumer's LZO, is GPL-2.0, as is
  Linux's, and the Rust crates ported from either are too; the kernel is
  Apache-2.0 OR MIT and takes no GPL code (ADR 0087). The MIT-licensed
  lzokay and its Rust port exist, but a decoder is some 200 lines that
  the format's description gives.

## Decision

- **Three decoders of the crate's own**, each from the format's
  published description and nothing else: `inflate` from RFC 1950 and
  RFC 1951; `zstd` from RFC 8878, with XXH64 from the xxHash
  specification; `lzo` from the description of LZO1X in Linux's
  `Documentation/staging/lzo.rst` and from the segments Linux and
  `mkfs.btrfs` write. They are safe Rust, as the whole crate is
  (`forbid(unsafe_code)`); every table is allocated once, fallibly, and
  every read and copy is bounds-checked, so data that does not decode
  fails with `Io` and never panics.
- **They behave as Linux's do** on the same data. A zlib stream's header
  must be valid and name no preset dictionary; its Adler-32, which Linux
  does not check, is not read. A zstd frame's checksum and content size
  are checked when it has them. An LZO stream must end where its end
  marker is. Data that ends before the extent's size reads as zeros
  after its end, and decoding stops when the extent's size is reached,
  as on Linux.
- **An extent is read whole.** Each of its sectors is read and checked
  against its checksum, from the next copy when one does not match, as
  for any data (ADR 0127), and then the extent is uncompressed into a
  buffer and the page asked for is copied out. A damaged sector is thus
  found by its checksum before the decoder sees it; a sector that passes
  and still does not decode fails the read with `Io`.
- **The extent's other pages go to the page cache.** The VFS's backing
  gains `read_pages`, which reads a page and offers the file's other
  pages it read on the way; the page cache keeps each one it lacks. A
  compressed extent read from its start is uncompressed once, not once
  per page, as Linux adds a compressed extent's other pages to its page
  cache.
- **One workspace per filesystem**: the buffers for an extent's
  compressed bytes and its data, and each decoder's tables, about 400 KiB
  in all, boxed, made at the filesystem's first compressed read and
  dropped by its shrinker when memory runs short, unless a read holds it.
  Compressed reads of one filesystem take the workspace in turn.
- **Limits**: an extent that claims more than 128 KiB of data or of the
  disk, or whose place on the disk is not whole sectors, is corrupt and
  fails with `Corrupt`.
- **Tests** decode what the formats' own tools make. zstd frames made by
  the `zstd` program from generated data, at levels from `--fast=4` to
  19 and windows from 1 KiB, with and without checksums, are kept in the
  tree, with an ignored test that makes them again; together with frames
  written by hand they reach raw and RLE blocks, every kind of literals
  section and every mode of the sequence tables. zlib streams come from
  Python's `zlib` at several levels, windows and strategies, with full
  flushes between pieces, LZO instructions are
  written by hand from their description, and every test checks the
  bytes. Btrfs filesystems made by `mkfs.btrfs --compress` with zlib,
  LZO and zstd at several levels are read through the VFS byte for byte,
  with damaged copies read from the other copy or failing with `Io`, and
  2,500 random damages to compressed extents fail or read, never panic.
  The `btrfs` boot test's two disks are compressed, one with zstd and one
  with zlib.

## Consequences

- Files Linux compressed on Btrfs read in Dukovo as Linux reads them.
- About 1,700 lines of decoders are the project's to keep, with their
  tests; any fix or speed-up is ours to make.
- No `ob bench` row measures compressed reads yet. Linux's zstd and zlib
  are tuned, so these decoders are measured against them, and made at
  least as fast, when Phase 10c writes compressed extents and adds the
  rows for compressed reads and writes.
- Compressed reads of one filesystem are serialized on its one
  workspace; Linux keeps a workspace per CPU at most. A pool comes when a
  benchmark shows the need.
