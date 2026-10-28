lspci(8) "Dukovo" "Dukovo Manual"

; SPDX-FileCopyrightText: 2026 The Dukovo contributors
; SPDX-License-Identifier: Apache-2.0 OR MIT

# NAME

lspci - list the PCI functions

# SYNOPSIS

*lspci* [_option_ ...]

# DESCRIPTION

lspci lists the PCI functions the kernel found, from
_/kernel/bus/pci/devices_ (*radix-sysfs*(5)), in the order of their
addresses, as pciutils' lspci lists them on Linux: each function's slot
(_bus_:_device_._function_, with the PCI domain in front when there is
more than domain 0), its class, its vendor and device, and its revision
when that is not 0.

```
$ lspci
00:00.0 Host bridge: Intel Corporation 82G33/G31/P35/P31 Express DRAM Controller
00:1f.2 SATA controller: Intel Corporation 82801IR/IO/IH (ICH9R/DO/DH) 6 port SATA Controller [AHCI mode] (rev 02)
```

The names come from the PCI ID database of the PCI ID Project
(https://pci-ids.ucw.cz), _/system/resources/hwdata/pci.ids_, which
Dukovo ships under the 3-clause BSD license. A function the database
does not name shows its numbers: _Device 8086:2922_, or _Class 0106_.

# OPTIONS

*-n*
	Numbers instead of names: the class, then _vendor_:_device_. Twice
	(*-nn*), both: each name with its number in brackets.

*-v*
	The programming interface after each function, with its name, then
	the subsystem (the board's vendor and model) on a line of its own,
	and a blank line after each function. Linux's lspci shows more:
	the function's resources and capabilities.

*-m*, *-mm*
	One line a function for scripts, each field in quotes: the slot,
	class, vendor and device, *-r* and the revision and *-p* and the
	programming interface when not 0, then the subsystem's vendor and
	device (*""* when it has none).

*-D*
	The PCI domain in every slot.

*-s* [[[[_domain_]:]_bus_]:][_device_][.[_function_]]
	Only the functions at that slot, each part in hexadecimal; a part
	left out or *\** matches any.

*-d* [_vendor_]:[_device_][:_class_]
	Only the functions with those IDs, in hexadecimal; _class_ is the
	class and subclass, as *0106*.

*-i* _file_
	Names from _file_, a database in the format of _pci.ids_.

*-k* is taken and changes nothing.

# EXIT STATUS

0, or 1 if the kernel's list cannot be read or for an option it does
not know.

# SEE ALSO

*lscpu*(1), *lsmem*(1), *radix-sysfs*(5), *systools*(1)
