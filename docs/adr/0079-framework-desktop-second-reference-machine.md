<!--
SPDX-FileCopyrightText: 2026 The Dukovo contributors
SPDX-License-Identifier: Apache-2.0 OR MIT
-->

# 0079. The Framework Desktop is the second reference machine

- Status: Accepted
- Date: 2026-10-23

## Context

ADR 0009 made a Dell Inspiron 15 5570 laptop the reference machine. It is
an Intel machine from 2017, with SATA disks and Fast Ethernet. Now that
1.0 is a server release (ADR 0078), it cannot test several things a
server needs:

- NVMe;
- mirrored disks;
- fast Ethernet;
- AMD's platform devices;
- many cores.

The maintainer added a Framework Desktop to the project on 2026-10-23:

- an AMD Ryzen AI Max+ 395 (Zen 5, 16 cores and 32 threads);
- 128 GB of memory;
- two NVMe slots;
- a Realtek RTL8126 5 GbE port;
- a Radeon 8060S GPU and an XDNA 2 NPU.

Framework publishes the mainboard's specifications, and its embedded
controller firmware is open source.

## Decision

- The Framework Desktop is the second reference machine, alongside the
  Dell (ADR 0009) and QEMU. Every one of its devices gets a driver,
  listed with its phase in the roadmap's Reference machines section.
- From Phase 10 on, phase exit criteria name both machines where they
  apply:
  - AHCI first for the Dell, then NVMe for the Framework;
  - Btrfs RAID1 across the Framework's two NVMe drives;
  - the Realtek driver family covers both Ethernet chips;
  - AMD platform support and the embedded controller in Phase 11.
- The Framework is also the bare-metal test server for Phase 16, the
  multi-core performance machine, and the AMD GPU machine for gaming
  (Phase 29). Its Zen 5 cores match CI's newer runners.

## Consequences

- AMD-Vi joins Intel VT-d in Phase 11, and the drivers for this machine
  are written from documentation or permissively licensed code, since
  Linux's are GPL (for example OpenBSD's ISC-licensed rge(4) for the
  RTL8126).
- Its closed firmware files go in the optional firmware package
  (ADR 0007).
- The machine boots only under UEFI; BIOS boot stays tested in QEMU and
  on the Dell.
- It is smoke-booted at the end of every phase once it is set up, and its
  results join the Dell's in the handbook's Supported hardware chapter.
