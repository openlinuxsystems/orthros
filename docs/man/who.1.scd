who(1) "Dukovo" "Dukovo Manual"

; SPDX-FileCopyrightText: 2026 The Dukovo contributors
; SPDX-License-Identifier: Apache-2.0 OR MIT

# NAME

who, users - who is logged in

# SYNOPSIS

*who* [*-abHmqu*]

*users*

# DESCRIPTION

who prints a line per login session: the user, the terminal and when
the session began, as GNU who does. users prints the users of the
sessions on one line.

Radix keeps no utmp file. A session is a session leader with a
controlling terminal, as _/processes_ shows it (*radix-proc*(5)), which began
when its leader started: a login shell on a terminal. Its user is the
one its leader acts for, who signed in on that terminal (*login*(1)).
users lists them sorted, once per session.

# OPTIONS

*-H*, *--heading*
	A heading line first.

*-b*, *--boot*
	Only the time the system started.

*-a*, *--all*
	The time the system started, then the sessions with *-u*'s
	columns.

*-u*, *--users*
	Each session's idle time (*.* if it was used in the last minute)
	and its leader's number.

*-q*, *--count*
	The users on one line, then their number.

*-m*
	Only the session of who's own terminal.

*-s*, *-T*, *-w*, *-l*, *-p*, *-r*, *-d* and *-t* are taken and change
nothing.

# SEE ALSO

*w*(1), *uptime*(1), *last*(1), *systools*(1)
