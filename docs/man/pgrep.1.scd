pgrep(1) "Dukovo" "Dukovo Manual"

; SPDX-FileCopyrightText: 2026 The Dukovo contributors
; SPDX-License-Identifier: Apache-2.0 OR MIT

# NAME

pgrep, pkill - find or signal processes by name and other attributes

# SYNOPSIS

*pgrep* [_option_ ...] [_pattern_]

*pkill* [*-*_signal_] [_option_ ...] [_pattern_]

# DESCRIPTION

pgrep lists the numbers of the processes whose name matches _pattern_,
an extended regular expression, and that the other options
select; pkill sends them a signal, TERM unless another is named, as
Linux's pgrep and pkill (procps-ng) do. Neither ever selects itself.
Every option that selects narrows the selection: a process must match
them all. Zombies, processes that ended and wait for their parent, are
found too (*-r Z* lists them).

# OPTIONS

*-f*, *--full*
	Match the pattern against the whole command line, not the name. A
	process without arguments has *[*_name_*]* for one, and a zombie
	*[*_name_*] <defunct>*, as *ps*(1) shows them.

*-x*, *--exact*
	The pattern must match all of the name (or command line).

*-i*, *--ignore-case*
	Match without regard to case.

*-v*, *--inverse*
	Select the processes that do not match.

*-n*, *--newest*; *-o*, *--oldest*
	Only the newest, or the oldest, of those selected.

*-P*, *--parent* _list_
	Only the children of these processes.

*-g*, *--pgroup* _list_
	Only these process groups; *0* is pgrep's own.

*-s*, *--session* _list_
	Only these sessions; *0* is pgrep's own.

*-t*, *--terminal* _list_
	Only the processes with these terminals (*pts/0*).

*-u*, *-U* _list_
	Only these users' processes, by name or number.

*-r*, *--runstates* _letters_
	Only the processes in these states, as *ps*(1) shows them (*R*,
	*S*, *D*, *T*, *Z*).

*-O*, *--older* _seconds_
	Only the processes older than this.

*-c*, *--count*
	Print how many processes were selected instead of their numbers.

pgrep only:

*-l*, *--list-name*
	Each number with the process's name.

*-a*, *--list-full*
	Each number with the process's command line.

*-d*, *--delimiter* _text_
	Put _text_ between the numbers, not a newline.

pkill only:

*-*_signal_, *--signal* _signal_
	The signal to send, by number or name, as for *kill*(1).

*-e*, *--echo*
	Say which processes were sent the signal.

# EXIT STATUS

0 if a process was selected (for pkill, and sent the signal), 1 if none
was, 2 for a mistake in the command line, 3 if _/proc_ could not be
read.

# EXAMPLES

The numbers and command lines of every nukesh:

	pgrep -a nukesh

Ask every process of terminal pts/1 to hang up:

	pkill -HUP -t pts/1

# SEE ALSO

*kill*(1), *ps*(1), *systools*(1)
