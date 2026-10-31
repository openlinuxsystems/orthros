dmesg(1) "Dukovo" "Dukovo Manual"

; SPDX-FileCopyrightText: 2026 The Dukovo contributors
; SPDX-License-Identifier: Apache-2.0 OR MIT

# NAME

dmesg - print or clear the kernel log

# SYNOPSIS

*dmesg* [*-cdkrTtuwWx*] [*-l* _levels_]

*dmesg* *-C*

# DESCRIPTION

dmesg prints the kernel log, as util-linux's dmesg does, a record a
line: the time since boot the record was made, in seconds, and its
text. It reads the records from _/devices/kmsg_ (*radix-kernel-log*(2)),
the oldest kept first. The kernel keeps the newest records in a ring;
older ones are gone.

The log holds what the kernel says and what programs write to it; each
line a program wrote starts with its name and is of the facility its
*<*_n_*>* gave, or the user facility, the kernel's own of the kernel
facility, but for its security events, of authpriv (ADR 0211).
*log*(1) shows the same records
with the services' lines named by service, kept across boots by
*logd*(8).

The kernel log is for administrators, as Linux's is with
*kernel.dmesg_restrict* set: _/devices/kmsg_ is root's, mode 0622, so
only root reads it or clears it, and anyone may write a line to it, as
*logger*(1) does (ADR 0252). An administrator reads it with *elevate
dmesg*; dmesg tells anyone else so. *log*(1) shows everyone the lines
their own programs wrote.

# OPTIONS

*-l*, *--level* _list_
	Only the records of these levels, by commas: by name (*emerg*,
	*alert*, *crit*, *err*, *warn*, *notice*, *info*, *debug*) or
	number. _level_*+* is that level and every more urgent one,
	*+*_level_ that level and every less urgent one.

*-w*, *--follow*
	Print the records kept, then wait for new ones and print them as
	they come.

*-W*, *--follow-new*
	Wait for new records only.

*-c*, *--read-clear*
	Clear the log after printing it.

*-C*, *--clear*
	Clear the log without printing it: later readers start after the
	records it holds. It needs leave to write to _/devices/kmsg_.

*-t*, *--notime*
	No times.

*-T*, *--ctime*
	The local time of each record, worked out from the time of boot.

*-d*, *--show-delta*
	The time since the record before, too.

*-f*, *--facility* _list_
	Only the records of these facilities, by commas: *kern*, *user*,
	*mail*, *daemon*, *auth*, *syslog*, *lpr*, *news*, *uucp*, *cron*,
	*authpriv*, *ftp*, *res0* to *res3*, *local0* to *local7*.

*-r*, *--raw*
	Each record's facility and level as *<*_n_*>* before it: the
	facility times eight plus the level.

*-x*, *--decode*
	Each record's facility and level by name before it.

*-k*, *--kernel*
	Only the kernel's own records.

*-u*, *--userspace*
	Only the records programs wrote.

*-n*, *-D* and *-E* fail: Radix has no console log level to set. *-H*,
*-L*, *-P*, *--color*, *--nopager* and *--human* are taken and change
nothing.

# EXAMPLES

The errors and warnings since boot:

	dmesg -l warn+

Watch the log:

	dmesg -w

As an administrator, from a session:

	elevate dmesg

# SEE ALSO

*log*(1), *logger*(1), *logd*(8), *elevate*(8), *systools*(1), *radix-kernel-log*(2),
*radix-debug*(7), *radix-files*(7)
