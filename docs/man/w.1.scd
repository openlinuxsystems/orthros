w(1) "Dukovo" "Dukovo Manual"

; SPDX-FileCopyrightText: 2026 The Dukovo contributors
; SPDX-License-Identifier: Apache-2.0 OR MIT

# NAME

w - who is logged in, and what they are doing

# SYNOPSIS

*w* [*-hsf*] [_user_]

# DESCRIPTION

w prints *uptime*(1)'s line, then a line per login session, as Linux's
w (procps-ng) does: the user, the terminal (TTY), when the session began
(LOGIN@), how long since its terminal was read or written (IDLE), the
CPU time of all the processes on its terminal (JCPU) and of the one it
runs now, the newest of them (PCPU), and that one's command line (WHAT).
Given a _user_, a name or a number, only that user's sessions.

Sessions are found as *who*(1) finds them.

# OPTIONS

*-h*, *--no-header*
	Neither the uptime line nor the column headers.

*-s*, *--short*
	Leave out LOGIN@, JCPU and PCPU.

*-f*, *--from*
	Leave out the FROM column (or, given twice, show it), which says
	*-*: no session comes from another machine yet.

*-u*, *-i*, *-o* and *-p* are taken and change nothing.

# SEE ALSO

*who*(1), *uptime*(1), *ps*(1), *systools*(1)
