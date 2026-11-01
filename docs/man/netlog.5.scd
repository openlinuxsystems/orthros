netlog(5) "Dukovo" "Dukovo Manual"

; SPDX-FileCopyrightText: 2026 The Dukovo contributors
; SPDX-License-Identifier: Apache-2.0 OR MIT

# NAME

netlog - where netlog sends the log

# SYNOPSIS

_/system/settings/netlog_

# DESCRIPTION

*netlog*(8) sends the system's log over UDP to the computer this file
names (ADR 0196). The default file says nothing but comments, so netlog
sends nothing. *service netlog reload* has netlog read the file again.
The kernel command line's *netlog=* flag (*dukovo-boot*(7)) names a
target for one boot in place of this file's.

Each line is _setting_ *=* _value_. A *#* starts a comment.

# SETTINGS

*target =* _host_[*:*_port_]
	The computer to send to, by name or address, and its UDP port,
	6666 unless it is given. An IPv6 address goes in brackets when it
	has a port. *target =* with nothing after it sends nothing.

*level =* _number_
	Only records of this level or a more urgent one, by *syslog*(3)'s
	numbers: 0 emergencies, 1 alerts, 2 critical, 3 errors, 4
	warnings, 5 notices, 6 information, 7 debugging. All of them are
	sent without it.

netlog says what is wrong in the file in the system's log, and leaves
out that line: a setting it does not know, or a level that is not 0 to
7.

# EXAMPLES

The log, but its debugging records, to a computer on the local
network:

```
target = 192.168.1.20:6666
level = 6
```

There, *nc -klu 6666* shows it as it comes.

# FILES

_/system/settings/netlog_
	This file; Spark copies the default one there when there is none
	(*spark*(8)).

# SEE ALSO

*netlog*(8), *dukovo-boot*(7), *dukovo-privacy*(7)
