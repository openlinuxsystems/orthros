pidstat(1) "Dukovo" "Dukovo Manual"

; SPDX-FileCopyrightText: 2026 The Dukovo contributors
; SPDX-License-Identifier: Apache-2.0 OR MIT

# NAME

pidstat - report what processes use

# SYNOPSIS

*pidstat* [*-druwth*] [*-p* _list_ | *ALL* | *SELF*] [*-C* _pattern_] [_interval_ [_count_]]

# DESCRIPTION

pidstat reports, a line a process, what each one uses, as sysstat's
pidstat does, from _/proc_ (*radix-proc*(5)). Without an _interval_ it
reports each one's use since it started. With one it reports the use
in each _interval_ seconds, _count_ times or until interrupted, then the
averages from the first report to the last.

Without *-p*, only the processes that used something in a report are
shown, as sysstat's pidstat does.

# REPORTS

*-u*
	CPU, the report by default: *%usr* and *%system*, the share of one
	CPU used in the program and in the kernel for it, their sum *%CPU*,
	and the *CPU* its first thread last ran on. *%guest* and *%wait*
	are 0: Radix runs no guests and does not count the time threads
	wait to run.

*-r*
	Memory: minor and major page faults a second, the virtual and
	resident sizes in KiB, and the share of memory resident.

*-d*
	I/O: KiB read and written a second. Radix counts what a process
	reads and writes from files, pipes, sockets and terminals alike, not
	only what goes to a disk as Linux's pidstat counts it. *kB_ccwr/s*
	and *iodelay* are 0: Radix does not count cancelled writes or the
	time spent waiting for the disk yet.

*-w*
	Context switches a second: *cswch/s* when a thread waited,
	*nvcswch/s* when it was preempted.

# OPTIONS

*-p* _list_
	The processes with these numbers, by commas, shown whether or not
	they did anything; *ALL* every process, *SELF* pidstat itself.

*-C* _pattern_
	The processes whose names match the regular expression _pattern_.

*-t*
	A line for each thread too, after its process's, with the thread's
	number in the *TID* column.

*-h*
	The reports asked for side by side, a line a process, not one after
	another.

*-l*, *-I*, *-H*, *-U*, *-G*, *-R*, *-s* and *-v* are taken and change
nothing.

Until users come, in Phase 13 of the roadmap, every process is root's:
the *UID* column is 0.

# EXAMPLES

What is using the CPU, each second, five times:

	pidstat 1 5

The memory and disk use of one process and its threads:

	pidstat -r -d -t -p 42 2

# SEE ALSO

*top*(1), *ps*(1), *vmstat*(8), *systools*(1), *radix-proc*(5)
