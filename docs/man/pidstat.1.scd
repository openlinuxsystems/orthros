pidstat(1) "Dukovo" "Dukovo Manual"

; SPDX-FileCopyrightText: 2026 The Dukovo contributors
; SPDX-License-Identifier: Apache-2.0 OR MIT

# NAME

pidstat - report what processes use

# SYNOPSIS

*pidstat* [*-dhHIlRrtuvw*] [*-p* _list_ | *ALL* | *SELF*] [*-C* _pattern_] [*-G* _pattern_] [*-U* [_user_]] [_interval_ [_count_]]

# DESCRIPTION

pidstat reports, a line a process, what each one uses, as sysstat's
pidstat does and in its layout, from _/proc_ (*radix-proc*(5)). Without
an _interval_ (or with 0) it reports each one's use since it started,
where sysstat's pidstat reports the use since the machine started. With
one it reports the use in each _interval_ seconds, _count_ times or
until interrupted (SIGINT, as with Ctrl+C), then the averages from the
first report to the last: of the sizes and counts of *-r* and *-v*,
their mean.

Each report is headed with the time of the sample before it. Without
*-p*, only the processes that used something in a report are shown,
and for *-v* and *-R* those whose numbers changed, as sysstat's pidstat
does. For one process (*-p* with one number, or *SELF*) in one report
the heading comes once, and the reports and the averages follow it
line by line.

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

*-v*
	The process's *threads* and its file descriptors, *fd-nr*.

*-R*
	Scheduling: the real-time priority *prio* and the *policy*:
	*NORMAL*, *FIFO*, *RR*, *BATCH* or *IDLE*.

# OPTIONS

*-p* _list_
	The processes with these numbers, by commas, shown whether or not
	they did anything; *ALL* every process, *SELF* pidstat itself.

*-C* _pattern_
	The processes whose names match the regular expression _pattern_;
	with *-t*, of their threads those whose names match, and the threads
	of other processes whose names match, shown as
	*(*_process_*)\_\_*_thread_.

*-G* _pattern_
	The processes whose names match the regular expression _pattern_,
	with all their threads with *-t*.

*-U* [_user_]
	User names in a *USER* column, not numbers; given a _user_ (a name
	that is not a number, which would be the _interval_), only that
	user's processes.

*-t*
	A line for each thread too, after its process's, with the process's
	number in the *TGID* column and the thread's in the *TID* column.

*-h*
	The reports asked for side by side, a line a process, under a
	heading of their own each time, and no averages.

*-l*
	The whole command line as the Command, not only the name.

*-I*
	*%CPU* as a share of all the CPUs, not of one.

*-H*
	Times as seconds since the epoch.

*-s* is taken, but adds no report: Radix does not tell the sizes of
stacks yet.

Until users come, in Phase 13 of the roadmap, every process is root's:
the *UID* column is 0, and *-U* names root.

# EXAMPLES

What is using the CPU, each second, five times:

	pidstat 1 5

The memory and disk use of one process and its threads:

	pidstat -r -d -t -p 42 2

Each second, the CPU of the processes called nukesh, by command line:

	pidstat -l -G '^nukesh$' 1

# SEE ALSO

*top*(1), *ps*(1), *vmstat*(8), *systools*(1), *radix-proc*(5)
