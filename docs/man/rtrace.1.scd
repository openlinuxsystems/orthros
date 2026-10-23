rtrace(1) "Dukovo" "Dukovo Manual"

; SPDX-FileCopyrightText: 2026 The Dukovo contributors
; SPDX-License-Identifier: Apache-2.0 OR MIT

# NAME

rtrace - trace a process's system calls and capability messages

# SYNOPSIS

*rtrace* [*-cCfnqrTtx*] [*-e* [*trace=*]_name_,...] [*-o* _file_] [*-s* _size_] _command_ [_argument_ ...]

*rtrace* [_option_ ...] *-p* _pid_[,_pid_...]

# DESCRIPTION

rtrace prints what a process asks of the kernel, a line for each call,
as strace does on Linux. It runs _command_ and traces it, or traces the
running processes *-p* names until they end or rtrace is interrupted.
It reads the records of a trace the kernel keeps (*radix-trace*(2),
ADR 0095), so the traced process neither stops nor knows.

Each line is a call: its name, its arguments and its result. A system
call has its own name (*cap_call*, *yield*); a message to a capability,
which is most of what a program asks, is named by the interface and
method that the label names (*file.read*, *process.wait*), with the
capability's slot first. Arguments are shown as the interface
describes them: numbers, flags and kinds by name, slots by name or
number, and the bytes a call is given or gives back (a path, the data
of a write or a read) as a quoted string. A call that failed shows
*-1*, its error's name and its text.

Records are written as calls return, so a call that waits (*recv*,
*wait*) appears when it ends. A signal the process takes shows as
*--- SIGINT (handled) ---*, a child it forks as *--- child* _n_
*started ---*, and its end as *+++ exited with* _n_ *+++* or *+++
killed by* _signal_ *+++*. A record the kernel had no room for, when
rtrace did not read fast enough, shows as *--- some records were lost
here ---*.

Tracing another process needs what *kill*(1) needs. A process has one
trace at a time: tracing one already traced (by rtrace or a debugger)
fails, and rtrace cannot trace itself.

# OPTIONS

*-p* _pid_[,_pid_...]
	Trace these running processes instead of a command.

*-f*
	Trace the children the processes fork too. Each line then starts
	with the thread's number.

*-e* [*trace=*]_name_,...
	Show only these calls, each by its whole name (*file.read*), its
	method (*read*) or its interface (*file*).

*-c*
	Count the calls instead of showing them, and print a table of
	each one's time, calls and errors at the end, as strace does.

*-C*
	Show the calls and the table.

*-T*
	The time each call took, after it, as *<0.000012>*.

*-t*
	The time of day before each line; *-tt* with microseconds;
	*-ttt* as seconds since 1970.

*-r*
	The time since the line before, before each line.

*-s* _size_
	Show at most _size_ bytes of each string (32 by default); a cut
	one ends in *...*. The kernel keeps at most 256 bytes of each.

*-x*
	Show the bytes that are not printable in hexadecimal; *-xx* every
	byte.

*-n*
	Do not ask for the bytes of calls: their addresses are shown.

*-o* _file_
	Write to _file_, not standard error.

*-q*
	Do not say when processes are attached or detached, or how they
	end.

*-h*
	A usage line.

*-V*
	Its version.

# EXIT STATUS

With a _command_, the command's status, or 128 and the signal's number
if a signal ended it; with *-p*, 0.

# EXAMPLES

What files a program opens:

	rtrace -e open ls /

Where a server spends its time:

	rtrace -c -p 42

# SEE ALSO

*lsof*(8), *kill*(1), *systools*(1), *radix-trace*(2),
*radix-debug*(7)
