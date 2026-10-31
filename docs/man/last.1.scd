last(1) "Dukovo" "Dukovo Manual"

; SPDX-FileCopyrightText: 2026 The Dukovo contributors
; SPDX-License-Identifier: Apache-2.0 OR MIT

# NAME

last, lastb - show the sign-ins, and the wrong passwords typed at them

# SYNOPSIS

*last* [_option_...] [_name_ | _terminal_...]

*lastb* [_option_...] [_name_ | _terminal_...]

# DESCRIPTION

last shows who signed in, where, when and for how long, newest first,
and when the computer started, from the sign-in records *authd*(8)
keeps (ADR 0254), as util-linux's last shows Linux's wtmp file. lastb
shows the wrong passwords typed at a sign-in, as util-linux's lastb
shows btmp.

Each line has the account's name, the terminal, where from (empty at
the computer itself), when the session began, when it ended and how
long it lasted, as *(HH:MM)* or *(DAYS+HH:MM)*. A session that runs
ends in *still logged in*; one the computer's stop ended, in *down*;
one that ran when the computer started again without stopping, in
*crash*; and one whose first process is gone with no sign-out seen, in
*gone - no logout*. Each start of the computer is a line of the user
*reboot*, on *system boot*, with the kernel's release; with *-x*, each
stop is one of *shutdown*, on *system down*.

With names or terminals, only their lines show; *last reboot* shows
the starts alone.

Everyone reads their own sign-ins and the computer's starts and stops,
and gets a hint on the standard error saying so. Root reads everyone's,
and lastb the names typed that are no account's too, so an
administrator runs *elevate last*.

# OPTIONS

*-n* _number_, *--limit* _number_, *-*_number_
	Show this many lines at most.

*-F*, *--fulltimes*
	Show whole times and dates, for the start and the end.

*--time-format* _format_
	*notime*: no times, the length only; *short*, the default;
	*full*, as *-F*; *iso*: ISO 8601, as _2026-10-31T12:40:00+0000_.

*-x*, *--system*
	Show the computer's stops too.

*-R*, *--nohostname*
	Leave out where each sign-in came from.

*-a*, *--hostlast*
	Show where from last, after the length.

*-w*, *--fullnames*
	Show whole names, which are otherwise cut at eight characters.

*-s* _time_, *--since* _time_
	Only sessions that began at _time_ or after it.

*-t* _time_, *--until* _time_
	Only sessions that began at _time_ or before it.

*-p* _time_, *--present* _time_
	Only sessions that ran at _time_.

*-d*, *--dns*, *-i*, *--ip*
	Taken, and change nothing: where from is kept as it came.

A _time_ is as *log*(1) takes it: _now_, _today_, _yesterday_,
_-2h_, _2026-10-31_, _2026-10-31 12:40_ or _12:40_.

# EXAMPLES

```
$ last
bob      pts/1                         Sat Oct 31 12:46   still logged in
bob      pts/1                         Sat Oct 31 12:41 - 12:45  (00:04)
reboot   system boot  0.13.0           Sat Oct 31 12:39   still running

sign-in records begin Sat Oct 31 12:39:01 2026
```

Everyone's, through elevate, the five newest:

```
$ elevate last -5
```

# FILES

_/system/state/sign-ins/system_
	The computer's starts and stops, which everyone reads.

_/system/state/sign-ins/_*number*
	Each account's sign-ins, sign-outs and wrong passwords, which its
	person and root read.

_/system/state/sign-ins/unknown_
	The wrong passwords typed for names that are no account's, which
	root reads.

# EXIT STATUS

0, or 1 if the records cannot be read, and 1 for a mistake in the
command line.

# SEE ALSO

*lastlog*(8), *who*(1), *w*(1), *authd*(8), *login*(1), *elevate*(8),
*systools*(1)
