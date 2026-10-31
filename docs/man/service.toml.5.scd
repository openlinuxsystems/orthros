service.toml(5) "Dukovo" "Dukovo Manual"

; SPDX-FileCopyrightText: 2026 The Dukovo contributors
; SPDX-License-Identifier: Apache-2.0 OR MIT

# NAME

service.toml - the files that describe Spark's services

# SYNOPSIS

_/system/settings/spark/services/name.toml_

# DESCRIPTION

Each file describes a service, a program *spark*(8) starts at boot and
keeps running. The file's name without _.toml_ is the service's, and its
resource domain's and its log lines' too: 1 to 23 letters, digits, *-*,
*\_* and *.*, starting with a letter, a digit or *\_*. The files are TOML
(https://toml.io), with comments after *#*, read by the same code in
Spark and in *service*(8), so *service check* says what Spark will say
(ADR 0216).

The system's own files come from
_/system/resources/default-settings/spark/services_, which Spark copies
in when they are missing; to turn one off, set *enabled = false* in its
file rather than removing it, or the next boot brings it back. Each
shipped file lists its settings with comments.

Write a new file with *service new* _name_, which writes every setting
explained, and edit one with *service edit* _name_, which checks the file
as you save it and puts it in place only once it reads. It opens
*nano*(1) unless *VISUAL* or *EDITOR* names another editor, and nano
highlights TOML.

A setting the format does not know is an error, so a misspelt one is
never silently ignored, and an error names its line and says what is
wrong in plain words:

```
/system/settings/spark/services/web.toml: line 6: restart must be always, on-failure or never, not `sometimes`
```

# SETTINGS

*service_version* = _1_
	The version of this format the file was written for; every file has
	it. A later Dukovo reads every earlier version. A file for a version
	newer than this Dukovo knows is refused, with a message to update
	Dukovo.

*description* = _"text"_
	What it is, in a few words, for *service status*.

*program* = _"path"_
	The program's absolute path. Every file has it.

*arguments* = [_"word"_, ...]
	Its arguments, after the program's name; none unless set.

*environment*
	A table of more of its environment, a variable a line, such as
	*TZ = "UTC"*, over the environment Spark gives every service. A
	name holds no *=*. Spark passes them in the order of their names.

*order* = _number_
	When it starts: lower first, 50 unless set. Shutting down stops the
	highest first, so the services that others need want a low one. The
	system's own take 5 to 50.

*enabled* = *true* | *false*
	Whether Spark starts it at boot; *true* unless set. *service* _name_
	*start* starts one that is not.

*restart* = *"always"* | *"on-failure"* | *"never"*
	When it is started again after it ends: whatever its end (the
	default), only after an exit code other than 0 or a signal, or not
	at all.

*limits*
	A table of what its resource domain may use:

	*cpu_weight* = _number_
		The domain's CPU weight, 1 to 10000, 100 unless set: a busy
		domain of weight 200 gets twice the CPU time of one of 100
		when they compete (*radix-scheduling*(7)).

	*memory* = _"size"_ | _bytes_ | *"none"*
		The domain's memory limit: a number of bytes, or a size with
		*K*, *M* or *G* after it (KiB and so on), such as *"64M"*; no
		limit unless set.

	*oom* = *"service"* | *"app"*
		The domain's out-of-memory class: *"service"*, the default, is
		stopped only after every app domain when memory runs out
		(*radix-resource-domain*(2)).

*permissions*
	A table of what it may do beyond running, by the names of the
	security design's permission list, in groups. A service has only
	what its file gives it:

	*network.outbound* = *true* | *false*
		Whether it may connect out, to any address and port, and send
		datagrams; *false* unless set.

	*network.listen* = [_port_ | _"first-last"_, ...]
		The ports it may bind to and listen on, as numbers and ranges,
		such as *[22, "8000-8009"]*; none unless set. Ranges may not
		overlap, and with *network.outbound* there are at most eight.

	*admin.hardware* = *true* | *false*
		Whether it may set the machine's fans, their curves, its LEDs
		and its ARGB header, with the platform capability
		(*radix-platform*(2), ADR 0208); *false* unless set.
		*consoled*(8)'s file sets it, so that root's shells may use
		*ectool*(8).

	A dotted name is TOML's for a table in a table: *network.listen
	= [80]* under *[permissions]* is the same as *listen = [80]* under
	*[permissions.network]*.

# WHEN SPARK READS A FILE

Spark reads every file at boot, and a service's file again each time it
starts the service, by *service* _name_ *start* or *restart* or after
the service ended: a changed file takes effect at the service's next
start. Its CPU weight and memory limit change then too; its
out-of-memory class stays its domain's until the next boot. A file added
since boot is read when its service is first started.

A file that does not read never takes a service down. Spark keeps a copy
of each file that read in _/system/state/spark/good_, and when a file
does not read, it logs which line is wrong and why, once, and goes on
with that copy, at boot too:

```
spark: /system/settings/spark/services/web.toml: line 6: restart must be always, on-failure or never, not `sometimes`; it runs its last good copy
```

A service whose file never read is not started.

# FILES OF BEFORE PHASE 13

Until Phase 13, a service file was _name_*.service*, lines of
_key_ *=* _value_ (ADR 0101). Spark converts each it finds that has no
_name_*.toml* beside it, at boot before the new defaults come in, or
when its service is started: it writes _name_*.toml*, with every comment
above the setting it was above, and keeps the old file as
_name_*.service.old*. *weight* becomes *cpu_weight*, and with *memory*
and *oom* goes in *[limits]*; *environment* becomes a table; and
*network.outbound*, *network.listen* and *admin.hardware* go in
*[permissions]*. A file with an error is not converted, and Spark logs
why.

# EXAMPLE

A web server on ports 80 and 443, started after the system's services:

```
service_version = 1
description = "Web server"
program = "/applications/commands/nginx"
arguments = ["-g", "daemon off;"]
order = 60
restart = "on-failure"

[environment]
TZ = "UTC"

[limits]
memory = "256M"

[permissions]
network.listen = [80, 443]
```

# FILES

_/system/settings/spark/services/name.toml_
	The services.

_/system/resources/default-settings/spark/services_
	The system's own, copied in when missing.

_/system/state/spark/good/name.toml_
	The last copy of each that read.

# SEE ALSO

*spark*(8), *service*(8), *logd*(8)
