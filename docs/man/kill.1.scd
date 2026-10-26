kill(1) "Dukovo" "Dukovo Manual"

; SPDX-FileCopyrightText: 2026 The Dukovo contributors
; SPDX-License-Identifier: Apache-2.0 OR MIT

# NAME

kill - send a signal to processes

# SYNOPSIS

*kill* [*-s* _signal_ | *-*_signal_] _pid_ ...

*kill -l* [_signal_ | _status_]

*kill -L*

# DESCRIPTION

kill sends a signal, TERM unless another is named, to each process
named by its number. A negative number, after the signal or after *--*,
names a process group. A signal
is named by its number or its name, with or without *SIG*, in any case:
*-9*, *-KILL*, *-s kill* and *-s SIGKILL* are the same.

Nukesh has its own *kill* built in, which also takes jobs (*%1*);
_/system/commands/kill_ is this one.

# OPTIONS

*-s*, *--signal*, *-n* _signal_
	The signal to send.

*-*_signal_
	The same.

*-l*, *--list* [_signal_ | _status_]
	List the signals' names; or give the name of a signal's number, or
	of the signal that ended a process with exit status _status_ (above
	128); or the number of a signal's name.

*-L*, *--table*
	List the signals' numbers and names as a table.

*-h*, *--help*
	A usage line.

*-a*, *--all*, *-p* and *--pid* are taken and change nothing.

# EXIT STATUS

0 if every process was sent the signal; 1 if one could not be, or for
a mistake in the command line.

# EXAMPLES

Ask process 12 to end, then make it:

	kill 12
	kill -9 12

Stop every process of group 30:

	kill -STOP -- -30

The signal that ended a command whose status was 137:

	kill -l 137

# SEE ALSO

*pkill*(1), *ps*(1), *nukesh*(1), *systools*(1)
