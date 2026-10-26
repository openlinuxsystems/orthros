vmstat(8) "Dukovo" "Dukovo Manual"

; SPDX-FileCopyrightText: 2026 The Dukovo contributors
; SPDX-License-Identifier: Apache-2.0 OR MIT

# NAME

vmstat - memory, CPU and disk activity

# SYNOPSIS

*vmstat* [_option_ ...] [_delay_ [_count_]]

# DESCRIPTION

vmstat shows, as Linux's vmstat (procps-ng) does, a line of the
system's activity: under *procs*, the threads ready to run (*r*) and
waiting for a disk (*b*); under *memory*, the swap used (*swpd*), free
memory, *buff* (0) and *cache* (the page cache), in KiB; under *swap*,
pages swapped in and out (*si*, *so*); under *io*, the blocks of 1 KiB
read from and written to disks (*bi*, *bo*); under *system*, interrupts
(*in*) and context switches (*cs*); and under *cpu*, the percentage of
CPU time in user mode (*us*), the kernel (*sy*), idle (*id*), waiting
for a disk (*wa*), stolen by a hypervisor (*st*) and running guests
(*gu*).

The first line is the averages since the system started; with a
_delay_ in seconds, a line follows every _delay_ seconds of what
happened since the line before, _count_ lines in all or until
interrupted.

It reads _/processes/stat_ and _/processes/meminfo_ (*radix-proc*(5)). Radix has
no swap yet and does not count interrupts or the time spent waiting for
disks or stolen, so *swpd*, *si*, *so*, *in*, *wa*, *st* and *gu* are 0.

# OPTIONS

*-w*, *--wide*
	Wider columns.

*-n*, *--one-header*
	The header once, not every screenful.

*-t*, *--timestamp*
	The time of each line, at its end.

*-S* _unit_
	Memory in *k* (1000 bytes), *K* (1024, the default), *m* (10^6) or
	*M* (2^20).

*-s*, *--stats*
	A table of totals since boot, one a line: memory, swap, CPU ticks
	(hundredths of a second), the sectors read and written, context
	switches and the time of boot.

*-d*, *--disk*
	What the disks have done since boot: reads and writes and their
	sectors, for all disks together, as *all*.

*-a* and *-y* are taken and change nothing.

# SEE ALSO

*free*(1), *top*(1), *pidstat*(1), *systools*(1), *radix-proc*(5)
