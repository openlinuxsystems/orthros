<!--
SPDX-FileCopyrightText: 2026 The Dukovo contributors
SPDX-License-Identifier: Apache-2.0 OR MIT
-->

# 0144. lspci, lscpu and lsmem are systools, with the PCI ID database as a port

- Status: Accepted
- Date: 2026-10-28

## Context

The hardware rows of Phase 11's administration commands are lspci,
lsusb, lscpu, lsmem, sensors, hwinfo and watchdog. An administrator runs
the first three on any Linux server to see what the machine is: its PCI
functions, its processor and its RAM. On Linux they come from pciutils
(GPL-2.0-or-later) and util-linux (GPL-2.0-or-later), which read
`/sys/bus/pci/devices`, `/proc/cpuinfo` and `/sys/devices/system/memory`.

Radix's `/kernel` already lists each PCI function's vendor, device and
class, and `/kernel/cpus` the CPUs it runs. It kept no revision or
subsystem, and nothing of the memory map once the boot report was
written. lspci's names come from the PCI ID Project's database,
`pci.ids`: data, not code, which may be distributed under the GPL,
version 2 or later, or the 3-clause BSD license.

## Decision

- lspci, lscpu and lsmem are tools of systools (ADR 0096), written for
  Radix in Rust, with the Linux tools' output and common options.
- `/kernel/bus/pci/devices/<address>` gains `revision`,
  `subsystem_vendor` and `subsystem_device`, as Linux's sysfs has them.
  An endpoint's subsystem is in its header; a bridge's is in its
  Subsystem ID capability, where Linux reads it.
- `/kernel/firmware/memmap/<n>` shows the firmware's memory map, one
  directory a range with its `start`, `end` (the last byte) and `type`,
  in Linux's names (`System RAM`, `Reserved`, `ACPI Tables`, `ACPI
  Non-volatile Storage`, `Unusable memory`), as Linux's
  `/sys/firmware/memmap`. lsmem puts it in memory blocks as Linux would:
  of 128 MiB, or 2 GiB above 64 GiB of RAM, each block that holds RAM
  online, since Radix takes no memory on or off line.
- lscpu reads the processor with CPUID, which user programs may run,
  rather than through a `/proc/cpuinfo`: the flags are the CPUID bits
  with Linux's names, without the flags Linux's kernel adds of its own.
  The topology is CPUID's too (leaf 0xB, else AMD's extended leaves).
- The port `pciids` installs `pci.ids` in
  `/system/resources/hwdata/pci.ids`, where hwdata puts it on Linux,
  under the 3-clause BSD license, pinned to a commit of the PCI ID
  Project's repository of daily snapshots. lspci reads only the entries
  for the functions it shows.

## Consequences

- The `posix` boot test runs the three tools on QEMU's q35 and checks
  what they say of it: its host bridge by name and number, its
  subsystem, 4 CPUs and 1 GiB of RAM in 8 blocks.
- A `/proc/cpuinfo`, for programs that read it, is still to come, as is
  lsusb, which waits for the USB stack to list devices.
- Updating the database is a change of the port's commit, as for any
  port.
