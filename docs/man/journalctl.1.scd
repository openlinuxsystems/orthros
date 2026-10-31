journalctl(1) "Dukovo" "Dukovo Manual"

; SPDX-FileCopyrightText: 2026 The Dukovo contributors
; SPDX-License-Identifier: Apache-2.0 OR MIT

# NAME

journalctl - read the system's log, with journalctl's options

# SYNOPSIS

*journalctl* [_option_ ...] [_match_ ...]

# DESCRIPTION

journalctl is *log*(1) with the options of systemd's journalctl and its
defaults, for the scripts and the habits that come from Linux: without
*-b* or *-k* it shows every boot the log keeps. A user who is not root
sees only the records of their own programs, as *log*(1) says; an
administrator sees all of them with *elevate journalctl*. What it
shows comes from
*logd*(8), which keeps no journal files: *--vacuum-size* and the other
options that change the journal's files are not taken.

# OPTIONS

*-u*, *--unit* _service_
	Only the records of this service, the lines Spark wrote for it;
	*.service* may follow its name.

*-t*, *--identifier* _name_
	Only the records of this service or program, by name (a program's
	is the name it gave its thread).

*-k*, *--dmesg*
	Only the kernel's own records, of this boot unless *-b* names
	another.

*-b*, *--boot* [_boot_]
	The records of this boot, or of another: *-1* the one before,
	*all*, or a boot's number.

*-p*, *--priority* _level_
	As *log -p*; of a range such as *err..warning* the less urgent end
	counts.

*-S*, *--since* _time_, *-U*, *--until* _time_, *-g*, *--grep*
_pattern_, *-n*, *--lines* [_n_], *-f*, *--follow*, *-r*, *--reverse*,
*-o*, *--output* _format_, *--utc*, *--no-hostname*, *--list-boots*
	As *log*(1); *-n* alone is the last 10.

*-e*, *--pager-end*
	The last 1000 records, unless *-n* says otherwise.

*--disk-usage*
	journalctl's line of how much space the log takes.

*--vacuum-size* _size_, *--vacuum-time* _span_
	Trim the log, as *logtrim --size* and *logtrim --days* do: remove
	its oldest day files while they take more than _size_ (*500M*,
	*1G*), or those older than _span_ (*2weeks*, *10d*, *6months*,
	*1y*). The audit log keeps to its own setting (*log*(5)).

*-q*, *--quiet*
	Say nothing of what a user who is not root does not see, as
	journalctl's hint to a user outside its groups.

*-a*, *--all*, *-l*, *--full*, *-m*, *--merge*, *-x*, *--catalog*, *--system*, *--no-pager*
	Change nothing.

# MATCHES

A _match_ *\_SYSTEMD\_UNIT=*_service_ or *UNIT=*_service_ is *-u*;
*SYSLOG\_IDENTIFIER=*_name_ or *\_COMM=*_name_ is *-t*;
*\_TRANSPORT=audit* reads the audit log, as *log --audit* does. Other
fields are not kept, and fail.

# EXIT STATUS

0, or 1 if logd is not running or does not answer, or the command line
is wrong.

# SEE ALSO

*log*(1), *dmesg*(1), *logger*(1), *logd*(8), *logtrim*(8), *systools*(1)
