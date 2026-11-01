ip(8) "Dukovo" "Dukovo Manual"

; SPDX-FileCopyrightText: 2026 The Dukovo contributors
; SPDX-License-Identifier: Apache-2.0 OR MIT

# NAME

ip - show and set the network interfaces, their addresses and routes

# SYNOPSIS

*ip* [_options_] *link* [*show* [[*dev*] _name_]]

*ip* [_options_] *link set* [*dev*] _name_ *up*

*ip* [_options_] *address* [*show* [[*dev*] _name_]]

*ip address* {*add* | *del*} _address_[/_prefix_] *dev* _name_

*ip address flush dev* _name_

*ip route* [*show* [*dev* _name_]]

*ip route* {*add* | *replace*} *default via* _address_ [*dev* _name_]

*ip route del default* [*via* _address_] [*dev* _name_]

# DESCRIPTION

ip shows the network interfaces, their IPv4 addresses and their routes,
and sets them, with the words and output of iproute2's ip on Linux for
what Dukovo has (ADR 0190). An object or a command may be cut to its
first letters, as iproute2 takes them: *ip a* is *ip address show*, and
*ip r* is *ip route show*. With no command, an object is shown.

The interfaces are *lo*, numbered 1, and the network cards, *net0*,
*net1*... from 2, in the order the kernel found them. ip reads them from
_/kernel/class/net_ (*radix-sysfs*(5)) and the routes from
_/processes/net/route_ (*radix-proc*(5)), which anyone may.

Each card has up to four addresses, the first the one it sends from, and
at most one router, its default route; the route to each address's
network comes with the address. ip changes them through the network
capability (*radix-network*(2)): it reads what the interface has, changes
that and gives it the whole back. Only a program given *admin.network*
by its service (*spark*(8)) may; root's shells at the console are
(*consoled*(8)).

# OBJECTS AND COMMANDS

*link* [*show* [[*dev*] _name_]]
	Each interface, or the one named: its number, name, flags, MTU and
	state, then its MAC address and the broadcast one. The flags are
	Linux's: *LOOPBACK* for *lo*, else *BROADCAST*; *UP*, as every
	interface always is; and *LOWER\_UP* while its link is, else
	*NO-CARRIER*. The state is *UP*, *DOWN* for a card the kernel
	stopped after an error, or *UNKNOWN* for *lo*.

```
2: net0: <BROADCAST,UP,LOWER_UP> mtu 1500 qdisc noqueue state UP mode DEFAULT group default
    link/ether 52:54:00:12:34:56 brd ff:ff:ff:ff:ff:ff
```

*link set* [*dev*] _name_ *up*
	Does nothing: the interfaces are always up. *down* fails, as taking
	one down is not there yet.

*address* [*show* [[*dev*] _name_]]
	As *link*, with each IPv4 address, its network's broadcast address
	and its scope: *host* on *lo*, else *global*; an address in the
	network of one before it is a *secondary* one. _lo_ shows
	127.0.0.1/8 only: the cards' addresses it answers are theirs.

```
    inet 10.0.2.15/24 brd 10.0.2.255 scope global net0
       valid_lft forever preferred_lft forever
```

*address add* _address_[/_prefix_] *dev* _name_
	Gives the card one more address, after those it has; without a
	prefix, a /32. *local* may come before the address, and *brd +* or
	the network's own broadcast address after it, as scripts write
	them; another broadcast address is refused, as the kernel takes the
	network's.

*address del* _address_[/_prefix_] *dev* _name_
	Takes the address from the card; without a prefix, whatever its
	prefix. The router goes with the last address whose network
	reaches it, as Linux's routes through it do.

*address flush dev* _name_
	Takes all the card's addresses, and its router.

*route* [*show* [*dev* _name_]]
	The default routes, then each address's network, as Linux's main
	table lists them; _lo_'s, in Linux's local table, are not shown.

```
default via 10.0.2.2 dev net0
10.0.2.0/24 dev net0 proto kernel scope link src 10.0.2.15
```

*route add default via* _address_ [*dev* _name_]
	Gives the card whose network reaches _address_, or the one named,
	_address_ as its router. *0.0.0.0/0* is *default*. Other routes are
	not set this way: each network's comes with its address.

*route replace default via* _address_ [*dev* _name_]
	As *add*, in place of the router it has.

*route del default* [*via* _address_] [*dev* _name_]
	Takes the card's router, if it is _address_ when one is given.

# OPTIONS

*-br*, *-brief*
	A line for each interface: its name, state and MAC address and
	flags for *link*, or its addresses for *address*.

*-s*, *-stats*, *-statistics*
	With *link*, what each interface received and sent: bytes,
	packets, errors and drops.

*-c*, *-color*[*=always*|*auto*|*never*]
	Interface names, states, addresses and MAC addresses in colour,
	always (the default) or with *auto* only on a terminal and not
	with *NO_COLOR* set. *IP_COLORS* changes the colours, as
	_name_*=*_SGR_ pairs joined by *:*, with the names *name*, *up*,
	*down*, *address* and *mac*; *nukesh*(1) sets it from
	*colors.toml*(5), and its alias gives *-color=auto*.

*-4*
	IPv4, all there is until Phase 12c of the roadmap; *-6* fails.

*-h*, *-help*
	The commands, briefly.

Not yet: the other objects (*neigh*, *rule*, *netns*...), *-j*,
routes to networks of their own, *ip route get*, *ip link set* _mtu_ or
_down_, and IPv6.

# EXIT STATUS

0 when it showed or changed what it was asked; 1 for a mistake in the
command line, or an interface that is not there; 2 when the change was
refused, after *RTNETLINK answers:* and why, as iproute2 says it: *File
exists* for an address or router the card has, *Cannot assign requested
address* for an address it does not, *Network is unreachable* for a
router no card's network reaches, and *Operation not permitted* without
*admin.network*.

# EXAMPLES

```
# ip address add 192.168.1.20/24 dev net0
# ip route replace default via 192.168.1.1
# ip -br address
lo               UNKNOWN        127.0.0.1/8
net0             UP             192.168.1.20/24
```

# SEE ALSO

*ss*(8), *colors.toml*(5), *radix-network*(2), *radix-proc*(5),
*radix-sysfs*(5), *spark*(8), *systools*(1)
