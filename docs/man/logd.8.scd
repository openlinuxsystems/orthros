logd(8) "Dukovo" "Dukovo Manual"

; SPDX-FileCopyrightText: 2026 The Dukovo contributors
; SPDX-License-Identifier: Apache-2.0 OR MIT

# NAME

logd - keep the system's log

# SYNOPSIS

*logd* [*--directory* _dir_]

# DESCRIPTION

logd keeps the system's log and its audit log (ADRs 0210 and 0211).
Spark runs it as the *logd* service, before the others.

Everything goes to the kernel log (*radix-kernel-log*(2)): the kernel's
own records, the lines programs write to it through _/devices/kmsg_ or
the kernel log capability, and the lines the services write to their
standard output and error, which Spark copies there after each
service's name (*spark*(8)). The kernel keeps only the newest records,
in a ring. logd reads each record from _/devices/kmsg_ as it comes and
appends it to the log, a line of JSON in the file of its day, in UTC:

```
/system/state/log/2026-10-30.jsonl
```

so that the log outlives the ring and the boot. Each record says when
it was made, in which boot, its number in that boot's kernel log, its
level, who wrote it and what:

```
{"time":"2026-10-30T21:25:01.123456Z","boot":"6c1f...","seq":412,
 "level":"warning","from":"program","name":"nukesh","process":57,
 "text":"..."}
```

(one line). *time* is RFC 3339 in UTC, to the microsecond; *boot* a
number logd makes at its first start in each boot, 32 hexadecimal
digits, kept in _/system/live/log/boot_; *level* is *error*,
*warning*, *info* or *debug*; *from* is *kernel*, *service* or
*program*, and *name* the service's name, the program's, or *kernel*.
*process* is the number of the process that wrote a program's record,
and *audit* is there, *true*, for a security event.

Who wrote a record is what the kernel says, not what the record says:
the kernel puts the number of the writing process on each record a
program writes, and a record of Spark's process, number 1, whose text
starts with a name and *:* is that service's line. A program that
writes a line with a service's name in front of it is still that
program. Spark's lines of the program it runs from the kernel command
line (*--posix*) are named after that program as a service's are.

A security event is a record of the authpriv or auth facility that the
kernel or Spark wrote: a service writes one with *<86>* in front of a
line of its output. Spark's answers to requests that change something,
the power button, the kernel log cleared, records lost and the log
trimmed are security events now; sign-ins and permissions will be.
logd keeps each one in the audit log too, the file of its day in
_/system/state/log/audit_, which it keeps for longer. The same facility
in a line a program writes itself is only a line of the log.

logd trims the log and the audit log as _/system/settings/log_ says
(*log*(5)): 90 days of the log, a year of the audit log, and at most
4 GiB or a tenth of their file system together, removing the oldest day
files first but never today's or one it is writing. It trims when it
starts, when a day begins, after each 64 MiB it writes, when it reads
its settings again (SIGHUP, which *service logd reload* sends) and when
*logtrim*(8) asks. Each removal is a security event.

logd flushes what it wrote to the disk 5 seconds after the first record
that is not on it yet, and when it is stopped. Started again, it goes
on from the record after the last one of this boot that it kept, so
none is kept twice. If the ring lost records before logd read them, it
says how many in a record of its own.

Programs read the log through logd, not from the files: *log*(1) and
*journalctl*(1) send a request, a line of JSON, to the socket
_/system/live/log/query_, and logd answers with the records that match,
a line of JSON each, then closes the connection, or keeps sending new
ones to a follower. It answers 32 requests at a time and refuses more.

Until Dukovo has users every program can read the log; once it has
them, only administrators can.

# OPTIONS

*--directory* _dir_
	Keep the log in _dir_, not _/system/state/log_.

# FILES

_/system/state/log/YYYY-MM-DD.jsonl_
	The records of each day, in UTC.

_/system/state/log/audit/YYYY-MM-DD.jsonl_
	The security events of each day.

_/system/settings/log_
	How much logd keeps (*log*(5)).

_/system/live/log/query_
	The socket logd answers on.

_/system/live/log/boot_
	This boot's number.

_/system/live/log/writing_
	The day file logd writes to, for logd started again.

# EXIT STATUS

As a service, logd does not exit but when it is stopped, with 0; it
exits with 1 if it cannot start.

# SEE ALSO

*log*(1), *journalctl*(1), *logtrim*(8), *log*(5), *dmesg*(1),
*spark*(8), *service*(8), *radix-kernel-log*(2), *systools*(1)
