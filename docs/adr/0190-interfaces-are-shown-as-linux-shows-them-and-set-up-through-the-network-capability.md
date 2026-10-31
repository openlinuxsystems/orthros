<!--
SPDX-FileCopyrightText: 2026 The Dukovo contributors
SPDX-License-Identifier: Apache-2.0 OR MIT
-->

# 0190. Interfaces are shown as Linux shows them, and set up through the network capability

- Status: Accepted
- Date: 2026-10-31

## Context

Until now a card got an address only from the `ip=` and `gateway=`
flags, at boot, and nothing showed the interfaces or the sockets: no
`ip`, no `ss`, no `if_nametoindex`. The roadmap's 12a.4 asks for a
DHCP client and static addresses (netd), and for ip(8) and ss(8).

Linux shows its interfaces in `/sys/class/net`, its counts, routes and
sockets in `/proc/net`, and gives and takes addresses and routes over
rtnetlink, a socket protocol that iproute2's ip speaks; ss asks
sock_diag, another netlink protocol, or reads `/proc/net/tcp`. A DHCP
client must send before its card has an address: from 0.0.0.0, to
255.255.255.255, out of the one card it is setting up, which Linux's
clients do with a packet socket or with SO_BINDTODEVICE. Radix has no
netlink, and its programs reach the network only through capabilities
of its ports (ADR 0183).

## Decision

- **Shown as Linux shows them, in files anyone may read.**
  `/kernel/class/net` has a directory for each interface with Linux's
  files (`ifindex`, `address`, `mtu`, `type`, `flags`, `operstate`,
  `carrier`, `statistics/rx_bytes`...) and one of Radix's own, `ipv4`,
  its addresses with their prefixes, which Linux gives only over
  netlink (radix-sysfs(5)). `/processes/net` has `dev`, `route`, `tcp`
  and `udp` in the formats of Linux's `/proc/net` (radix-proc(5)). The
  interfaces are numbered as Linux numbers them, `lo` 1 and the cards
  from 2, so that a number a program took from Linux's habits names
  the same one. The stack counts what each interface sends, receives
  and drops for them.
- **Set up through the network capability.** radix-network(2), now
  version 2, has `configure`: an interface's addresses, up to four,
  the first the one it sends from, and its router, in place of those
  it had. It needs a new right of the network capability, control,
  whatever its ports; Spark gives a service a capability with only
  that right when its file asks for `admin.network` (spark(8)), and
  consoled's file asks, so that root's shells may, as root may on
  Linux. Each address's network is routed with it, and `lo` answers
  each card's addresses, as Linux's local table does; `lo` itself
  cannot be set.
- **No netlink.** A call of the capability that already decides who
  may use the network says who may set it up, and needs no protocol
  parser in the kernel that every program could reach. The whole set
  goes at once, so the kernel checks one list: a program that changes
  one address reads what the interface has, changes it and gives the
  whole back.
- **Sockets bound to an interface.** SO_BINDTODEVICE (radix-file(2)'s
  `bind_to_device`, by number; by name in the C library, as Linux takes
  it) keeps a socket's packets on one interface. A datagram socket
  bound to an interface with no address sends from 0.0.0.0, and
  receives what is broadcast to it, as a DHCP client needs. SO_BROADCAST,
  which the kernel only kept, now decides: a datagram to a broadcast
  address from a socket without it fails with a new error, `broadcast`,
  which the C library gives as EACCES, as Linux does.
- **The C library reads the files.** `if_nametoindex`,
  `if_indextoname` and `if_nameindex` read `/kernel/class/net`; mlibc's
  own way, rtnetlink, is not there.
- **ip(8) and ss(8) in systools**, with iproute2's words and output for
  what Dukovo has: `ip link`, `ip address` and `ip route` show; `ip
  address add`, `del` and `flush`, and `ip route add`, `replace` and
  `del` of the default route, change; `-br` and `-s`. ss lists the TCP
  and UDP sockets with `-t`, `-u`, `-l`, `-a`, `-n` and `-H`. Errors are
  iproute2's, `RTNETLINK answers: File exists` with exit status 2, as
  scripts look for them.

The DHCP client, netd, follows in 12a.4b on what this gives it.

## Consequences

- Scripts that show the interfaces and set an address and a default
  route with ip work unchanged, and so do ported programs that read
  `/sys/class/net` (through its link) or `/proc/net`. Programs that
  speak rtnetlink (NetworkManager, systemd-networkd, iproute2's own ip)
  do not run, and `getifaddrs` waits for mlibc's Linux option.
- A change is read, changed and written back, so two programs changing
  one interface at once can lose one's change. netd will keep the cards
  it sets up, and an administrator's ip change lasts until netd's next.
- Only a default route can be set: each network's route is its
  address's. Routes to other networks, and taking an interface down,
  come when something needs them.
- ss cannot say which process holds a socket (`-p`): the kernel's lists
  do not name one, as Linux's `/proc/net/tcp` names an inode that
  `/proc/<pid>/fd` leads to. A connected UDP socket is listed as not
  connected (UNCONN): the lists come from smoltcp's sockets, which keep
  no peer for one.
- The ABI's headers changed (an error, an option, a method), so the C
  library and every port are built again.
- Tests: radix-net's unit tests count the traffic and list the sockets;
  ip's and ss's unit tests read files made as the kernel makes them and
  check each change ip asks for; the `posix` boot test runs ip and ss on
  `lo` and binds sockets to it; and a new boot test, `interfaces`,
  starts with a card with no address, has a socket bound to it send a
  DHCPDISCOVER and receive QEMU's offer, sets the card up with ip, and
  connects through it.

## Alternatives considered

- **rtnetlink in the kernel.** It would run iproute2 and
  NetworkManager unchanged, but it is a large protocol, Linux's own,
  parsed by the kernel for every program that opens a socket, and
  Radix's capabilities would still have to decide who may change what.
  A port can speak it to a program that answers it later, if one is
  wanted.
- **Writing the files.** `echo 10.0.2.15/24 > .../ipv4` would need the
  filesystem to carry who may write, where Radix's rights are in
  capabilities; reading them suits a file, setting them a call.
- **netd as the only one that sets cards up**, with ip asking it over a
  socket. The kernel would still take the change from someone; giving
  it to whoever holds the right keeps ip working without netd, as on a
  machine set up by hand.
