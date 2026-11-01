resolvectl(1) "Dukovo" "Dukovo Manual"

; SPDX-FileCopyrightText: 2026 The Dukovo contributors
; SPDX-License-Identifier: Apache-2.0 OR MIT

# NAME

resolvectl - look names up, and show what resolved asks and how

# SYNOPSIS

*resolvectl* [*status*]

*resolvectl query* [*-t* _type_] _name_ | _address_...

*resolvectl flush-caches*

# DESCRIPTION

resolvectl shows what *resolved*(8), the DNS resolver every program
asks, is doing, and looks names up through it, with the commands of
systemd's *resolvectl* (ADR 0193).

*status*, or no command, shows the address resolved answers on, how it
asks servers named by address alone, how full its cache is, how many
queries it has answered and how, and each server it asks, with where
the server came from, how it was asked last, and how many of its
answers came and failed:

```
$ resolvectl
Listening on: 127.0.0.53:53 (UDP and TCP)
DNS over TLS: opportunistic
Cache: 112 of 4096 answers
Queries: 431, 302 from the cache, 6 answered here, 0 failed
Server: 9.9.9.9#dns.quad9.net (from the settings): DNS over TLS, its certificate checked; 123 answered, 0 failed
```

*query* asks resolved for each _name_'s addresses, its A and AAAA
records, and for each _address_'s name, its PTR record, and shows the
records of the answers as *dig*(1) does, with their times to live,
then how many there were and how long they took. An answer too long
for UDP is asked for again over TCP, as a program's would be.

```
$ resolvectl query dukovo.org
dukovo.org.	300	IN	A	192.0.2.10
-- 1 record in 31 ms
```

*flush-caches* empties resolved's cache, so that every name is asked
for anew.

# OPTIONS

*-t*, *--type* _type_
	Ask *query* for records of this type instead: *A*, *AAAA*,
	*ANY*, *CNAME*, *MX*, *NS*, *PTR*, *SOA*, *SRV* or *TXT*, or any
	type by its number, as *TYPE65*.

# EXIT STATUS

0 when it did what was asked; 1 when a name had no records of the type
asked for, when resolved did not answer or is not running, or for a
wrong command or option.

# FILES

_/system/live/resolved/status_
	What *status* shows, which resolved writes.

_/system/live/resolved/pid_
	resolved's process number, to which *flush-caches* sends
	*SIGUSR2*.

# SEE ALSO

*resolved*(8), *resolved*(5), *netd*(8), *systools*(1)
