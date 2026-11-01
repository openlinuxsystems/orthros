<!--
SPDX-FileCopyrightText: 2026 The Dukovo contributors
SPDX-License-Identifier: Apache-2.0 OR MIT
-->

# 0244. IDE disks, and SATA controllers in IDE mode, are one polled queue per channel, with bus-master DMA

- Status: Accepted
- Date: 2026-10-31

## Context

Many machines of 2008 to 2016 that boot Dukovo have their SATA
controller set to IDE mode in the firmware's setup, either by default
or because an older system needed it. In IDE mode the controller shows
itself as a PCI IDE controller (class 0101) with two channels, each with
a master and a slave, and no AHCI registers, so the AHCI driver (ADR
0112) never sees the disks. Real parallel ATA controllers, and QEMU's
`piix3-ide`, look the same.

An IDE channel takes one command at a time for both its devices, through
eight I/O ports (the legacy ones at 0x1f0 and 0x170, or BARs 0 to 3 in
native mode) and a control port. Data moves by bus-master DMA (the
*Programming Interface for Bus Master IDE Controller*, 1.0): a table of
physical region descriptors, each a buffer below 4 GiB of at most
64 KiB that does not cross a 64 KiB line. The commands are ATA's
(ACS-4), as on AHCI.

Linux's `ata_piix` and `pata_*` drivers are GPL. FreeBSD's `sys/dev/ata`
(BSD-2-Clause) was read as a reference for the order of a channel's
reset and probe; nothing is copied.

## Decision

- The channel driver is our own, written from the bus-master interface
  and ACS-4, in a compartment with no unsafe code, `radix-ide`, tested
  against a simulated channel with disks and an optical drive. It reads
  IDENTIFY DEVICE with `radix_ahci::ata`, which AHCI's disks use, so a
  disk is taken or refused by the same rules (48-bit addresses, 512- or
  4096-byte sectors).
- A channel is one lane of one disk queue (`fs/disk/queue.rs`), and each
  disk on it a volume of that queue, as an NVMe controller's namespaces
  are: the master is target 0, the slave 1. The queue then sends one
  command at a time to the channel, whichever disk it is for. The disks
  are named `sda` and on, as SATA disks are.
- Commands are READ DMA EXT, WRITE DMA EXT and FLUSH CACHE EXT, polled
  by the threads waiting for them, with the channel's interrupt off
  (nIEN): the kernel routes neither the legacy IRQs 14 and 15 nor INTx.
  A command unanswered for 30 s has the channel reset (a software reset)
  and goes again.
- A request whose buffers all lie below 4 GiB goes to them. Any other
  goes through a bounce buffer of 32 pages below 4 GiB, copied before a
  write and after a read: page caches are not kept below 4 GiB.
- When a disk says it is Serial ATA (IDENTIFY's word 76 or 222), its
  controller is a SATA controller in IDE mode. The log then says that
  setting it to AHCI in the firmware's setup lets its disks run faster,
  with queuing and hot-plug. The installer (Phase 14) shows the same
  hint before it installs.
- Packet devices (optical drives) are found and named in the log, and
  not driven yet.

## Consequences

- The boot test `storage-ide` runs the storage test on four disks on
  both channels of QEMU's PIIX3 IDE controller, with 5 GiB of memory so
  that requests go through the bounce buffer both ways.
- An IDE disk is slower than the same disk on AHCI: one command at a
  time per channel, no NCQ, no TRIM, no SMART yet, and a copy for
  buffers above 4 GiB. The hint tells the user how to avoid it.
- QEMU's IDE disks do not say they are Serial ATA, so the hint is tested
  only against the simulated channel.
