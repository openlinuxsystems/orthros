<!--
SPDX-FileCopyrightText: 2026 The Dukovo contributors
SPDX-License-Identifier: Apache-2.0 OR MIT
-->

# 0164. Each device reaches only its own memory behind Intel VT-d

- Status: Accepted
- Date: 2026-10-30

## Context

Phase 11d, its first part. Until now every PCI device could read and
write all of memory by DMA. A broken or hostile device, a Thunderbolt or
USB4 one plugged in, or a driver bug that hands a device a stale address
could reach any page. The Phase 12 network drivers are the next DMA
users, and their buffers come from the network, so they wait for this.

Intel's VT-d (the *Intel Virtualization Technology for Directed I/O
Architecture Specification*, revision 4.1) puts remapping units between
groups of devices and memory. The ACPI DMAR table says where each unit's
registers are, which devices it translates, and which memory the
firmware keeps for devices' own use (RMRRs). A unit translates each
request by the device's requester ID, through a root table, a context
table and the page tables of the device's domain. A device whose context
entry is not present reaches nothing, and the unit records a fault for
it.

Linux gives each device's driver a DMA API that returns a device address
(IOVA) per mapping, and by default batches invalidations ("lazy" mode),
leaving a window where a device can still reach memory given back.
Radix's drivers hand devices physical addresses everywhere.

## Decision

- **A device address is a physical address.** Each domain maps a page at
  its own physical address, so drivers keep giving physical addresses
  with or without a unit, and no driver keeps a second address.
- **Each PCI function gets its own domain**, attached the first time DMA
  memory is allocated or mapped for it. It reaches its DMA memory
  (`Devices::alloc_dma(source, …)`, mapped until the memory is dropped),
  the pages mapped for its requests in flight (`map_dma` before a
  transfer, `unmap_dma` once the device is done), and the memory the
  firmware reserved for it. A function nothing asked DMA for reaches no
  memory at all.
- **Disk requests map their buffers only while in flight:** the disk
  queue maps a request's pages before it goes to the controller and
  takes them back once the controller is done, for virtio, AHCI, NVMe
  and USB disks alike. A USB disk's pages are mapped for its xHCI
  controller.
- **Invalidation is strict:** when `unmap_dma` returns, the unit has
  forgotten the pages, so the device can no longer reach them. Each page
  counts its mappings in bits the unit ignores, so two requests on one
  page do not take it from each other. Page-selective invalidations go
  in naturally aligned blocks. A unit without them, or a block past its
  largest, invalidates the whole domain. Empty page tables are freed once
  the unit has forgotten them.
- **Taking over a unit:** at the end of `Platform::init`, before any
  driver starts a device, the kernel switches off whatever firmware or
  an earlier kernel left on (translation, interrupt remapping, the
  invalidation queue). It then gives the unit an empty root table, uses
  queued invalidation where the unit has it, maps the RMRRs for their
  devices, and turns translation on. Page tables have 4 levels where the
  unit walks them, else 3.
- A unit that translates only the display controller is left off, as no
  graphics driver hands it memory yet. The `iommu=off` flag leaves every
  unit off. Devices behind a unit left off reach all memory, as before.
- **Faults** raise the unit's own MSI, whose handler reads the fault
  records under a lock of their own, so it never waits for a change to
  the tables. It logs each fault with the device, the address and the
  unit's reason, at most 10 in each 5 s.
- The engine is `radix-vtd`, a crate with no `unsafe` code. The kernel
  lends it page memory and the registers. Host tests run it against a
  simulated unit that walks the tables as the specification says
  hardware does, IOTLB included. The DMAR parser is in `radix-acpi`.
- virtio devices are told their DMA goes through the platform's IOMMU
  (`VIRTIO_F_ACCESS_PLATFORM`, accepted whenever offered).

## Consequences

- A device can no longer reach memory it was not given, and a stopped
  access is logged, not silent. The `iommu` boot test shows it: behind
  QEMU's VT-d unit, the `disk-raw` checks pass on virtio, SATA and NVMe
  disks and the boot stick, and `selftest=iommu` has QEMU's `edu` device
  copy into a page it was given, and get stopped copying into one it was
  never given, and into one taken back.
- Each disk request costs a map and an unmap, and an unmap waits for the
  unit to invalidate. That is the price of the strict window; a lazy
  mode can come later if a benchmark on hardware asks for it. Without a
  unit, the cost is one load of a flag.
- Phase 12's network drivers allocate their rings with
  `alloc_dma(source, …)` and map packet buffers with `map_dma` for as
  long as the device has them.
- Not yet: interrupt remapping (a device may still send any interrupt
  message), AMD-Vi (11d's later parts), devices behind a PCIe-to-PCI
  bridge (their requests carry the bridge's ID, which their domain is not
  attached for), and DMA for the display controller.
