resolved(8) "Dukovo" "Dukovo Manual"

; SPDX-FileCopyrightText: 2026 The Dukovo contributors
; SPDX-License-Identifier: Apache-2.0 OR MIT

# NAME

resolved - the DNS resolver every program asks

# SYNOPSIS

*resolved*

# DESCRIPTION

resolved looks names up for the whole machine (ADR 0193), as systemd's
*systemd-resolved* does on Linux. Spark runs it as the *resolved*
service, just after *netd*(8) and before the services that use the
network (*spark*(8)).

It answers DNS queries on 127.0.0.53, port 53, over UDP and TCP. It
writes _/system/live/resolved/resolv.conf_, which names that address,
and _/etc/resolv.conf_ leads there, so every program's *getaddrinfo*(3)
asks it, and so do the programs that read _/etc/resolv.conf_ for
themselves. The C library looks a name up in _/etc/hosts_ first, which
names localhost, and asks with the file's search domains as glibc does.

It asks the servers that _/system/settings/resolved_ names
(*resolved*(5)), or else those DHCP gave, which netd writes into
_/system/live/netd/resolv.conf_, in turn: a server that fails or does
not answer within two seconds is passed over for the next one, and for
half a minute after that. It asks eight at most, and uses the search
domains of netd's file.

A server named by its address alone is asked over DNS over TLS (RFC
7858) on port 853 first, taking any certificate, as RFC 7858's
opportunistic privacy profile does: the network on the way cannot read
the names, though a machine that pretends to be the server could. A
server that does not answer there is asked over plain DNS, and over TLS
again an hour later. *dns-over-tls = yes* has such servers asked over
TLS only, with their certificates checked for their addresses, and
*no* over plain DNS only.

A server named with the name its certificate carries,
*9.9.9.9#dns.quad9.net*, is asked over TLS only, with its certificate
checked for that name, and one named by an *https://* URL is asked over
DNS over HTTPS (RFC 8484). Certificates are checked against the
machine's authorities (*dukovo-certificates*(7)) and those
*certificates* adds. A connection over TLS stays open for the next
queries.

Plain queries go from a port the kernel picks at random each time,
with a random identifier, and an answer is taken only for the question
that was asked, so that it is hard to slip a false one in. Queries sent
over UDP offer answers of up to 1232 bytes; a longer answer is asked
for again over TCP.

## The cache

Answers are kept for as long as the times to live of their records
say, a day at most, and the answers that a name is not there for as
long as their SOA record says (RFC 2308), an hour at most. An answer
given from the cache has its times to live counted down. *cache-size*
answers are kept, 4096 by default: past that, those whose time is up
go first, then the one closest to its end. *resolvectl flush-caches*
empties the cache.

## Names that never leave the machine

_localhost_ and the names below it are answered here, as 127.0.0.1 and
::1, and so are those addresses' reverse names (RFC 6761). Names under
_local_, which multicast DNS answers on the local network (RFC 6762),
and under _invalid_ are answered as not there, so that they are never
sent to a server.

## What it shows

*resolvectl status* (*resolvectl*(1)) shows
_/system/live/resolved/status_, which resolved writes as it changes:

```
Listening on: 127.0.0.53:53 (UDP and TCP)
DNS over TLS: opportunistic
Cache: 112 of 4096 answers
Queries: 431, 302 from the cache, 6 answered here, 0 failed
Server: 10.0.2.3 (from DHCP): plain DNS over UDP; 123 answered, 0 failed
```

What it does goes to its standard output and error, which Spark
copies into the system's log after *resolved:*.

## How it is built

The queries over UDP come to one thread, which answers those it can
from the cache and passes the rest to four threads that ask the
servers, each with connections of its own; 256 queries wait for them at
most, and past that a query is dropped, and its program asks again. A
TCP connection has a thread of its own, sixteen at most, and is closed
after ten seconds without a query. Another thread looks at the settings
and netd's file every two seconds, and reads them again when they
change.

Its service holds port 53, netd's folder to read and its own to write,
beyond what every service holds (*service.toml*(5)); it cannot write
the settings.

# SIGNALS

*SIGHUP*
	Read the settings again and empty the cache: *service resolved
	reload* (*service*(8)) sends it.

*SIGUSR2*
	Empty the cache: *resolvectl flush-caches* sends it.

*SIGTERM*
	End.

# FILES

_/system/settings/resolved_
	The settings (*resolved*(5)).

_/system/live/netd/resolv.conf_
	The servers and search domains DHCP gave, which *netd*(8) writes.

_/system/live/resolved/resolv.conf_
	The file _/etc/resolv.conf_ leads to: *nameserver 127.0.0.53*, and
	netd's search domains.

_/system/live/resolved/status_
	What *resolvectl status* shows.

_/system/live/resolved/pid_
	Its process number, for *resolvectl flush-caches*.

_/system/settings/spark/services/resolved.toml_
	Its service (*service.toml*(5)).

# EXIT STATUS

0 when it was ended; 1 for an argument, as it takes none, or when port
53 cannot be had.

Not yet: DNSSEC validation, IPv6 servers (Phase 12d of the roadmap), a
name for each network card's servers, LLMNR, and mDNS, which is to come
with discovery (Phase 12e).

# SEE ALSO

*resolved*(5), *resolvectl*(1), *netd*(8), *network*(5),
*dukovo-certificates*(7), *dukovo-privacy*(7), *service*(8),
*spark*(8), *systools*(1)
