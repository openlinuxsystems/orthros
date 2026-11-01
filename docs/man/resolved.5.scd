resolved(5) "Dukovo" "Dukovo Manual"

; SPDX-FileCopyrightText: 2026 The Dukovo contributors
; SPDX-License-Identifier: Apache-2.0 OR MIT

# NAME

resolved - which DNS servers resolved asks, and how

# SYNOPSIS

_/system/settings/resolved_

# DESCRIPTION

*resolved*(8) asks the servers this file names, or else those DHCP gave
(*netd*(8)), for the names the machine's programs look up (ADR 0193).
The default file says nothing but comments, so DHCP's servers are
asked, over DNS over TLS when they answer it. resolved reads the file
again when it changes, and on *service resolved reload*, which also
empties its cache.

Each line is _setting_ *=* _value_. A *#* at the start of a line, or
after white space, starts a comment; one inside a word, as in a server's
name, does not.

# SETTINGS

*servers =* _server_...
	The servers to ask, in turn, in place of DHCP's, up to eight, on
	one line or several. Each is one of:

	_address_[*:*_port_]
		Asked as *dns-over-tls* says: over DNS over TLS on port 853
		if it answers there, else over plain DNS on _port_, 53 by
		default. An IPv6 address goes in brackets when it has a port,
		*[2620:fe::fe]:53*.

	_address_[*:*_port_]*#*_name_
		Asked over DNS over TLS only, on _port_, 853 by default, with
		its certificate checked for _name_: *9.9.9.9#dns.quad9.net*.

	*https://*_host_[*:*_port_][_/path_][*#*_address_]
		Asked over DNS over HTTPS (RFC 8484) at _host_, reached at
		_address_, with its certificate checked for _host_; the path
		is _/dns-query_ unless the URL gives one. The address may be
		left out only when _host_ is one:
		*https://dns.quad9.net/dns-query#9.9.9.9*.

*dns-over-tls =* *opportunistic* | *yes* | *no*
	How servers named by their address alone are asked, those DHCP
	gave among them. *opportunistic*, the default: over TLS if they
	answer there, taking any certificate, else over plain DNS, and over
	TLS again an hour later. *yes*: over TLS only, with their
	certificates checked for their addresses; a server whose
	certificate does not carry its address fails. *no*: over plain DNS.

*cache-size =* _number_
	The answers kept, 4096 by default; *0* keeps none.

*certificates =* _file_...
	More authorities' certificates to trust, PEM files, beside the
	machine's (*dukovo-certificates*(7)): for a server of a network's
	own, whose certificate a private authority signed.

resolved says what is wrong in the file in the system's log, with the
line's number, and leaves out that line or that server: a setting it
does not know, a server it cannot read, a ninth server, or a value that
is not one of the setting's.

# EXAMPLES

Quad9's servers over DNS over TLS, and Cloudflare's over DNS over
HTTPS when they do not answer, in place of DHCP's:

```
servers = 9.9.9.9#dns.quad9.net 149.112.112.112#dns.quad9.net
servers = https://cloudflare-dns.com/dns-query#1.1.1.1
```

DHCP's servers, but over TLS only:

```
dns-over-tls = yes
```

# FILES

_/system/settings/resolved_
	This file; Spark copies the default one there when there is none
	(*spark*(8)).

# SEE ALSO

*resolved*(8), *resolvectl*(1), *netd*(8), *network*(5),
*dukovo-certificates*(7)
