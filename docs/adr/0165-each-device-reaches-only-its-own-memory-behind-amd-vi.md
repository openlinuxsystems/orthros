<!--
SPDX-FileCopyrightText: 2026 The Dukovo contributors
SPDX-License-Identifier: Apache-2.0 OR MIT
-->

# 0165. Each device reaches only its own memory behind AMD-Vi

- Status: Accepted
- Date: 2026-10-30

## Context

Phase 11d, its second part. ADR 0164 put each device behind Intel's
VT-d. The Framework Desktop and every other AMD machine has AMD's IOMMU
instead (the *AMD I/O Virtualization Technology (IOMMU) Specification*,
48882, revision 3.10). Its devices could still reach all of memory.

An AMD IOMMU is a PCI function of its own. The ACPI IVRS table says
where its registers are, which function it is, where that function's
IOMMU capability block is, which device IDs (requester IDs) it
translates and the IDs some devices' requests carry instead of their
own, and which memory the firmware keeps for devices (IVMD blocks). The
IOMMU translates each request through one device table, of an entry per
device ID, and the page tables of the entry's domain. It takes commands
(invalidations, completion waits) through a ring in memory and reports
faults and errors through an event log, another ring, whose interrupt
is the function's own MSI.

Its page tables differ from VT-d's in their entries' bits, and they have
only 7 bits the IOMMU ignores, where VT-d's have 10.

## Decision

- **The same model as VT-d's** (ADR 0164): a device address is a
  physical address, each PCI function gets its own domain when DMA is
  first allocated or mapped for it, a function nothing asked DMA for
  reaches nothing, and invalidation is strict.
- **One page-table core for both.** `radix-iopt` holds the domains, the
  mapping counts and the invalidation batching that `radix-vtd` had.
  Each engine gives it its entry format and its way to invalidate. On
  AMD a page's count fits in the 7 ignored bits, so a page can be mapped
  127 times at once; a table's emptiness is found by looking at its
  entries rather than counted.
- **The engine is `radix-amdvi`**, with no `unsafe` code. Taking an
  IOMMU over: translation and the rings off, a device table as long as
  the IVRS's highest device ID needs with every entry blocking (valid,
  translating, no access), a command buffer and event log of a page
  each, translation on, then every cache the IOMMU has invalidated
  (`INVALIDATE_ALL` where it has it, else every device entry).
  Translation goes on with the rings because an IOMMU runs its command
  buffer only while it translates (QEMU's does, and Linux takes one over
  in this order); every device is blocked until its domain is attached.
  Page tables have 4 levels.
  Each command batch ends with a completion wait that stores a number
  the kernel polls, for at most a second.
- **Attaching** writes the device entry's domain, then its first word,
  so the entry blocks until it is whole, and invalidates the entry and
  the domain's pages.
- **Device IDs**: a device's domain is attached for the ID its requests
  carry through the IOMMU. Two functions whose requests carry one ID,
  such as those behind a PCIe-to-PCI bridge, share a domain.
- **Firmware memory**: an IVMD block's memory is mapped, read and write,
  for each device it names (or for every device) when that device's
  domain is made. The domains of the devices a block names by ID, up to
  64 of them, are made right after the takeover, so they lose that
  memory only for that moment. An exclusion range is mapped the same
  way, and the IOMMU's own exclusion registers are left off.
- **Events**: the kernel points the IOMMU function's MSI capability at an
  interrupt of its own and lets the function write. Its handler reads
  the event log under a lock of its own and logs each event as VT-d's
  faults are logged, at most 10 in each 5 s. An overflowed log is
  restarted.
- Interrupts pass through untranslated (the device entry's interrupt
  part is not valid), as before. `iommu=off` leaves every IOMMU off.
- Host tests run the engine against a simulated IOMMU that walks the
  tables as the specification says hardware does, with its device-entry
  cache and IOTLB, the command ring and the event log. The IVRS parser
  is in `radix-acpi`.

## Consequences

- On AMD machines a device can no longer reach memory it was not given.
  The `iommu-amd` boot test shows it behind QEMU's AMD IOMMU: the
  `disk-raw` checks pass on virtio, SATA and NVMe disks and the boot
  stick, and `selftest=iommu` has `edu` copy into a page it was given and
  fail to copy into one never given or taken back. QEMU logs no event for
  a page that is not mapped, so the test counts no faults; the
  Framework run, later, checks the event log.
- VT-d's code is now its share of `iommu/`: the common part (domains,
  devices, faults), `vtd.rs` and `amd.rs`. Its behaviour does not change.
- A firmware block that grants only reads is mapped for writing too,
  because domains map pages read and write; no machine is known to need
  otherwise.
- Not yet: interrupt remapping on either IOMMU, the 5- and 6-level page
  tables some AMD IOMMUs walk (4 levels reach 256 TiB), and DMA for the
  display controller.
