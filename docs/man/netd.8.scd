netd(8) "Dukovo" "Dukovo Manual"

; SPDX-FileCopyrightText: 2026 The Dukovo contributors
; SPDX-License-Identifier: Apache-2.0 OR MIT

# NAME

netd - set the network cards up, by DHCP or as their settings say

# SYNOPSIS

*netd*

# DESCRIPTION

netd gives each network card its addresses, its router and its DNS
servers (ADR 0191). Spark runs it as the *netd* service on every
machine, after the consoles and before the services that use the
network, with *admin.network* (*spark*(8)).

A card gets them by DHCP unless _/system/settings/network_ names it
(*network*(5)), which may give it addresses of its own or have netd
leave it alone. A card that already has an address when netd first
looks, and no lease, which the kernel's *ip=* flag (*dukovo-boot*(7))
or an administrator gave it, is left alone too. netd sets a card up
through the network capability (*radix-network*(2)), as *ip*(8) does,
and changes only the addresses it gave: an administrator's stay beside
them, up to the card's four.

netd's client is DHCPv4's (RFC 2131). It asks for an address by
broadcast from port 68 of the card, waiting 4, 8, 16, 32 and then 64
seconds between tries, a second more or less at random; renews the
lease with its server at half its time and with any server at seven
eighths; and gives the address up when the lease ends, and asks again.
It asks for the address, router, DNS servers, domain name, lease time
and the times to renew and rebind, sends the card's MAC address as its
client identifier, and no host name. A server's answer is used only
for what passes netd's checks: a unicast address, a router on its
network, unicast DNS servers, a domain name of letters, digits, *-* and
*\_*, and a lease of at least a minute.

The kernel lets each card's socket hold port 68 beside the others', as
each is bound to its own card (*radix-posix*(7)). While another socket
holds the port on a card, the card waits for it, and netd tries again
each time it looks at the cards.

netd keeps each lease in _/system/state/netd_, a file for each card
named by its MAC address. netd started again goes on with the lease the
card has, and its connections go on; after a reboot the card asks for
the address it had, and for any if no server answers.

netd writes the DNS servers, three at most, the settings' first, and
the leases' domain names as *search*, into
_/system/live/netd/resolv.conf_. _/etc/resolv.conf_, which is
_/system/settings/resolv.conf_, is a symbolic link to it; to use servers
of your own, make it a file in its place.

Its service holds only its leases and that folder beyond what every
service reads (*service.toml*(5)): it cannot change the settings.

netd looks at the cards every five seconds: it sets up a card that
came, forgets one that went, and has a card whose link came back ask
for its address again.

What netd does goes to its standard error, which Spark copies into the
system's log after *netd:*:

```
netd: net0: asking for an address by DHCP
netd: net0 has 10.0.2.15/24 from 10.0.2.2 for 1 day, router 10.0.2.2, DNS 10.0.2.3
```

# SIGNALS

*SIGHUP*
	Read _/system/settings/network_ again and set each card up anew:
	*service netd reload* (*service*(8)) sends it.

*SIGTERM*
	End, leaving the cards as they are.

# FILES

_/system/settings/network_
	The settings (*network*(5)).

_/system/state/netd/_*52-54-00-12-34-56.lease*
	A card's lease: its address, router, DNS servers, domain, server,
	when it was given (seconds since 1970) and for how long, and when
	to renew and rebind it, a *key = value* a line.

_/system/live/netd/resolv.conf_
	The DNS servers, which _/etc/resolv.conf_ leads to.

_/system/settings/spark/services/netd.toml_
	Its service (*service.toml*(5)).

# EXIT STATUS

0 when it was ended; 1 for an argument, as it takes none.

Not yet: DHCPv6 and a DHCP server (12c of the roadmap), an ARP probe
of the address offered, DHCPDECLINE and DHCPRELEASE.

# SEE ALSO

*network*(5), *ip*(8), *service*(8), *service.toml*(5), *spark*(8),
*radix-network*(2), *systools*(1)
