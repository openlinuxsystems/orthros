<!--
SPDX-FileCopyrightText: 2026 The Dukovo contributors
SPDX-License-Identifier: Apache-2.0 OR MIT
-->

# 0231. The firmware's SMBIOS tables show what the machine is, and hwinfo shows them

- Status: Accepted
- Date: 2026-10-31

## Context

hwinfo is the last of Phase 11's hardware commands (ADRs 0144 and
0230). What a machine is, as its maker names it, is in the firmware's
SMBIOS tables (the DMTF's DSP0134, once called DMI): who made it and
its model, its board, chassis and firmware, its processor sockets, its
memory devices and much more, each a structure of a numbered type with
strings after it. Linux reads them at boot, logs the machine on a line
of its own (`DMI: Dell Inc. Inspiron 5570/0F5T7R, BIOS 1.33.0
01/15/2025`), and shows its names in `/sys/class/dmi/id`, where udev's
hardware database, fwupd and many scripts look; the serial numbers and
the UUID there are root's alone. dmidecode (GPL-2.0-or-later) reads the
tables whole from `/sys/firmware/dmi/tables`, root's alone too.

Under UEFI the system table's configuration tables say where the entry
point is; a BIOS keeps it in its area from 0xf0000. The entry point is
the 32-bit `_SM_` of SMBIOS 2, or the 64-bit `_SM3_` of SMBIOS 3, which
newer firmware gives alone. Radix kept nothing of these tables, and its
VFS has no owners or modes for a file only root may read.

## Decision

- **A new crate, `radix-smbios`**, reads the tables from DSP0134,
  version 3.7.0, with no code of Linux's or dmidecode's: both entry
  points with their checksums and Linux's fixes of versions firmware
  gets wrong (2.33 for 2.3, 2.51 for 2.6), the structures up to the end
  of the table, their strings (none for 0, a number past them, or spaces
  alone), the fields of `/sys/class/dmi/id` as Linux takes them (each
  from the first structure that has it; the UUID little-endian from 2.6;
  none where it is all zeros or all ones), processors (type 4) and
  memory arrays and devices (types 16 and 17), with their extended
  sizes, speeds and counts. The kernel and hwinfo share it.
- **The x86-64 layer finds the tables at boot**, after the ACPI tables:
  through the UEFI system table, whose address the boot loader passes,
  or in the BIOS's area; `_SM3_` before `_SM_`, as Linux takes them. It
  reads them where the firmware keeps them, as Linux does, in memory the
  firmware's memory map reserves; tables in memory the map calls free,
  above the first MiB, may already be the kernel's, and are refused with
  a warning.
- **The boot log says what Linux's does**: `SMBIOS 3.0.0 present.`,
  `DMI: QEMU Standard PC (Q35 + ICH9, 2009), BIOS ... 04/01/2014`, and
  `DMI: Memory slots populated: 1/1`.
- **`/kernel/class/dmi/id`** has Linux's files for the names the
  firmware gives (`sys_vendor`, `product_name`, `board_name`,
  `bios_version`, `chassis_type` and the rest), one value each, for
  anyone to read; a file the firmware gives no string for is not there.
  The serial numbers of the machine, its board and chassis, and its
  UUID, are not there.
- **The platform capability reads the tables whole** with a new method,
  `smbios` (radix-platform(2), version 2), which copies the entry point
  or the structures and needs the read right, as Linux's
  `/sys/firmware/dmi/tables` needs root. A program has the capability
  when Spark gave it `admin.hardware` (ADR 0208), as the console's shell
  has it.
- **hwinfo** (hwinfo(8)), a new command in systools, shows the system,
  board, chassis, firmware, processors and memory devices from the
  tables, a section each, or the sections named. Without the capability
  it shows what `/kernel/class/dmi/id` has, and says the processors and
  memory need it. Serial numbers and the UUID are left out unless `-s`
  asks for them, so that its output can be pasted in a bug report or a
  forum as it is.

## Consequences

- `cat /sys/class/dmi/id/product_name` works as on Linux, and scripts
  that pick a quirk by the machine's name find it there.
- The `smbios` boot test boots QEMU's q35 under UEFI with its own
  system's strings and serial (`-smbios type=1,...`), checks the boot
  lines and the files, and runs hwinfo at the console; the BIOS's area
  is checked by the `smbus` test's boot under SeaBIOS. The rest is the
  host tests', against tables cut from QEMU's and the Framework
  Desktop's.
- No owner or mode gates a file yet; when Phase 13 gives files owners,
  root may read the serial numbers in `/kernel/class/dmi/id` as on
  Linux, and the capability stays for hwinfo.
- Not yet: Linux's `modalias`, `/sys/firmware/dmi/entries`, and hwinfo's
  memory modules' own SPD data over the SMBus (Phase 11e.5b.2).
