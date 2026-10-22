<!--
SPDX-FileCopyrightText: 2026 The Dukovo contributors
SPDX-License-Identifier: Apache-2.0 OR MIT
-->

# 0053. Virtio devices are driven through the modern PCI transport only, with split virtqueues

- Status: Accepted
- Date: 2026-10-22

## Context

Virtio is how a virtual machine's disks, network cards and consoles
reach their hypervisor, and Dukovo runs in QEMU and KVM in every test.
The specification (OASIS, version 1.2) has two PCI transports: the
legacy one of virtio 0.9, whose registers sit in an I/O port BAR and
whose byte order is the guest's, and the modern one of virtio 1.0 and
later, which describes its register blocks with vendor-specific PCI
capabilities in memory BARs and is little-endian throughout. It has two
queue layouts: split virtqueues (a descriptor table, an available ring
and a used ring) and packed virtqueues (one ring), which devices may
offer as a feature.

## Decision

- `radix-virtio` drives the modern PCI transport only: the common
  configuration, the notification doorbells, the interrupt status and
  the device configuration, located through the function's virtio
  capabilities. Devices that offer only the legacy transport are left
  alone. QEMU's `virtio-*-pci` devices are transitional (both
  transports), so they work.
- Queues are split virtqueues. The driver accepts `VERSION_1` and only
  the features it uses.
- Each queue raises its own MSI-X vector (ADR 0052).
- The crate is a compartment, with `#![forbid(unsafe_code)]`, reaching
  its device only through `radix-hw`'s checked `Mmio` and `Dma` types;
  its tests run the drivers against a simulated device on the host.

## Consequences

- One code path, with no byte-order or I/O-port handling, and no legacy
  quirks to test.
- Hypervisors that offer only legacy virtio (very old QEMU and some
  older VirtualBox versions) give Dukovo no virtio disks. VirtualBox
  guests use its AHCI controller instead, which arrives with the
  reference machine's drivers in Phase 10.
- Packed virtqueues can be added later as a feature, if measurements
  show the split layout costs anything.
