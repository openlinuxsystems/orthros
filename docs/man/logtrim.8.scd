logtrim(8) "Dukovo" "Dukovo Manual"

; SPDX-FileCopyrightText: 2026 The Dukovo contributors
; SPDX-License-Identifier: Apache-2.0 OR MIT

# NAME

logtrim - trim the system's log

# SYNOPSIS

*logtrim* [*-n*] [*-s* _size_] [*-d* _days_]

# DESCRIPTION

logtrim asks *logd*(8) to trim the system's log now, and says which day
files it removed and how much room that made (ADR 0211).

logd trims the log by itself, as _/system/settings/log_ says (*log*(5)):
when it starts, when a day begins, after each 64 MiB it writes, and
when its settings are read again. Without options, logtrim asks for the
same trim now. With *--size* or *--days*, it trims the log, not the
audit log, to limits of its own, once, as journalctl's *--vacuum-size*
and *--vacuum-time* do; the audit log keeps to its own setting.

logd removes whole day files, oldest first, and never the one it is
writing or today's. Each removal goes into the audit log, with the
process that asked for it.

# OPTIONS

*-n*, *--dry-run*
	Say what would be removed, and remove nothing.

*-s*, *--size* _size_
	Remove the oldest of the log's day files while they take more
	than _size_: bytes, or with *K*, *M*, *G* or *T* after them (KiB
	and so on), or *KB*, *MB*, *GB*, *TB* (powers of 1000).

*-d*, *--days* _days_
	Remove the log's day files older than _days_ days.

# EXIT STATUS

0, or 1 if logd is not running or refuses, or the command line is
wrong.

# EXAMPLES

```
logtrim -n -s 500M
logtrim --days 30
```

# SEE ALSO

*logd*(8), *log*(5), *log*(1), *journalctl*(1), *systools*(1)
