<!--
SPDX-FileCopyrightText: 2026 The Dukovo contributors
SPDX-License-Identifier: Apache-2.0 OR MIT
-->

# 0191. netd sets each card up by DHCP, or as its settings say

- Status: Accepted
- Date: 2026-10-31

## Context

Since ADR 0190 a program holding the network capability's control
right can give a card its addresses and router, and ip(8) does it by
hand. Nothing does it by itself: a card gets an address only from the
`ip=` flag or from an administrator. The roadmap's 12a.4 asks for
netd, a DHCPv4 client and static settings, and 12a is done when DHCP
works in QEMU.

A DHCP client (RFC 2131) sends from port 68 before its card has an
address, to the servers' port 67 by broadcast, and keeps its lease by
renewing it at half its time (T1) and rebinding at seven eighths (T2).
On Linux each card's client binds port 68 with SO_BINDTODEVICE, or
uses a packet socket; Radix's ports were one table for the machine, so
only one socket could hold port 68. Linux lets sockets share a port
when each is bound to a different interface before its bind
(`udp_lib_lport_inuse`).

What a DHCP server sends goes into the kernel's routes and into
resolv.conf, so a server on a network the machine joins, the coffee
shop's or an attacker's, must not be able to make netd crash, set a
broadcast or multicast address, route through an address no card
reaches or write a name the resolver cannot read.

## Decision

- **netd is a service of systools, in Rust,** that Spark starts on
  every machine (`order = 20`, after the consoles), with
  `admin.network` to set the cards up and `network.listen = 68` for its
  clients. It sets a card up through radix-network(2)'s `configure`, as
  ip(8) does, and changes only the addresses it gave: an
  administrator's stay, beside them, up to the card's four.
- **Every card by DHCP, unless its settings say otherwise.**
  `/system/settings/network` (network(5)) has a section for each card
  it names, by its name or its MAC address: `address`, `router` and
  `dns` give it addresses of its own, and `dhcp = no` with none has
  netd leave it alone. A card that has an address at netd's first look
  and no lease, which the `ip=` flag or an administrator gave it, is
  left alone too, so that the boot flag still works. `service netd
  reload` (SIGHUP) reads the file again and sets each card up anew.
- **The client follows RFC 2131's states:** INIT, SELECTING,
  REQUESTING, INIT-REBOOT, BOUND, RENEWING and REBINDING, with its
  retransmissions 4, 8, 16, 32 and 64 seconds apart, each a second more
  or less at random, renewal unicast to the server at T1 and rebinding
  by broadcast at T2, at least a minute apart, and the address given up
  when the lease ends. A NAK starts it again, after a wait that
  doubles with each NAK in a row, to about a minute. It is a state
  machine of its own (`dhcp.rs`) that netd gives the time and the
  messages, so that unit tests run every state without a network.
- **Messages read and written with smoltcp's `wire::DhcpRepr`,** the
  kernel's stack's own parser (0BSD), built for the host's standard
  library. netd reads the router option itself, since smoltcp takes only
  one router, and the domain name. It asks for the address, router, DNS
  servers, domain name, lease time, server and T1 and T2, and sends the
  card's MAC address as its client identifier and no host name.
- **What a server says is checked before it is used.** The address must
  be a unicast one (not 0/8, loopback, multicast or 240/4); a router
  must be unicast, on the address's network and neither the address nor
  the network's broadcast one, or it is dropped; DNS servers must be
  unicast; a domain name must be labels of letters, digits, `-` and `_`,
  or it is dropped; a lease is at least a minute; T1 and T2 must come in
  order within the lease, or they are half and seven eighths of it. A
  reply whose transaction or MAC address is not the client's is
  ignored, and a message that does not parse is too.
- **Port 68 is shared among cards.** The kernel lets UDP and TCP
  sockets share a port when each was bound to a different interface
  with SO_BINDTODEVICE before its bind, as Linux does; one bound to no
  interface still holds the port alone. A socket that shares its port
  cannot then change its interface (EINVAL). netd gives each DHCP card
  a socket of its own on port 68, bound to it, with SO_BROADCAST; while
  another socket holds the port on that card, the card waits, and netd
  tries again at each look.
- **A refused bind no longer frees the port.** The port table made a
  hold even for a port another socket held, and dropping it cleared
  the port's bit, so the bind after a refused one took a port that was
  still held, and the search for a port of the kernel's own freed each
  held one it passed (since ADR 0183). The `dhcp` boot test found it;
  the table makes a hold only for a port it took.
