<!--
SPDX-FileCopyrightText: 2026 The Dukovo contributors
SPDX-License-Identifier: Apache-2.0 OR MIT
-->

# 0301. A partition table read again keeps the partitions in use

- Status: Accepted
- Date: 2026-11-02

## Context

Phase 14a. Radix read a disk's partition table once, when it found the
disk, and registered a device for each partition. A partitioner that
writes a table needs the kernel to read it again, as Linux's BLKRRPART
asks, or the new partitions appear only at the next boot.

- Linux's BLKRRPART fails with EBUSY if any partition of the disk is
  open; fdisk then falls back to BLKPG, which adds and removes one
  partition at a time, and says the kernel "still uses the old table"
  for those in use. A partition added to the disk the system runs from
  needs that.
- A partition device in Radix is a slice of its disk: its offset and
  size. A file that has an old partition open after the table changed
  would read and write where another partition may be now.

## Decision

- **REREAD_PARTITIONS** (BLKRRPART's number, 0x125f) on a disk reads
  its table again (radix-block(2)). It needs the write right, and on a
  partition fails with `invalid_argument`, as Linux's does.
  - A partition the table has where it was, of the size it was, stays as
    it is: open, mounted, swapped to. Its type follows the table.
  - One the table no longer has leaves `/devices` and `/kernel/block`,
    and is **retired**: every request on it fails with `io` from then
    on, as on a disk unplugged (`BlockDevice::retire`, which only a
    partition's slice does anything for).
  - One new to the table comes, and the log says so as at boot.
  - If a partition that would go or change is in use, or the disk is,
    nothing changes and the request fails with `in_use`. Re-reads are
    one at a time.
- **GET_IN_USE** (0x80045205, Dukovo's own) says what uses a disk or
  partition: a mount from it, its swap area, a Btrfs mounted from
  another of its disks, a BitLocker volume of it unlocked. rdisk(8) and
  the mkfs tools ask it before they write, and say who uses the device.
- **volumed(8)** leaves unmounted a partition that comes on a disk that
  was there already: it was made by partitioning, not plugged in, and
  its maker will put a filesystem on it.

## Consequences

- rdisk adds a partition in the free space of the disk the system runs
  from, with its other partitions mounted, and the partition appears at
  once.
- A partition taken away while a program had it open makes that
  program's reads and writes fail, rather than land on its neighbour.
- Two new block requests change the C headers the ports are built
  against, so every port is built again once.
