<!--
SPDX-FileCopyrightText: 2026 The Dukovo contributors
SPDX-License-Identifier: Apache-2.0 OR MIT
-->

# 0112. SATA disks through AHCI, written from Intel's specification, in a crate without unsafe code

- Status: Accepted
- Date: 2026-10-25

## Context

The Dell Inspiron 15 5570, one of the two reference machines, keeps its
disks on SATA, behind the chipset's AHCI controller, and QEMU's q35
machine has one built in. Phase 10 roots the system on a disk, so the
driver is where a bug loses data. Linux's `libata` is GPL and is not
read (the rule for every GPL source in this project).

## Decision

- `radix-ahci` drives AHCI 1.x controllers and the ATA disks on them,
  written from Intel's *Serial ATA AHCI 1.3.1* specification and the
  ATA command set (ACS) and SATA specifications. Like `radix-virtio` it
  is a compartment: `forbid(unsafe_code)`, the controller reached only
  through `radix-hw`'s MMIO windows and DMA memory, no locks and no
  sleeping inside (the kernel passes in how to wait). It runs on the
  host against a simulated controller and disk.
- The controller is taken from the firmware (BIOS/OS handoff), reset
  into AHCI mode, and every port's link is reset at once (COMRESET),
  with link power saving turned off. A link up twice in a row 5 ms
  apart is up; one with no device for 100 ms is empty; a device gets a
  second to bring its link up and ten to stop being busy, for a hard
  disk spinning up.
- A disk queues (NCQ) when the controller and the disk both do: reads
  and writes go out as READ and WRITE FPDMA QUEUED, a slot's number
  their tag, up to the disk's depth. A flush, a TRIM, and every command
  to a disk that does not queue go out alone. Writes through the cache
  use FUA where the disk has it. TRIM takes up to 64 ranges a command.
- An error stops the port and turns its interrupts off. Recovery stops
  the command engine, clears the error, clears a stuck busy bit (command
  list override) or resets the link when it must, and starts the engine
  again. For a queued command it reads the disk's NCQ Command Error log,
  which names the failed tag and which the disk needs read before it
  takes another command. The command the disk names fails if the disk
  could not read or find its sectors (UNC, IDNF, AMNF), as it would fail
  again; after any other error (an aborted command, a CRC error on the
  link) it goes again, as every other one in flight does, five times at
  most (ADR 0111). This is Linux's rule too, which retries everything but
  media errors. Without the log, the link is reset and all go again. A
  disk that does not come back, or a different disk after a reset
  (another serial number or size), is gone: its requests fail.
- One MSI for the whole controller: the handler collects each port the
  controller says has news, then clears the controller's status. A
  controller without MSI is polled.
- Not supported for now, with a line in the log: port multipliers,
  ATAPI devices (optical drives), disks with 4096-byte logical sectors,
  disks without 48-bit addresses, and controllers that reach only the
  first 4 GiB of memory, since page caches are not kept below it. Disks
  are not hot-plugged: a pulled disk is gone until the next boot.

## Consequences

- SATA disks are `sda`, `sdb` and on, in the order controllers and
  ports are found, with their partitions, as on Linux.
- Every boot on q35 probes the built-in controller, which adds about
  0.1 s for its empty ports. Linux spends that too, though in parallel
  with the rest of its boot.
- The `storage-sata` boot test runs the storage test on four disks of
  an added AHCI controller, and `disk-errors` has QEMU fail reads.
- Hot plug, port multipliers and 4Kn disks can be added inside the
  crate when hardware calls for them.
