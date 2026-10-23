<!--
SPDX-FileCopyrightText: 2026 The Dukovo contributors
SPDX-License-Identifier: Apache-2.0 OR MIT
-->

# 0088. libarchive is tar, cpio and unzip; each compressor is its own project's

- Status: Accepted
- Date: 2026-10-23

## Context

A server's shell moves files around in archives: release tarballs
compressed with gzip, xz or zstd, initramfs images in cpio, zip files
from elsewhere, and backups piped through a compressor. The plan's
Phase 9 command set names libarchive, gzip, bzip2, xz, zstd, lz4, 7-Zip
and zip for this, and `file` and `jq` beside them, which scripts use to
look at what they were handed.

Each format has a tool of its own on other systems, often several: GNU
tar and cpio (GPL), Info-ZIP's UnZip, and libarchive's bsdtar, bsdcpio
and bsdunzip (BSD-2-Clause), which FreeBSD and macOS ship as their tar.

## Decision

- `tar`, `cpio` and `unzip` are libarchive's bsdtar, bsdcpio and
  bsdunzip, with links under their usual names; `bsdcat` keeps its own,
  as `cat` is the core utilities'. One library reads every format, so
  tar also reads and writes zip, 7z, ISO 9660 and the others, and reads
  RAR with libarchive's own decoder.
- Each compressor is its own project's program and library: GNU gzip
  (a program of its own under the GPL, ADR 0087), bzip2, XZ Utils, Zstandard
  and LZ4. Their static libraries stay in the sysroot for the ports built
  on them: libarchive links all five, zstd reads .gz, .xz and .lz4 with
  theirs, and file and Zip use some.
- `zip` is Info-ZIP's Zip 3.0, the last release, which every
  distribution still ships, with the eight fixes Debian carries, each
  with its author's header: three buffer overflows, a command injection
  through the archive name in `-T`, `zipnote` closing the wrong file,
  symbolic link updates, its manual in section 1, and no build date.
  `zip -T` runs Info-ZIP's `unzip` and checks its version, so with
  libarchive's it takes `-TT "unzip -q -t"`.
- `file` comes with its magic database, compiled by a `file` built for
  the build machine, since the database is compiled by file itself.
- `jq` is built with the Oniguruma its release carries, for its regular
  expressions, and decNumber, so it keeps large numbers exact.
- The Port area of the license manifest allows the permissive licenses
  these use: 0BSD (XZ Utils), bzip2-1.0.6, Info-ZIP, and ICU, dtoa and
  CC-BY-3.0 (jq, its decNumber, its number formatting and its manual).
  Info-ZIP's text is in `LICENSES/`, as the zip patches are under it;
  the others are in `image/licenses/`.
- 7-Zip waits for the next part of Phase 9, which ports a C++ runtime.

## Consequences

- Scripts written for GNU tar mostly work: bsdtar takes the common
  options (`-czf`, `-xJf`, `--zstd`, `-C`, `--exclude`, `-a` by suffix),
  but not all of GNU's: renaming with `--transform` is `-s` here, and
  GNU's incremental archives (`--listed-incremental`) are missing.
- Without a cryptography library yet, libarchive reads and writes only
  ZipCrypto-encrypted zip archives, not AES ones, and mtree files
  without checksums; without an XML library, no xar archives. Radix has
  no ACLs or extended attributes, so none are stored or restored.
- The boot archive grows from 46 MiB to 71 MiB, about 10 MiB of it the
  compiled magic database and 8 MiB the four libarchive programs, each
  linked with the whole library. The kernel kept the archive's frames
  among Spark's 32 kept capabilities, two for each 4 MiB, which held at
  most 56 MiB; it now keeps them in its own capability space for Spark,
  sized for the archive.
- The ports found three things for the C library to fix. mlibc's
  `<math.h>` now declares `drem`, `scalb` and `significand`, which its
  libm already had and jq calls, as glibc and musl do. Its getopt lost
  the first option of a cluster that followed another (`xz -qQ -cdf`,
  as xzdiff runs it, never saw `-c`), and refused an argument that
  began with `-`; both are fixed as POSIX has them. `O_PATH`, which
  libarchive opens directories with, counted as an access mode, so
  opening a directory with it failed with EISDIR; it now opens for
  reading. And libarchive's tree walk climbs with `openat(fd, "..")`,
  which ADR 0089 makes work.
