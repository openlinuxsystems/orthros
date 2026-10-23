<!--
SPDX-FileCopyrightText: 2026 The Dukovo contributors
SPDX-License-Identifier: Apache-2.0 OR MIT
-->

# 0049. The programs a machine starts with come in a ustar boot archive that GRUB loads as a module, checked whole before use

- Status: Accepted
- Date: 2026-10-21

## Context

Spark must start before any file system or disk driver exists: the full
VFS and the first block devices come in Phase 7, and spec 16.2 numbers
Spark's start before the step that mounts file systems. The kernel needs
the programs from somewhere the boot loader can already reach, in a
format that tools on any build host can write and inspect, and that is
simple enough to check completely, since it is data from outside the
kernel. Zircon's bootfs and Linux's initramfs (a cpio archive) solve
the same problem.

## Decision

- `ob build` writes the **boot archive**, `/boot/bootfs` in the boot
  image, as a POSIX **ustar** file (POSIX.1-1988's interchange format),
  with `/system/bin/spark` and the programs Spark starts, under their
  directories, owned by root, mode 0755 and dated from the commit
  (`SOURCE_DATE_EPOCH`), so that the same programs make the same bytes.
  GRUB loads it as a module whose command line has the word `bootfs`.
- `radix-bootfs` reads it, in the kernel and in user space alike. `open`
  checks the whole archive in one pass before anything uses it: every
  header's checksum, every number, every size inside the file, every
  path relative, at most 255 bytes and free of `.` and `..`, the paths
  in order with each directory before what is in it, and at most 4,096
  entries. Only regular files and directories are allowed. After that,
  looking up a file or listing a directory cannot fail. The reader
  forbids `unsafe` code and its property tests feed it garbage.
- The kernel maps the archive read-only into Spark (ADR 0050) and
  radix-rt serves it as a minimal read-only file system (`BootFs`:
  `files`, `read`, `list`, ADR 0051). It is the "minimal read-only VFS"
  of Phase 6; Phase 7's VFS mounts it under `/system` beside the real
  file systems.
- ustar over cpio: `tar` is on every build host and in every
  `libarchive`, ustar's fixed 512-byte headers with octal fields are
  easy to check strictly, and its file data starts on a block boundary,
  ready to be mapped in place later.

## Consequences

- Adding a program to the image is a line in `ARCHIVE` in
  `tools/xtask/src/userspace.rs`; `tar tvf out/sysroot/boot/bootfs`
  shows what is in it.
- Long names are limited to ustar's 255 bytes (prefix and name), and
  there are no owners or extended attributes, which the programs of the
  boot archive do not need. Symbolic links came in Phase 9 (ADR 0081).
- The kernel copies the archive into frames of Spark's own once; a
  later phase can map the module's pages directly instead.
