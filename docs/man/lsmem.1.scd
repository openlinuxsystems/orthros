lsmem(1) "Dukovo" "Dukovo Manual"

; SPDX-FileCopyrightText: 2026 The Dukovo contributors
; SPDX-License-Identifier: Apache-2.0 OR MIT

# NAME

lsmem - list the machine's RAM in memory blocks

# SYNOPSIS

*lsmem* [_option_ ...]

# DESCRIPTION

lsmem lists the machine's RAM as util-linux's lsmem lists it on Linux:
in memory blocks, the units Linux takes memory on and off line in, each
range of consecutive blocks on a line, then a summary. It reads the
firmware's memory map, _/kernel/firmware/memmap_ (*radix-sysfs*(5)): a
block is RAM when any of it is. Blocks are of 128 MiB, or of 2 GiB on a
machine with RAM above 64 GiB, as Linux makes them on x86-64. Radix does
not take memory on or off line, so every block is online.

```
$ lsmem
RANGE                                  SIZE  STATE BLOCK
0x0000000000000000-0x000000003fffffff    1G online   0-7

Memory block size:       128M
Total online memory:       1G
Total offline memory:      0B
```

Linux's lsmem also has a REMOVABLE column, for memory that can be
unplugged.

# OPTIONS

*-a*, *--all*
	Each block on a line of its own.

*-b*, *--bytes*
	Sizes in bytes.

*-n*, *--noheadings*
	No heading line.

*--summary*[=_when_]
	The summary *only* (as without _when_), *always* (the default) or
	*never*.

# EXIT STATUS

0, or 1 if the memory map cannot be read or for an option it does not
know.

# SEE ALSO

*lscpu*(1), *lspci*(8), *free*(1), *radix-sysfs*(5), *systools*(1)
