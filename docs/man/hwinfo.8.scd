hwinfo(8) "Dukovo" "Dukovo Manual"

; SPDX-FileCopyrightText: 2026 The Dukovo contributors
; SPDX-License-Identifier: Apache-2.0 OR MIT

# NAME

hwinfo - what the machine is, as its firmware says

# SYNOPSIS

*hwinfo* [*-s*] [_section_...]

# DESCRIPTION

hwinfo shows what the firmware's SMBIOS tables say of the machine: who
made it and its model, its board, chassis and firmware, its processors'
sockets and its memory devices, a section each, in that order. Only the
sections named are shown: *system*, *board*, *chassis*, *firmware*,
*processor* and *memory*.

```
$ hwinfo system memory
System
  Manufacturer:     Framework
  Product:          Desktop (AMD Ryzen AI Max 300 Series)
  Version:          A6
  Family:           Desktop
  SKU:              FRAMDACP06

Memory
  Installed:        128 GiB in 8 of 8 slots
  Capacity:         128 GiB
  Error correction: None
  DIMM 0 (P0 CHANNEL A): 16 GiB LPDDR5 Row Of Chips, 8000 MT/s, Micron Technology MT62F2G32D4DS-023 WT
```

A memory device's line gives its size, kind and form, its speed (and
the speed it was set to, where that is lower), and who made it with its
part number; *empty* for a slot without a module. A processor socket's
gives the processor's name, its cores and threads, and its speeds in
MHz.

The tables come whole from the kernel through the platform capability
(*radix-platform*(2)), which a program holds when Spark gave it
admin.hardware (*spark*(8)), as the console's shell has it. Without the
capability, hwinfo shows what anyone may read in _/kernel/class/dmi/id_
(*radix-sysfs*(5)): the system, board, chassis and firmware, without
the processors and memory, and says so.

The serial numbers of the machine, its board, chassis and memory
devices, and the machine's UUID, are left out unless *-s* asks for
them, so that what hwinfo prints can be shared as it is.

# OPTIONS

*-s*, *--serials*
	Show the serial numbers and the UUID too. They need the platform
	capability.

Not yet: the memory modules' own SPD data, which the SMBus has, and
dmidecode's dump of every structure.

# EXIT STATUS

0 when the tables, or the kernel's files, were read; 1 when the
firmware gives no SMBIOS tables, when *-s*, *processor* or *memory*
were asked for without the platform capability, or for an option or
section it does not know.

# SEE ALSO

*lscpu*(1), *lsmem*(1), *radix-platform*(2), *radix-sysfs*(5),
*systools*(1)
