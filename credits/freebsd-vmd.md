<!--
SPDX-FileCopyrightText: 2026 The Dukovo contributors
SPDX-License-Identifier: Apache-2.0 OR MIT
-->

Section: Specifications and papers
- FreeBSD's `vmd(4)` driver (BSD-2-Clause), from which `radix-vmd`
  read the registers of Intel's Volume Management Device that Intel
  does not publish (VMCAP, VMCONFIG and where its buses start), and the
  PCI Local Bus Specification 3.0 and PCI-to-PCI Bridge Architecture
  Specification 1.2, from which it numbers the buses behind a VMD and
  places their devices' memory. The VMDs' device IDs, which of them
  restrict their buses, and numbering their domains past the firmware's
  segments follow Linux's `vmd` (GPL-2.0)
  ([ADR 0249](docs/adr/0249-nvme-disks-behind-an-intel-vmd-are-a-pci-segment-of-their-own-polled.md)).
  No code is taken from either.
