<!--
SPDX-FileCopyrightText: 2026 The Dukovo contributors
SPDX-License-Identifier: Apache-2.0 OR MIT
-->

# 0247. SD card readers are one polled queue per slot, through ADMA2, and their cards are named `mmcblk`

- Status: Accepted
- Date: 2026-11-01

## Context

Most laptops of the last fifteen years have an SD card reader, and on
many it is a PCI SD host controller (class 0805), as are the SD and
eMMC controllers of many chipsets. The SD Association publishes the
*SD Host Controller Simplified Specification* (version 4.20) and the
*Physical Layer Simplified Specification* (version 9.10) for anyone to
implement. QEMU's `sdhci-pci` is a version 2.00 controller with one
slot and 32-bit descriptors.

A controller has up to six slots, each with its own registers in a BAR.
A card is set up by a fixed series of commands, then read and written
in 512-byte blocks; its CSD says how many. ADMA2, the controller's
scatter-gather DMA, takes a table of descriptors, each a buffer of up
to 64 KiB, with 32-bit addresses before version 3.00 and 64-bit ones
after, where the controller has them.

Linux's `sdhci.c` and `mmc_block` are GPL and were not used; FreeBSD's
`sys/dev/sdhci` (BSD-2-Clause) was not needed. Linux names the cards
`mmcblk0` and on, and their partitions `mmcblk0p1`.

## Decision

- `radix-sdhci`, a compartment with no unsafe code, drives a slot from
  the two specifications: it resets the slot, needs ADMA2, uses 64-bit
  descriptors where the controller has them, and sets a card up with
  CMD0, CMD8, ACMD41, CMD2, CMD3, CMD9, CMD7, the default speed's
  25 MHz and the four-bit bus (ACMD6). Reads and writes are CMD17,
  CMD18, CMD24 and CMD25, the controller sending CMD12 after the
  multiple ones. Its tests run it against a simulated controller and
  card, with both sizes of descriptor.
- Each slot with a card in it at boot is one lane of a disk queue of its
  own (`fs/disk/queue.rs`), polled by the threads waiting for it as an
  IDE channel is (ADR 0244); its interrupt is left off. A request that
  fails stops the card (CMD12) before the next. One unanswered for 30 s
  has the lines reset and goes again.
- A slot with 32-bit descriptors takes a request whose buffers all lie
  below 4 GiB as it is, and any other through a bounce buffer of 32
  pages below 4 GiB.
- A card keeps no write cache the host sees, so a flush is done at once.
  A card whose write-protect switch is set is read-only. Discards (the
  cards' ERASE) are not sent yet.
- The cards are named `mmcblk0` and on, as on Linux, and
  `/kernel/block/mmcblk0/device` gives the card's product name, its
  serial number in hex and its revision, as Linux's sysfs does.

## Consequences

- The boot test `storage-sd` runs the storage test on four SD cards in
  four of QEMU's readers, with 5 GiB of memory so that requests go
  through the bounce buffer both ways.
- Cards inserted or taken out after boot are not seen, UHS-I speeds
  (SD 3.0's 1.8 V signalling, up to 104 MB/s) and eMMC are left for
  later, and a reader on USB is a USB stick to the kernel (ADR 0161).
- Readers that are not SD host controllers (some Realtek and O2Micro
  ones with their own registers) are not driven yet.