- **Leases are kept,** one file for each card in `/system/state/netd`,
  named by its MAC address, written whole and renamed into place. netd
  started again goes on with the lease the card has, and connections
  go on through the address; after a reboot the card asks for the
  address it had (INIT-REBOOT), and asks afresh if no server answers
  twice.
- **resolv.conf is netd's, and `/etc/resolv.conf` leads to it.**
  netd writes `/system/live/netd/resolv.conf`: the first three DNS
  servers, the settings' before DHCP's, and the domain names as
  `search`. The default settings have `/system/settings/resolv.conf`,
  which is `/etc/resolv.conf`, as a symbolic link to it, which Spark now
  copies as a link, as Linux's leads to systemd-resolved's file. An
  administrator who wants servers of their own makes it a file. netd's
  service runs in a view (ADR 0217) that holds only its leases and that
  folder, so netd, which reads what any network sends, cannot change
  the settings; every service's view holds the folder read-only, as it
  holds notifyd's, so that the C library's resolver finds it.
- **netd looks at the cards every five seconds,** for cards that come
  and go and links that come back, which have the card ask for its
  address again; otherwise it sleeps until a client has something to
  send. SIGTERM ends it and leaves the cards as they are.

## Consequences

- A machine on QEMU's user network, or on a home or office network, is
  set up at boot with nothing to configure, and the `ip=` flag and ip(8)
  keep working beside netd. Ported programs find `/etc/resolv.conf` as
  on Linux; mlibc's resolver reads only its first `nameserver` until
  resolved (12a.5) takes over. A service started before netd made its
  folder, at the first boot, has no DNS servers in its view until it is
  started again; services that use the network start after netd's
  order, 20.
- netd wakes every five seconds to look at the cards, a read of
  `/kernel/class/net` and `/processes/net/route` each time, until the
  kernel can say when a card comes or its link changes.
- The kernel keeps at most 64 sockets on shared ports at once, across
  protocols and interfaces, a card's DHCP client each; a bind past that
  fails with EADDRINUSE.
- netd does not probe the offered address with ARP before taking it
  (RFC 2131's SHOULD, RFC 5227), nor send DHCPDECLINE or DHCPRELEASE;
  a shut-down machine keeps its lease until it ends, as most clients
  do. DHCPv6 comes with IPv6 (12c), and the roadmap's optional DHCP
  server, for a machine that hands out addresses, with it.
- Two programs setting one card up at once can still lose one's change
  (ADR 0190): an administrator's address lasts, but a router changed by
  hand is replaced by the lease's at netd's next change.
- Privacy: each DHCP request carries the card's MAC address, as the
  privacy chapter's table says; a card with addresses of its own sends
  nothing. Randomized MAC addresses per network wait for Wi-Fi.
- Tests: `dhcp.rs`'s unit tests walk each state, its timers and
  retransmissions, NAKs, foreign replies, what a server says that must
  be refused and 20,000 random messages; netd's read and write its
  settings, leases and resolv.conf; and a new boot test, `dhcp`, has
  netd set a card with no address up by QEMU's DHCP server, checks the
  address, router, resolv.conf and lease, shares port 68 on `lo`,
  restarts netd and keeps a connection through it, and changes the
  settings to an address of their own and back to DHCP. The `posix`
  test's sockets are refused a port twice. The `consoles` boot test
  counts six default services.

## Alternatives considered

- **dhcproto**, the Rust crate most DHCP programs use (MIT). It brings
  about fifty crates, among them Unicode and ICU data for host names,
  for messages smoltcp's parser, already the kernel's and audited for
  it, reads and writes.
- **DHCP in the kernel**, as the `ip=` flag's setup is. Every card
  would have a client running in kernel code, parsing what any network
  sends, where a service crashes alone and restarts; the kernel needs
  only the shared port.
- **One socket for every card,** on 0.0.0.0:68, telling the cards apart
  by IP_PKTINFO. Radix's sockets do not say which interface a datagram
  came in on, and a broadcast must still go out of one card.
- **A packet socket**, as dhclient and dhcpcd use. Radix has none,
  and one would let netd see all the traffic on the card.
- **dhcpcd or systemd-networkd, ported.** Both set cards up over
  netlink (ADR 0190 has none) and bring settings of their own.
