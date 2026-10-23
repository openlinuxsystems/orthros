dmesg(1) "Dukovo" "Dukovo Manual"

; SPDX-FileCopyrightText: 2026 The Dukovo contributors
; SPDX-License-Identifier: Apache-2.0 OR MIT

# NAME

dmesg - print or clear the kernel log

# SYNOPSIS

*dmesg* [*-cdrTtwWx*] [*-l* _levels_]

*dmesg* *-C*

# DESCRIPTION

dmesg prints the kernel log, as util-linux's dmesg does, a record a
line: the time since boot the record was made, in seconds, and its
text. It reads the records from _/dev/kmsg_ (*radix-kernel-log*(2)),
the oldest kept first. The kernel keeps the newest records in a ring;
older ones are gone.

The log holds what the kernel says and what programs write to it
before their own logs are up; each line a program wrote starts with its
name. Every record is of the kernel's facility.

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
	records it holds. It needs leave to write to _/dev/kmsg_.

*-t*, *--notime*
	No times.

*-T*, *--ctime*
	The local time of each record, worked out from the time of boot.

*-d*, *--show-delta*
	The time since the record before, too.

*-r*, *--raw*
	Each record's level as *<*_n_*>* before it.

*-x*, *--decode*
	Each record's facility and level by name before it.

*-u* prints nothing, as Radix keeps no userspace records in the
kernel's log. *-n*, *-D* and *-E* fail: Radix has no console log level
to set. *-k*, *-H*, *-L*, *-P*, *--kernel*, *--color*, *--nopager* and
*--human* are taken and change nothing.

# EXAMPLES

The errors and warnings since boot:

	dmesg -l warn+

Watch the log:

	dmesg -w

# SEE ALSO

*systools*(1), *radix-kernel-log*(2), *radix-debug*(7),
*radix-files*(7)
