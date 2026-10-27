<!--
SPDX-FileCopyrightText: 2026 The Dukovo contributors
SPDX-License-Identifier: Apache-2.0 OR MIT
-->

# 0123. Disks of 4 KiB sectors keep 512-byte addresses and move whole sectors

- Status: Accepted
- Date: 2026-10-27

## Context

More and more disks have logical sectors of 4 KiB ("4Kn"): NVMe
namespaces formatted that way, large SATA and SAS drives, and the
virtual disks of some clouds. A disk of 4 KiB sectors reads and writes
nothing smaller, and counts its addresses in them. Until now every layer
of Dukovo's storage took sectors of 512 bytes: the drivers refused a
disk with other sectors, and the block layer, its cache, the partition
tables, raw devices and FAT all counted in 512 bytes.

Linux reads and writes such disks. Its addresses stay in sectors of 512
bytes (`sector_t`) at every layer, and each request must start and end
on the disk's logical block. Partition tables count in the disk's
logical blocks, as their specifications say, while `/sys` gives a
partition's start and size in 512-byte sectors. `BLKSSZGET` and
`queue/logical_block_size` give the logical block, and a discard must
cover whole ones. A FAT volume mounts only when its sectors are at least
the disk's.

## Decision

Dukovo does as Linux does: addresses stay in sectors of 512 bytes, and
what moves is whole logical blocks.

- The block layer's description of a disk names its logical block, 512
  to 4096 bytes (`radix_block::Info::logical_block`). Every request to a
  volume must start and end on one, which the volume checks with the
  rest of a request; nothing above the block layer changes its units.
- The NVMe driver takes namespaces of 512 to 4096-byte blocks and turns
  sectors into blocks as it builds each command; a namespace's preferred
  write granularity and atomic write size count in its blocks. The AHCI
  driver reads a drive's logical sector size from IDENTIFY DEVICE (words
  106, 117 and 118), takes 512 or 4096 bytes, and turns sectors into the
  drive's own. The virtio driver takes a disk's block size
  (`VIRTIO_BLK_F_BLK_SIZE`) as its logical block and, with
  `VIRTIO_BLK_F_TOPOLOGY`, its physical block from the exponent; virtio's
  requests count in 512-byte sectors whatever the disk's are.
- The block cache fetches and writes back whole logical blocks, and a
  read of one sector reads the block that holds it.
- MBR and GPT partition tables are read in logical blocks (a GPT's
  header is in block 1, wherever that is), and each partition's start
  and size are kept in 512-byte sectors.
- A raw device's read or write of part of a block reads the whole block
  and, for a write, writes it back; a transfer goes straight between the
  disk and a program's memory only when it covers whole blocks. Discards
  need whole blocks, and `BLKSSZGET`, `queue/logical_block_size` and
  `queue/discard_granularity` give the logical block.
- FAT, like Linux, refuses with `EINVAL` to mount a volume whose
  sectors are smaller than the disk's logical block.

## Consequences

- A disk of 4 KiB sectors shows the same sizes, partition starts and
  answers to Linux's block requests as on Linux, so tools and scripts
  written for Linux need no changes.
- Above the block layer nothing counts differently, so Btrfs (Phase
  10b), whose blocks are 4 KiB or larger, needs nothing more.
- Disks of 512-byte sectors and 4 KiB physical blocks ("512e"), the
  common kind, work as before.
- Logical blocks above 4 KiB, which few disks have and Linux took only
  lately, are refused: the driver names the size when it skips the disk.
- Unit tests check every layer on RAM disks of 4 KiB blocks, and the
  `storage-4k` and `disk-raw-4k` boot tests run the storage tests on
  QEMU's virtio and NVMe disks of 4 KiB sectors. QEMU's SATA disks have
  only 512-byte sectors, and neither reference machine has a disk of 4 KiB
  sectors, so the AHCI driver's handling of them is checked by its unit
  tests alone.
