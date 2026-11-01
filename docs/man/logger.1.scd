logger(1) "Dukovo" "Dukovo Manual"

; SPDX-FileCopyrightText: 2026 The Dukovo contributors
; SPDX-License-Identifier: Apache-2.0 OR MIT

# NAME

logger - write a line to the system's log

# SYNOPSIS

*logger* [*-ise*] [*-t* _tag_] [*-p* _priority_] [*-f* _file_] [_message_...]

# DESCRIPTION

logger writes a message to the system's log, as util-linux's logger
does, for scripts: the _message_ words joined by blanks, or else each
line of _file_ or of the standard input as a message of its own.

Each message is a line written to _/devices/kmsg_
(*radix-kernel-log*(2)), which anyone may write to (ADR 0252). The
kernel starts the record with the name of the thread that wrote it, the
tag, and marks it with the process and the user that process acts for.
*logd*(8) keeps it, and *log*(1) shows it back to its writer, who sees
the records of their own programs, and to root:

```
logger -t backup 'All 3 disks are copied.'
log backup
```

prints

```
Oct 31 09:12:44 dukovo backup[61]: All 3 disks are copied.
```

The kernel keeps the first 256 bytes of a line, the *<*_n_*>* in front
included; a control character in a message is written as a blank, so a
message is always one record.

# OPTIONS

*-t*, *--tag* _tag_
	The name the record goes by, the first 15 bytes of it, as the
	kernel keeps a thread's name; the user's name by default. A tag
	is a claim: a record whose tag is a service's name is still the
	record of a program, with its process number after it, and never
	one of the service's.

*-p*, *--priority* _priority_
	_facility_*.*_level_, a level alone of the user facility, or a
	number: *user.notice* by default. The facilities are syslog(3)'s
	(*user*, *daemon*, *auth*, *authpriv*, *local0* to *local7* and
	the rest) but the kernel's own, *kern*; the levels are *emerg*,
	*alert*, *crit*, *err*, *warning*, *notice*, *info* and *debug*.
	The kernel keeps four of them: *emerg* to *err* as *err*, and
	*notice* as *info*, which is how *dmesg*(1) and *netlog*(8) give
	them back. A program's record of the auth or authpriv facility is not a
	security event, which only the kernel and Spark write (*logd*(8)).

*-s*, *--stderr*
	Write each message to the standard error too.

*-f*, *--file* _file_
	Log each line of _file_.

*-e*, *--skip-empty*
	Do not log empty lines.

*--no-act*
	Write nothing to the log; with *-s*, only to the standard error.

*-i*, *--id*[=_id_]
	Taken and changes nothing: the kernel marks every record with its
	process.

*-n*, *--server*, *-P*, *--port*, *-u*, *--socket*, *-d*, *--udp*, *-T*, *--tcp*
	Refused: logger writes to this machine's log, and Dukovo has no
	syslog server.

# EXIT STATUS

0, or 1 if the log or _file_ cannot be written or read, or the command
line is wrong.

# SEE ALSO

*log*(1), *dmesg*(1), *logd*(8), *notify-send*(1), *systools*(1),
*radix-kernel-log*(2)
