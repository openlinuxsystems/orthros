<!--
SPDX-FileCopyrightText: 2026 The Dukovo contributors
SPDX-License-Identifier: Apache-2.0 OR MIT
-->

# 0302. FAT and exFAT made and checked by Dukovo's own tools

- Status: Accepted
- Date: 2026-11-02

## Context

Phase 14a. An installed Dukovo boots from an EFI system partition, which
is FAT32; USB sticks and SD cards are FAT32 or exFAT, which every system
reads. The kernel reads and writes both (ADRs 0056 and 0140), but making
them took dosfstools' mkfs.fat (GPL-3.0) and exfatprogs' mkfs.exfat
(GPL-2.0) on Linux. A FAT left dirty by a power cut needs checking, as
fsck.fat does.

## Decision

- **The disk engine makes them** (ADR 0300), from the Microsoft
  specifications Radix reads them by, "FAT: General Overview of On-Disk
  Format" 1.03 and the exFAT File System Specification:
  - FAT12 up to 4.1 MiB, FAT16 under 512 MiB and FAT32 above, unless
    asked; the cluster sizes of the specification's tables, which
    Windows' `format` uses too; the data area on a whole cluster; FAT32's
    FSInfo and backup boot sector; the hidden sectors of a partition; a
    boot sector that says, if booted, that it is not a boot disk.
  - exFAT with Windows' cluster sizes (4 KiB to 256 MiB, 32 KiB to
    32 GiB, 128 KiB above), its table and heap on whole MiB from 64 MiB,
    the main and backup boot regions with their checksums, and the
    up-case table from Unicode's simple upper-case mappings, as Rust's
    `char` has them, compressed as the specification allows.
- **mkfs-fat(8)** and **mkfs-exfat(8)** take dosfstools' and
  exfatprogs' options and answer to their names (`mkfs.fat`, `mkfs.vfat`,
  `mkfs.exfat`); **mkfs(8)** runs the one `-t` names, as util-linux's
  does. They refuse a device in use, and a whole disk with partitions
  without `-I`. `--invariant` and `SOURCE_DATE_EPOCH` make the same
  volume each time, for `./ob image`.
- **fsck-fat(8)** (`fsck.fat`, `fsck.vfat`) checks a FAT volume as
  fsck.fat does: the tables against each other, every directory and long
  name, every chain against its file's size, clusters shared or lost, the
  FSInfo, and the flags a mounted volume sets. `-a` mends what it finds.
  Its exit status is fsck(8)'s (0, 1, 4, 8, 16), as e2fsck gives and
  systemd-fsck reads, where dosfstools gives 1 for any problem.
- **Tested** on the host: what mkfs-fat makes, fsck.fat finds clean and
  mtools reads and writes; what mkfs-exfat makes, fsck.exfat finds clean;
  what fsck.fat finds wrong in a volume spoiled on purpose, fsck-fat finds
  and mends, and fsck.fat then finds clean. Windows' `chkdsk` checks a
  FAT32, FAT16, FAT12 and exFAT volume in CI's Windows job.

## Consequences

- Dukovo makes its own EFI system partitions and sticks.
- fsck-fat frees lost chains rather than saving them as files, and asks
  nothing: `-r` mends as `-a` does.
- An exFAT volume is not checked yet; fsck.exfat is not there.
