log(1) "Dukovo" "Dukovo Manual"

; SPDX-FileCopyrightText: 2026 The Dukovo contributors
; SPDX-License-Identifier: Apache-2.0 OR MIT

# NAME

log - read the system's log

# SYNOPSIS

*log* [_option_ ...] [_name_ ...]

*log --list-boots*

*log --disk-usage*

# DESCRIPTION

log prints the records of the system's log that *logd*(8) keeps: the
kernel's own, the services' lines and the lines programs write to the
kernel log, this boot's unless asked for others, oldest first, a record
a line:

```
Oct 30 21:25:01 dukovo volumed: watching for volumes; 5 disks and partitions now
Oct 30 21:25:03 dukovo nukesh[57]: ...
```

the local time the record was made, the host's name, who wrote it (a
service by its name, a program by its name and process number, the
kernel as *kernel*) and the text. On a terminal errors are red and
warnings yellow.

A _name_ shows only the records of that service or program: a
program's name is the one it gave its thread, which a program can
choose, so a program can show among a service's records by its name,
with its process number after it. *-s* shows a service's records alone.
Each option narrows what is shown; together they all must hold.

*journalctl*(1) is the same command with journalctl's options.

# OPTIONS

*-s*, *--service* _name_
	Only the records of this service: the lines Spark wrote for it. It
	may be given more than once, and with _name_ operands.

*-k*, *--kernel*
	Only the kernel's own records.

*-p*, *--priority* _level_
	Only the records of this level and the more urgent ones: *error*,
	*warning*, *info* or *debug*, the names of *syslog*(3) (*emerg*,
	*alert*, *crit*, *err*, *warn*, *notice*) or a number from 0 to 7.

*-S*, *--since* _time_, *-U*, *--until* _time_
	Only the records made at or after _time_, or before it. A _time_
	is *now*, *today*, *yesterday* or *tomorrow*; a time from now, as
	*-1h*, *+30min* or *2 days ago* (units *s*, *min*, *h*, *d*, *w* and
	their longer names); or *YYYY-MM-DD* [*HH:MM*[*:SS*]], or
	*HH:MM*[*:SS*] today, in local time unless it ends with *Z* or
	*UTC*.

*-g*, *--grep* _pattern_
	Only the records whose text an extended regular expression
	matches, ignoring case unless it has a capital letter.

*-n*, *--lines* _n_
	Only the last _n_ records; *all* for every one.

*-f*, *--follow*
	After the records kept (the last 10, unless *-n* says otherwise),
	print new ones as they come, until interrupted.

*-b*, *--boot* [_boot_]
	The records of another boot: *0* is this one, *-1* the one before
	it and so on, or a boot's number as *--list-boots* shows it.

*--all*
	The records of every boot the log keeps, with a line between the
	boots.

*-r*, *--reverse*
	Newest first. Not with *-f*.

*-o*, *--output* _format_
	*short* (as above), *short-iso* (the time as ISO 8601),
	*short-precise* (the time to the microsecond), *cat* (the text
	alone), *json* (each record as logd keeps it, a line of JSON) or
	*json-pretty*.

*--json*
	*-o json*.

*--utc*
	Times in UTC, not local time.

*--no-hostname*
	Leave out the host's name.

*--list-boots*
	List the boots the log keeps, oldest first: each one's place back
	from this one, its number and the times of its first and last
	records.

*--audit*
	The audit log's records: the security events the kernel and
	Spark wrote (*logd*(8)), which it keeps for a year. Every other
	option chooses among them as among the log's.

*--disk-usage*
	Print how much space the log takes, the audit log's share among it.

*--no-pager*
	Changes nothing: log does not page.

# EXIT STATUS

0, or 1 if logd is not running or does not answer, or the command line
is wrong.

# EXAMPLES

What volumed said since boot:

	log volumed

The errors of the last hour, in every boot:

	log --all -p error --since -1h

Watch the log:

	log -f

The security events of the last week:

	log --audit --all --since "7 days ago"

# SEE ALSO

*journalctl*(1), *dmesg*(1), *logd*(8), *logtrim*(8), *log*(5), *systools*(1)
