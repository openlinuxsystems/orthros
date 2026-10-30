<!--
SPDX-FileCopyrightText: 2026 The Dukovo contributors
SPDX-License-Identifier: Apache-2.0 OR MIT
-->

# 0166. Each device raises only its own interrupts

- Status: Accepted
- Date: 2026-10-30

## Context

Phase 11d, its third part. ADRs 0164 and 0165 confined each device's DMA
to the memory it is given, behind Intel's VT-d and AMD's IOMMU. A device
raises an interrupt with a DMA write too, to the local APICs' window at
0xfee00000, and those writes passed through untranslated: any device, or
the firmware running on it, could raise any vector at any CPU, those of
other devices and the kernel's own among them, by writing the right
message.

Both IOMMUs can remap interrupts. VT-d has one table of interrupt
entries for a unit, which a device picks with a handle in a message of
its own *remappable* format; each entry names the vector, the CPU and the
requester ID allowed to use it, and with interrupt remapping on, the
old *compatibility* format can be blocked. AMD-Vi gives each device ID a
table of its own in its device table entry, which the low bits of the
message's data pick; the message keeps its old format. An I/O APIC's
inputs come through the IOMMU too: on VT-d its redirection entries take
the remappable format, and on AMD-Vi its requests carry a device ID the
IVRS table gives (an IVHD "special" entry).

## Decision

- **Every vector gets an entry of the same number.** The kernel's device
  vectors (0x50 to 0xef) are one pool for every CPU, so a vector names
  one interrupt of one device or I/O APIC input. Its entry is entry
  *vector* of the device's unit (VT-d) or of the device's own table
  (AMD-Vi): no allocator of entries is needed, and the 256 entries a
  table has are enough.
- **The HAL's `allocate_interrupt` names the device** that will raise it,
  as `alloc_dma` does. Behind a unit that remaps, it gives that device an
  entry and returns the message that picks it; the disk, NVMe, AHCI,
  virtio and USB drivers pass their PCI function. `free_interrupt` takes
  the entry away before the vector can be handed out again.
- **VT-d**: once every unit translates, interrupts are remapped if every
  unit can, with queued invalidation, and every I/O APIC is one a unit's
  device scope names; else none are, and the log says why. Each unit gets
  a table of 256 entries, its entry cache is invalidated, compatibility
  interrupts are blocked (the CFI bit stays clear), and remapping goes on.
  An entry checks all 16 bits of the requester ID. With the local APICs
  in x2APIC mode and a unit that has extended interrupt mode, entries
  name 32-bit APIC IDs, so a device's queue can send its interrupts to
  any CPU; otherwise a CPU above 255 gets the boot CPU's, as before.
- **AMD-Vi**: interrupts are remapped if every I/O APIC is one an IVHD
  block names. The device table is then made with every device's
  interrupts aborted from the takeover on (its entry's interrupt part
  valid, with no table), and a device gets a table of 256 32-bit entries
  the first time it is given an interrupt. The IOMMU's own event
  interrupt gets an entry of its function's, should the IOMMU remap it.
  32-bit entries name APIC IDs up to 255.
- **I/O APIC inputs** (the PS/2 keyboard's and ACPI's SCI) get entries
  too: on VT-d the redirection entry takes the remappable format with the
  vector as its handle, and an entry's trigger mode follows the input's,
  so a level-triggered input's end of interrupt reaches the I/O APIC; on
  AMD-Vi the redirection entry is as before and the vector picks the
  entry.
- **Faults**: an interrupt a unit blocked is logged as a stopped DMA is,
  "*device* was stopped raising an interrupt: *why*", within the same
  limit of 10 in each 5 s.
- `iommu=off` leaves interrupt remapping off with the rest.
- The engines' host tests (`radix-vtd`, `radix-amdvi`) raise interrupts
  through their simulated IOMMUs: an entry not given, given to another
  device, or taken away is blocked, and so, under remapping, is a
  compatibility-format one.

## Consequences

- Behind either IOMMU a device raises only the interrupts it was given,
  at the CPU the kernel chose. The `iommu` and `iommu-amd` boot tests run
  with QEMU's `intremap=on` (and, under KVM, `kernel-irqchip=split`,
  which an IOMMU that remaps interrupts needs): the disks' and the boot
  stick's interrupts arrive through their entries, `selftest=iommu` has
  `edu` raise its own interrupt, which arrives, and one given to the
  host bridge, with the host bridge's message, which is blocked, and the
  power button's SCI, through the I/O APIC's entry, powers the machine
  off.
- Phase 12's network drivers pass their PCI function to
  `allocate_interrupt` and `allocate_interrupt_at`, as these drivers do.
- QEMU passes compatibility-format interrupts through whatever the CFI
  bit says, so the blocking of that format is checked only by the host
  tests until the Dell's run.
- A device behind a PCIe-to-PCI bridge, whose requests carry the
  bridge's ID, cannot raise its interrupts on VT-d (nor reach its DMA
  memory, ADR 0164); it comes with those devices' DMA.
- Not yet: HPET MSIs (no driver uses them), posted interrupts for guests,
  and AMD-Vi's 128-bit entries for 32-bit APIC IDs (the Framework Desktop
  has 32 CPUs, all below 256).
