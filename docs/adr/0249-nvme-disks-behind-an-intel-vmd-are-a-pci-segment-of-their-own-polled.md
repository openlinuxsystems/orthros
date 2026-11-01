<!--
SPDX-FileCopyrightText: 2026 The Dukovo contributors
SPDX-License-Identifier: Apache-2.0 OR MIT
-->

# 0249. NVMe disks behind an Intel VMD are a PCI segment of their own, set up by the kernel and polled

- Status: Accepted
- Date: 2026-11-01

## Context

Many laptops with Intel processors from Tiger Lake on ship with the
firmware's storage set to "RST", "RAID" or "Intel Optane". The NVMe
disks, and the root ports they are on, then vanish from the machine's
PCI buses into the domain of a Volume Management Device (VMD): one PCI
function (class 0104h) whose BAR 0 is the configuration space of a PCI
domain of its own, laid out as ECAM is, and whose BARs 2 and 4 are the
memory the devices in it are placed in. On such a laptop an operating
system without a VMD driver sees no disk, and installers tell the user
to change the setting first, which some vendors hide.

The firmware leaves the domain unset: the operating system numbers its
buses and places its devices' BARs and its bridges' windows, as it
would a host bridge's. On the client models the buses start where
VMCONFIG's bits 8 and 9 say (0, 128 or 224), when VMCAP's bit 0 says
they are restricted; MEMBAR2's first 8 KiB are the VMD's MSI-X table
and pending bits. The devices' MSI-X messages go to the VMD, which
raises one of its own vectors for them; on some models that remapping
can be turned off. Their DMA reaches the IOMMU with the VMD's own
requester ID.

Intel publishes no specification of the VMD's own registers. FreeBSD's
`vmd(4)` (BSD-2-Clause) was read for them; Linux's `vmd.c` is GPL and
was not used. QEMU has no VMD.

## Decision

- `radix-vmd`, a compartment with no unsafe code and no dependencies,
  knows the VMDs' device IDs, reads where a domain's buses start, and
  sets the domain up through the caller's configuration space: it turns
  every device's decoding off, numbers the buses behind each bridge
  depth first (six bridges deep at most), sizes every memory BAR from
  the PCI Local Bus Specification, packs them into each bridge's windows
  from the largest alignment down, and places the windows and the first
  bus's BARs in MEMBAR1 (below 4 GiB) and in MEMBAR2 past what the VMD
  keeps (prefetchable 64-bit BARs, where every bridge above has a 64-bit
  prefetchable window). The domain has no I/O space; I/O BARs are left
  off. Its tests run it against a simulated domain of root ports, a
  switch, disks and an AHCI controller.
- The kernel (`fs/disk/vmd.rs`) binds a VMD found in the PCI scan, maps
  32 of its buses at most, sets the domain up, and walks it as PCI
  segment 1000 (then 1001 and on), logging and listing each function as
  the firmware's are, in `/kernel/bus/pci`. Its NVMe controllers are
  bound as the others are and named `nvme0` and on in the order found,
  but polled, as a controller without MSI-X vectors to spare is, and
  their DMA is mapped for the VMD's address in the IOMMU.
- Other functions behind a VMD, as the AHCI controller some keep, are
  logged and not used. A domain that cannot be set up is a warning that
  says why and that setting the firmware's storage to AHCI shows the
  disks.

## Consequences

- A laptop set to RST boots and installs with its NVMe disks seen, as
  on Linux, without the user having to find the firmware's setting.
- Polling costs a little latency and CPU time on those disks, so
  routing the VMD's MSI-X vectors (or turning remapping off on the
  models that can) is left for a later change; so is driving an AHCI
  controller behind it, and the RAID volumes RST makes, which Linux too
  leaves to `mdadm`.
- No boot test runs it, since QEMU has no VMD; the host tests check the
  numbering, the placement (every BAR aligned, inside its bridge's
  window, none overlapping) and the errors, and the NVMe driver behind
  it is the one every NVMe boot test runs. Neither reference machine
  has a VMD (the Dell's processor predates them, and the Framework
  Desktop's is AMD's).
