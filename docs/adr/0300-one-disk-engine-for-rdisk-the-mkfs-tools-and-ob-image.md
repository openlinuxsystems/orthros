<!--
SPDX-FileCopyrightText: 2026 The Dukovo contributors
SPDX-License-Identifier: Apache-2.0 OR MIT
-->

# 0300. One disk engine for rdisk, the mkfs tools and ./ob image

- Status: Accepted
- Date: 2026-11-02

## Context

Phase 14a, its first part. The installer partitions a disk and makes its
filesystems, and so do people at the live system's console; `./ob
image` makes the images the installer and the cloud are given. Until
now the images came from Linux's tools (sgdisk, mkfs.fat) on the build
machine, and Dukovo had none of its own: a disk could be read and
mounted, not partitioned.

- Linux's partitioners are util-linux's fdisk and sfdisk (GPL-2.0 and
  later; LGPL-2.1 libfdisk) and gdisk's sgdisk (GPL-2.0); dosfstools
  (GPL-3.0) and exfatprogs (GPL-2.0) make and check FAT and exFAT.
  Porting them brings their licences into the base system, and fdisk's
  libfdisk is 40,000 lines of C.
- GPT is in the UEFI specification, MBR is the IBM PC's, and FAT and
  exFAT are in Microsoft's published specifications ("FAT: General
  Overview of On-Disk Format" 1.03, the exFAT File System
  Specification), which anyone may implement.
- The installer and `./ob image` should make the same tables, so that
  the images are what an install makes.

## Decision

- **One crate, `userspace/programs/disk`** (safe Rust, the workspace's
  licence), reads and writes partition tables and makes filesystems, on
  `std` alone: serde and serde_json read layout files (both MIT OR
  Apache-2.0, already in systools). It builds for Dukovo, in systools,
  and for Linux, where `./ob image` will use it (Phase 14a, part 2).
  - **GPT**: both copies read and checked, the good one used when the
    other is damaged, and mended when written; the backup moved to the
    end of a disk that grew; the backup written first, so that a write
    cut short leaves one whole copy. 128 entries, as every tool makes.
  - **MBR**: four primary partitions, and an extended partition's
    logical ones read and kept as they are, but not made: a GPT has room
    for 128 partitions of any size.
  - New partitions start on a whole MiB, as fdisk's and the Windows
    installer's do. An MBR over an old GPT clears its headers, and a table
    over a filesystem on the whole disk clears its marks, as fdisk does,
    so that nothing reads the disk as what it was.
  - **Layout files** (rdisk-layout(5)), a table in JSON, which the
    installer and `./ob image` apply, and `rdisk --dump` writes.
  - **FAT12, FAT16, FAT32 and exFAT** made from Microsoft's
    specifications (ADR 0302).
- **rdisk(8)** is the partitioner, in systools: fdisk's letters at a
  prompt, `--script` for layout files, `--dry-run` to see what a write
  writes, `-l` and `--dump`. A disk with no table starts with a GPT. A
  disk in use, or a partition in use that the table changes, is refused
  before anything is written; once it is, the kernel reads the table
  again (ADR 0301).
- **Tested** by the crate's host tests, which check every table and
  volume it makes with the Linux tools that read them: sgdisk and
  sfdisk read its GPTs and MBRs, it reads theirs, and dosfstools,
  mtools and exfatprogs check its volumes. Those tools run on the host
  only, as they did for the boot tests. Windows' `chkdsk` checks its FAT
  and exFAT volumes in CI's Windows job, as it checks NTFS (ADR 0238).
  The `rdisk` boot test partitions a blank disk at the console.

## Consequences

- Dukovo partitions and formats disks with nothing from Linux, and the
  installer can be written (Phase 14a, part 3).
- Logical partitions are not made; a disk that has them keeps them.
- fdisk's expert menu, sfdisk's script format and GPT's hybrid MBRs are
  not there; a hybrid MBR read is written back as a plain protective
  one, with a note.
- The host tests need gdisk, fdisk, dosfstools, exfatprogs and mtools
  on the build machine; `./ob doctor` lists them.
