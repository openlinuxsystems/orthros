<!--
SPDX-FileCopyrightText: 2026 The Dukovo contributors
SPDX-License-Identifier: Apache-2.0 OR MIT
-->

# 0052. Device interrupts are message-signalled only, and PCI is reached through ECAM

- Status: Accepted
- Date: 2026-10-22

## Context

Phase 7 brings the first device drivers: virtio block devices on PCI.
A PCI device can raise an interrupt in three ways. Legacy INTx pins are
shared between devices and routed to the I/O APIC through the ACPI
`_PRT` tables, which means running AML, the bytecode in the firmware's
DSDT, and an interpreter for it (ACPICA, or a port of one). MSI and
MSI-X have the device write a message, an address and a value the
kernel chose, which the local APIC turns into an interrupt on one CPU,
with no routing tables, no sharing and one vector per queue. The PCI
Express specification requires every function that raises interrupts to
support MSI or MSI-X, so the reference machine's controllers (AHCI,
NVMe, xHCI, Realtek Ethernet) have them, and QEMU's virtio PCI devices
offer MSI-X.

PCI configuration space can be reached through the old I/O ports `0xcf8`
and `0xcfc`, which reach only the first 256 bytes of each function, or
through the PCI Express enhanced configuration access mechanism (ECAM):
a memory window per segment, described by the ACPI MCFG table, in which
every function's 4 KiB of configuration space has a fixed place.

## Decision

- Drivers use MSI-X when a function has it, and MSI otherwise. INTx is
  not supported, so the kernel needs no AML interpreter for interrupts.
  The architecture hands out a vector with the message that raises it
  (`radix_hw::MsiMessage`), aimed at one CPU.
- A device with no vector to spare is polled by the threads waiting for
  it rather than given a shared line.
- Configuration space is reached only through ECAM, from the MCFG
  table's windows. `radix_hw::pci` walks each segment from its first bus,
  follows PCI-to-PCI bridges, sizes the base address registers and
  programs the MSI and MSI-X capabilities, all in safe code over checked
  `Mmio` windows.
- The architecture maps device registers uncached and allocates DMA
  memory (`radix_hal::Devices`); `radix-hw` is the only crate with code
  that touches device registers or DMA memory directly, and it hands both
  out as safe types to drivers that forbid `unsafe` code.

## Consequences

- Interrupt setup is a few register writes per device, and each disk
  queue raises its own vector on its own CPU.
- A machine whose firmware has no MCFG table, or a device with neither
  MSI nor MSI-X, is not supported. Neither exists among the targets of
  Dukovo 1.0 (QEMU's q35 machine and x86-64 PCs from the last decade).
- An AML interpreter is still needed later for power management (the
  battery, the lid and sleep states in Phase 11), but not for any driver
  to work.
