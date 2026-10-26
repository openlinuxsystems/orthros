uptime(1) "Dukovo" "Dukovo Manual"

; SPDX-FileCopyrightText: 2026 The Dukovo contributors
; SPDX-License-Identifier: Apache-2.0 OR MIT

# NAME

uptime - how long the system has been up

# SYNOPSIS

*uptime* [*-p* | *-s*]

# DESCRIPTION

uptime prints a line, as Linux's uptime (procps-ng) does: the time, how
long the system has been up, the users logged in (as *who*(1) counts
them) and the load averages over the last 1, 5 and 15 minutes (the
threads running, ready to run or waiting for a disk, as
_/processes/loadavg_ gives them, *radix-proc*(5)).

# OPTIONS

*-p*, *--pretty*
	Only how long it has been up, in words: *up 2 hours, 5 minutes*.

*-s*, *--since*
	When it started, as _yyyy_-_mm_-_dd_ _hh_:_mm_:_ss_.

*--help*
	A usage line.

# SEE ALSO

*w*(1), *top*(1), *systools*(1), *radix-proc*(5)
