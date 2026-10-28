<!--
SPDX-FileCopyrightText: 2026 The Dukovo contributors
SPDX-License-Identifier: Apache-2.0 OR MIT
-->

# 0140. exFAT is our own implementation from Microsoft's specification, and ships though its patents run to 2027

- Status: Accepted
- Date: 2026-10-28

## Context

exFAT is what SDXC cards, most USB sticks larger than 32 GB and many
external disks come formatted with, and what cameras and phones write.
Without it Dukovo cannot read the card out of a camera. FAT (ADR 0056)
stops at files of 4 GiB, which is why exFAT replaced it on such media.

Linux's `exfat` driver and exfatprogs are GPL and are not read (the
rule for every GPL source in this project). Microsoft published the
"exFAT File System Specification" in 2019, so the format can be built
from the specification alone.

exFAT is patented. In 2019 Microsoft said it supports exFAT in the Linux
kernel, and put its exFAT patents under the Open Invention Network's
licence. That licence covers the "Linux System" as OIN defines it, a
list of Linux packages. Radix is not Linux and is not on that list, so
the grant does not cover Dukovo. The last of the exFAT patents found,
US 8,321,439 ("Quick File Name Lookup Using Name Hash"), runs to
2027-03-09 (Google Patents). The Phase 10 preparation suggested holding
exFAT until a legal answer or the patent's end. Chase decided on
2026-10-25 to build it from the specification and ship it in Phase 10
like everything else, accepting the patent risk until 2027.

## Decision

- `radix-exfat` implements exFAT 1.00, read and write, from Microsoft's
  specification. It is a compartment, `#![forbid(unsafe_code)]`, and
  uses `radix-fat`'s cluster chains, FAT times and name buffers. Like
  FAT, its metadata goes through the device's block cache and file data
  through each file's page cache, straight to and from its clusters
  (ADR 0055). Its locks follow FAT's order: the namespace, a file's
  pages, one node, the clusters, the block cache.
- Every number from the disk is checked before use. The boot region's
  checksum is checked; a volume of a later major revision is refused
  (`NotSupported`); one whose numbers do not add up is `Corrupt`; one
  with sectors smaller than the disk's is refused, as Linux does. A
  Kani proof checks that any boot sector that mounts keeps every
  cluster on the device.
- Names compare case-insensitively through the volume's own up-case
  table, which is loaded and checked against its checksum, as the
  specification says. Lookups use the name hash. Names of more than 255
  UTF-16 units, with the characters exFAT forbids, or ending in a dot
  or space are refused, as in FAT.
- The volume is marked dirty, with a write straight to the disk, before
  the first change after a mount or sync, and clean again only once a
  sync has written everything. A dirty volume mounts; its marks stay
  until the next sync.
- New files and directories are contiguous (`NoFatChain`), grown into
  the clusters after their last when they are free, and given a chain
  in the table only when they cannot be. Freed clusters are cleared in
  the bitmap only.
- Growing a file does not write zeros: its valid data length stays
  where the data ends, and bytes past it read as zeros. A write past it
  zeroes the disk between the two first, then moves it.
- A volume with two tables (TexFAT), or whose medium reported a
  failure, is read but not written: a writable mount fails with
  `NotSupported`, and Spark mounts it read-only, as with a Btrfs that
  Radix cannot write (ADR 0130). Spark tries FAT, then exFAT, then Btrfs
  on each disk. The mount ABI's FilesystemKind gains `exfat` (8).
- The tests use other implementations as the reference. Host tests
  format with exfatprogs' `mkfs.exfat` and check every volume Radix
  wrote with its `fsck.exfat`. The Sleuth Kit (`fls`, `icat`), an exFAT
  reader of its own, reads the files back. Radix reads and then changes
  a volume exfat-fuse wrote, kept in the repository as its non-zero
  sectors. A model test runs random changes and remounts, and damaged
  volumes must fail with errors, never a panic. The `exfat-write` boot
  test runs the storage program on two exFAT disks, checked by
  `fsck.exfat` after each boot and by The Sleuth Kit between them.
  Volumes of 4 KiB sectors are made by the tests from the
  specification, as `mkfs.exfat` makes them only on such disks.

## Consequences

- Dukovo reads and writes cards, sticks and disks formatted with exFAT,
  with files larger than 4 GiB, and its volumes pass Linux's checker.
- Dukovo carries the patent risk Chase accepted until 2027-03-09. If a
  legal answer comes before then, a later ADR records it.
- Benign secondary entries that Radix does not know (vendor extensions)
  are kept while a file stays where it is, but not carried over when it
  is renamed, and the clusters of a vendor allocation entry are not
  freed when its file is removed: they stay marked in the bitmap. Few
  volumes have them.
- No hard links, symbolic links or owners, as exFAT has none, as on
  FAT.
- No exFAT bench rows, as FAT has none: the disk rows measure the
  block layer, and Btrfs is the system's filesystem.
