free(1) "Dukovo" "Dukovo Manual"

; SPDX-FileCopyrightText: 2026 The Dukovo contributors
; SPDX-License-Identifier: Apache-2.0 OR MIT

# NAME

free - memory in use and free

# SYNOPSIS

*free* [_option_ ...]

# DESCRIPTION

free shows the memory the kernel manages, as Linux's free (procps-ng)
does, from _/proc/meminfo_ (*radix-proc*(5)): its *total*, how much is
*used* by programs and the kernel, how much is *free*, *shared* (always
0), *buff/cache* (the page cache: files' data held in memory) and
*available*: what programs could have, free memory with the page cache,
which the kernel gives back when memory runs low. A second line, Swap,
is 0 until Radix has swap. Sizes are in KiB unless an option says
otherwise.

# OPTIONS

*-b*, *--bytes*; *-k*, *--kibi*; *-m*, *--mebi*; *-g*, *--gibi*; *--tebi*; *--pebi*
	Sizes in bytes, KiB, MiB, GiB, TiB or PiB.

*--kilo*, *--mega*, *--giga*, *--tera*
	Sizes in kB, MB, GB or TB, powers of 1000.

*--si*
	Powers of 1000 for the units of the options above and *-h*.

*-h*, *--human*
	Each size with a unit that keeps it short, as procps writes it: in
	the first unit where it fits four characters besides the *i* of
	*Ki*, *Mi*, *Gi*..., with one decimal while that fits and whole
	after, so 15.7 GiB reads *15Gi*.

*-w*, *--wide*
	The page cache in a column of its own, *cache*, with *buffers* 0.

*-l*, *--lohi*
	Low and High lines too, as Linux shows them on 32-bit machines:
	all the memory is low memory.

*-t*, *--total*
	A Total line: memory and swap together.

*-s*, *--seconds* _seconds_
	Show it again every _seconds_, until interrupted.

*-c*, *--count* _n_
	Show it _n_ times (every second, unless *-s* says otherwise).

*-v* and *-L* are taken and change nothing.

# SEE ALSO

*vmstat*(8), *top*(1), *systools*(1), *radix-proc*(5)
