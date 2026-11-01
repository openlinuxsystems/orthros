<!--
SPDX-FileCopyrightText: 2026 The Dukovo contributors
SPDX-License-Identifier: Apache-2.0 OR MIT
-->

Section: Specifications and papers
- Intel's datasheets for the 8254x, 8257x, 82576, 82580, I350, I210,
  I225, I226, 82599, X540 and X550 Ethernet controllers, from which
  `radix-intel-net` drives them, with FreeBSD's `em(4)`, `igb(4)`,
  `igc(4)` and `ix(4)` drivers (BSD-2-Clause and BSD-3-Clause) and
  Intel's shared code in them (BSD-3-Clause, Copyright Intel
  Corporation) as the reference for the reset, PHY and semaphore
  sequences and the errata the datasheets leave to their specification
  updates; FreeBSD's `vmx(4)` (ISC, Copyright Tsubai Masanari, Bryan
  Venteicher and Patrick Kelsey) and VMware's public VMXNET3 headers for
  the driver-shared area, rings and commands of
  `radix-vmxnet3`; Amazon's ena-com and FreeBSD's `ena(4)` (BSD-3-Clause,
  Copyright Amazon.com, Inc. or its affiliates) for the registers,
  readless reads, admin, completion and event queues and descriptors of
  `radix-ena`; and FreeBSD's `gve(4)` (BSD-3-Clause, Copyright Google LLC)
  for gVNIC's registers, admin commands, queue page lists and GQI rings
  in `radix-gve`, with Linux's `gve` (GPL-2.0) read for the bits of its
  admin queue by address on revision 1
  ([ADR 0197](docs/adr/0197-network-cards-share-one-driver-interface-and-copy-frames-through-slots.md)).
  No code is taken from any of them.
- FreeBSD's `re(4)` and its `if_rlreg.h` (BSD-4-Clause, Copyright Bill
  Paul), the reference for the registers, descriptors, chip revisions
  and their quirks, and the start and stop sequences from which
  `radix-realtek-net` drives Realtek's RTL8169, RTL8168 and RTL810x
  chips. No code is taken from it.
