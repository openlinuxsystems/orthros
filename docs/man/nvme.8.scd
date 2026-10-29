nvme(8) "Dukovo" "Dukovo Manual"

; SPDX-FileCopyrightText: 2026 The Dukovo contributors
; SPDX-License-Identifier: Apache-2.0 OR MIT

# NAME

nvme - list the NVMe namespaces, and show a drive's health log

# SYNOPSIS

*nvme list*

*nvme smart-log* _disk_

# DESCRIPTION

nvme shows what NVMe drives say, as nvme-cli's *nvme list* and *nvme
smart-log* do.

*list*
	A line for each NVMe namespace the kernel has, by its name in
	_/devices_: the controller's serial number, model and firmware
	revision, the namespace's ID, its size in powers of 1000, and its
	sector size, from what _/kernel/block_ shows of it
	(*radix-sysfs*(5)).

*smart-log* _disk_
	The SMART / Health Information log of the controller of _disk_,
	a namespace in _/devices_ such as _nvme0n1_: its critical
	warning, temperature, spare capacity, endurance used, data read
	and written, power cycles, hours on, unsafe shutdowns, media
	errors and time spent too hot, a line each, as nvme-cli words
	them. It is read with *radix-block*(2)'s GET_HEALTH, as
	*diskhealth*(8) reads it, which judges it too.

# DIFFERENCES FROM LINUX

- Only *list* and *smart-log* are there: none of nvme-cli's other
  commands, which send the drive admin commands of their own.
- *list* shows the namespace's size, not how much of it is used, and
  no *-o json*.
- *smart-log* shows the fields above, not the temperature sensors and
  thermal management counts, and no *-o json*.

# EXIT STATUS

0 when it was done; 1 when _disk_ is no NVMe drive or cannot be read,
or for a mistake in the command line.

# SEE ALSO

*diskhealth*(8), *lsblk*(8), *radix-block*(2)
