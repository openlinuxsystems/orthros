network(5) "Dukovo" "Dukovo Manual"

; SPDX-FileCopyrightText: 2026 The Dukovo contributors
; SPDX-License-Identifier: Apache-2.0 OR MIT

# NAME

network - how netd sets the network cards up

# SYNOPSIS

_/system/settings/network_

# DESCRIPTION

*netd*(8) gives every network card its address, router and DNS servers
by DHCP, unless this file names the card (ADR 0191). The default file
says nothing but comments, so every card asks DHCP. *service netd
reload* has netd read the file again and set each card up anew.

A section starts with the card's name in brackets, *[net0]*, or its MAC
address, *[52:54:00:12:34:56]*, which stays the card's when cards come
in another order; upper and lower case are the same. The first section
that names a card is the one it takes. The settings after it, one a
line, are _setting_ *=* _value_. A line that starts with *#* is a
comment.

# SETTINGS

*address =* _address_[/_prefix_]
	An address of the card's own, without a prefix a /32; up to four,
	the first the one it sends from. A card with addresses does not
	ask DHCP unless *dhcp = yes* says to, and then has its lease's
	address first and these after it.

*router =* _address_
	The card's router, its default route. It must be on the network
	of one of the card's addresses. With *dhcp = yes*, the lease's
	router comes first.

*dns =* _address_
	A DNS server, up to three; netd writes them into
	_/etc/resolv.conf_ before those DHCP gives.

*dhcp =* *yes* | *no*
	Whether the card asks DHCP. The default is *yes* for a card with no
	*address*, else *no*. *dhcp = no* with no *address* has netd leave
	the card alone, as the *ip=* flag (*dukovo-boot*(7)) or *ip*(8)
	sets it up.

netd says what is wrong in the file in the system's log, with the line's
number, and leaves out that line: a setting it does not know, an
address that is not a unicast IPv4 one, a fifth address or a fourth DNS
server, a second router or *dhcp*, a router on none of the card's
networks, or a card named twice.

# EXAMPLES

Every card by DHCP but *net1*, which has an address of its own, and a
card that is set up by hand:

```
[net1]
address = 192.168.1.10/24
router = 192.168.1.1
dns = 192.168.1.1

[52:54:00:ab:cd:ef]
dhcp = no
```

# FILES

_/system/settings/network_
	This file; Spark copies the default one there when there is none
	(*spark*(8)).

# SEE ALSO

*netd*(8), *ip*(8), *spark*(8)
