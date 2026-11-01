timed(5) "Dukovo" "Dukovo Manual"

; SPDX-FileCopyrightText: 2026 The Dukovo contributors
; SPDX-License-Identifier: Apache-2.0 OR MIT

# NAME

timed - which time servers timed asks

# SYNOPSIS

_/system/settings/timed_

# DESCRIPTION

*timed*(8) sets the clock by the time servers this file names (ADR
0194). The default file says nothing but comments, so timed asks
Cloudflare's, Netnod's and PTB's servers over Network Time Security,
and _pool.ntp.org_ over plain NTP only while none of them answers.
*service timed reload* has timed read the file again.

Each line is _setting_ *=* _value_. A line or the end of one that starts
with *#* is a comment.

# SETTINGS

*servers =* _host_[*:*_port_]...
	The Network Time Security servers to ask, in turn, by the name of
	their key establishment server, whose certificate must carry it,
	and its port, 4460 unless it is given; an IPv6 address goes in
	brackets when it has a port. *servers =* with nothing after it asks
	none. The default: *time.cloudflare.com nts.netnod.se
	ptbtime1.ptb.de*.

*fallback =* _host_[*:*_port_]...
	The plain NTP servers to ask only while no NTS server answers, on
	port 123 unless it is given. Their time is not authenticated: a
	machine on the way could change it. *fallback =* with nothing after
	it asks none, so that the clock is only set by authenticated time.
	The default: *pool.ntp.org*.

*certificates =* _file_...
	More authorities' certificates to trust, PEM files, beside the
	machine's (*dukovo-certificates*(7)): for an NTS server of a
	network's own, whose certificate a private authority signed.

timed says what is wrong in the file in the system's log, with the
line's number, and leaves out that line: a setting it does not know,
or a line with no *=*.

# EXAMPLES

Netnod's servers only, and no plain NTP:

```
servers = nts.netnod.se sth1.nts.netnod.se
fallback =
```

A network's own NTS server, whose certificate its own authority signed:

```
servers = time.example.lan:4460
certificates = /system/settings/example-ca.pem
fallback =
```

# FILES

_/system/settings/timed_
	This file; Spark copies the default one there when there is none
	(*spark*(8)).

# SEE ALSO

*timed*(8), *timedatectl*(1), *dukovo-certificates*(7),
*dukovo-privacy*(7)
