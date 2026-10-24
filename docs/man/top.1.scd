top(1) "Dukovo" "Dukovo Manual"

; SPDX-FileCopyrightText: 2026 The Dukovo contributors
; SPDX-License-Identifier: Apache-2.0 OR MIT

# NAME

top - the processes using the CPU most, refreshed

# SYNOPSIS

*top* [*-bcHi1*] [*-d* _seconds_] [*-n* _frames_] [*-o* _field_] [*-p* _list_] [*-u* _user_] [*-w* [_columns_]]

# DESCRIPTION

top shows the system's state and its busiest processes on the whole
terminal, as Linux's top (procps-ng) does, and shows it again every
three seconds until *q* is pressed. Its first lines are the time, how
long the system has been up, the users and the load averages; the
tasks by state; the CPU time in user mode, in the kernel and idle; and
the memory and swap in MiB. Below them is a line per process: PID,
USER, PR, NI, VIRT, RES, SHR, S, %CPU, %MEM, TIME+ and COMMAND.

%CPU is of one CPU, over the time since the frame before, so a process
that keeps two CPUs busy shows 200. It reads _/proc_
(*radix-proc*(5)). Radix has no swap yet, and SHR is always 0.

In batch mode (*-b*) top writes frame after frame to its standard
output, for a log or a pipe, and reads no keys.

# OPTIONS

*-b*
	Batch mode.

*-n* _frames_
	Stop after this many frames.

*-d* _seconds_
	The time between frames, which may have a fraction (*0.5*).

*-p* _list_
	Only these processes.

*-u*, *-U* _user_
	Only this user's processes.

*-H*
	A line per thread.

*-o* _field_
	Sort by _field_: *%CPU* (the default), *%MEM*, *TIME+*, *PID* or
	*COMMAND*, highest first (for *COMMAND*, the names last in
	alphabetical order); with *-* before it, lowest first.

*-c*
	The command with its arguments, not only its name.

*-1*
	A CPU line for each CPU.

*-i*
	Leave out the processes that used no CPU since the frame before.

*-w* [_columns_]
	Lines this wide in batch mode (512 if no number is joined to it).

*-h*
	A usage line.

*-v*
	top's version.

*-s*, *-S*, *-E* and *-e* are taken and change nothing.

# KEYS

*q*, ^C, ^D
	Quit.

*P*, *M*, *T*, *N*
	Sort by %CPU, %MEM, TIME+ or PID.

*R*
	Reverse the order.

*c*
	The command with or without its arguments.

*H*
	Threads or processes.

*1*
	A line for each CPU, or one for all.

*i*
	Show or leave out the idle processes.

*h*, *?*
	Help.

# EXAMPLES

Five frames a second apart, to a file:

	top -b -n 5 -d 1 > top.log

# SEE ALSO

*ps*(1), *pidstat*(1), *vmstat*(8), *uptime*(1), *systools*(1)
