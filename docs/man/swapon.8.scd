swapon(8) "Dukovo" "Dukovo Manual"

; SPDX-FileCopyrightText: 2026 The Dukovo contributors
; SPDX-License-Identifier: Apache-2.0 OR MIT

# NAME

swapon - turn swap areas on, and list them

# SYNOPSIS

*swapon* [*-p* _priority_] [*-v*] [*-e*] _device_...

*swapon* [*--show*[=_columns_]] [*--noheadings*] [*--raw*] [*--bytes*]

*swapon -s*

# DESCRIPTION

swapon turns on the swap area *mkswap*(8) made on each _device_, a disk
or partition in */devices*, as util-linux's *swapon* does: from then on
the kernel moves programs' memory out to it when memory runs short, and
reads each page back when it is used (ADR 0152). While the area is on,
the device cannot be written or mounted.

Areas are filled highest priority first, and an area of equal priority
after the one turned on before it. Without *-p*, an area gets the next
of Linux's default priorities: -2, then -3, and on.

Without a _device_, swapon lists the areas on, as *--show* does.

# OPTIONS

*-p* _priority_, *--priority* _priority_
	Give the areas this priority, from 0 to 32767.

*-e*, *--ifexists*
	Pass over a _device_ that is not there, saying nothing.

*-v*, *--verbose*
	Say what is turned on.

*--show*[=_columns_]
	List the areas on, with the columns given, separated by commas:
	*NAME*, *TYPE*, *SIZE*, *USED*, *PRIO* and *UUID* and *LABEL*. The
	first five are listed without _columns_.

*--noheadings*
	List them without the line of headings.

*--raw*
	List them with one space between columns, without lining them up.

*--bytes*
	Give sizes in bytes, not with a unit.

*-s*, *--summary*
	Print */processes/swaps*, as Linux's prints */proc/swaps*.

# DIFFERENCES FROM LINUX

- There is no */etc/fstab*: *-a* (turn on every area it lists) and
  *-T* are not there, and an area is named by its device only, not by
  *LABEL=* or *UUID=*.
- Swap files are not taken yet, only disks and partitions.
- *-d* (discard) fails with _Operation not supported_.
- At most 8 areas are on at once; Linux takes up to 32.

# EXIT STATUS

0 when every area was turned on; 1 when one was not, after the ones
before it, and for a mistake in the command line.

# SEE ALSO

*swapoff*(8), *mkswap*(8), *free*(1), *vmstat*(8), *radix-block*(2),
*radix-proc*(5)
