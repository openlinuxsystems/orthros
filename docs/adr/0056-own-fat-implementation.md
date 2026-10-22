<!--
SPDX-FileCopyrightText: 2026 The Dukovo contributors
SPDX-License-Identifier: Apache-2.0 OR MIT
-->

# 0056. FAT is our own implementation from Microsoft's specification, checked against dosfstools and mtools

- Status: Accepted
- Date: 2026-10-22

## Context

FAT is the filesystem every computer reads: USB sticks, SD cards and
the EFI system partition use it, so Radix needs it from Phase 7, before
its native Btrfs (ADR 0004). Existing implementations are GPL (Linux's
`vfat`, dosfstools), in C with buffering and locking of their own
(ChaN's FatFs, under a permissive BSD-style license), or Rust crates
built around their own storage and buffering traits for single-threaded
use (`fatfs`, MIT), which could not share the kernel's page cache and
block cache or its locking rules. The plan keeps the kernel and
`/system` free of copyleft code, and the kernel in Rust.

## Decision

- `radix-fat` implements FAT12, FAT16 and FAT32, read and write, with
  VFAT long names, from Microsoft's published "FAT: General Overview of
  On-Disk Format", version 1.03. Times use Howard Hinnant's civil-date
  algorithms. It is a compartment, with `#![forbid(unsafe_code)]`.
- Metadata goes through the device's block cache, file data through each
  file's page cache straight to and from its clusters (ADR 0055). A
  directory's clusters are discarded from the block cache when they are
  freed, so that stale entries are never written over file data later
  put there; a TLA+ model (`kernel/fs/fat/tla/Clusters.tla`) checks that
  nothing is ever written to a cluster but by its owner of the moment,
  and fails without the discard.
- The reference tools check it: the host tests format volumes with
  `mkfs.fat`, check what Radix wrote with `fsck.fat` and read it back with
  mtools; the `storage` boot test writes four disks from a Radix program,
  checks them with `fsck.fat` and mtools, changes one from the host and
  reads everything back after a reboot. dosfstools and mtools are GPL
  host tools, run as separate programs, and never part of Dukovo.

## Consequences

- No copyleft code in the kernel, and FAT shares the page cache, the
  block cache and the locking rules of every other filesystem.
- We own a FAT implementation, and its bugs. The reference tools on
  every push, the model, and host tests against a model of the volume
  keep that cost down.
- exFAT is not part of Phase 7. Microsoft published its specification
  in 2019, and the roadmap adds it read-write in Phase 10, built the same
  way.
