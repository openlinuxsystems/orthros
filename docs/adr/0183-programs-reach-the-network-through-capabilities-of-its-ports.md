<!--
SPDX-FileCopyrightText: 2026 The Dukovo contributors
SPDX-License-Identifier: Apache-2.0 OR MIT
-->

# 0183. Programs reach the network through capabilities of its ports

- Status: Accepted
- Date: 2026-10-30

## Context

Phase 12a.2 gives programs TCP and UDP sockets on the kernel's stack
(ADR 0180). On Linux every process can open a socket, connect anywhere
and listen on any port above 1023; a firewall or a sandbox takes that
away afterwards, if one is set up. Dukovo's permission model is the
other way round: an app's manifest asks for `network.outbound`, to
connect out, and `network.listen`, for the ports it serves, and gets
nothing it did not ask for (the plan's manifest review). Everything
else a Radix program can reach comes through a capability (ADR 0023),
so the network should too, and the check should be the kernel's, not a
library's that a static binary or a raw system call can skip.

A socket also needs to behave as POSIX's does, since the C library's
BSD sockets (12a.2c) and every port above it are written for Linux:
blocking and non-blocking calls, `poll`, `shutdown`, the errors
programs test for, and descriptors that are copied, passed and
inherited like files.

## Decision

- **A network capability is a range of ports** (`network`, kind 18,
  radix-network(2)), as an I/O port capability is a range of ports of
  the machine's. Its **send right lets its holder connect out**, to
  any address and port, from a port the kernel picks, and send
  datagrams: what `network.outbound` grants. Its **receive right lets
  its holder bind a socket to a port in its range** and listen there:
  `network.listen`, for those ports. `restrict` derives one for fewer
  ports or fewer rights, `ports` says which it has.
- **The kernel gives Spark one for all 65,536 ports.** Spark gives a
  program a derived one in the eight slots from `SLOT_NETWORK`, which
  stay through `fork` and `exec`, as its manifest asks (12a.2c); a
  program given none cannot use the network at all. The boot tests'
  `--network` gives the program Spark's own.
- **Sockets are files** (radix-file(2)), nodes of the pipes' detached
  filesystem as the Unix family's are (ADR 0085), so their descriptions
  are copied, passed and inherited the same way. A socket is made from
  a network capability (`socket`, which needs either right), and the
  methods that bind, connect or send somewhere new, `bind_address`,
  `connect_address` and `send_to`, **take the slot of a network
  capability and check it each time**: the socket itself grants
  nothing. `listen` needs a socket bound through a capability with the
  receive right for its port. A socket that connects or sends unbound is
  bound to a port the kernel picks first, which needs no receive right.
- **Ports are taken one socket each, per protocol, whatever the
  address.** The kernel picks from 49152 to 65535, from a random place
  the first time. A connection closed while it still has packets to
  send keeps the port the kernel picked for it until it has gone, so a
  new connection does not take the place of one still closing; a bound
  port is free once its socket is closed. The connections a listening
  socket takes hold none of their own.
- **The loopback interface, `lo`, is an interface like a card's**, on a
  link that hands back what is sent on it: a 64 KiB ring of packets, an
  MTU of 16 KiB, and smoltcp's IP medium, without Ethernet or ARP. It
  has 127.0.0.1/8 and every card's own address, so what a program sends
  to the machine itself goes through it.
- **A socket is smoltcp's sockets, one on each interface it is on**: a
  connection's on the interface it goes through; a datagram socket's on
  each interface its address is on; a listening socket's a pool of up to
  four listening smoltcp sockets on each, each of which becomes a
  connection, for `accept` to take and the pool to make again. A TCP
  socket gets 64 KiB each way, a UDP socket 32 KiB and 16 datagrams
  each way, lent from the heap and taken back when it closes (ADR 0182).
- **Calls wait on one count of networking's changes**, which an
  interface's thread moves after a poll that may have changed what a
  socket can do, and `poll` counts every network socket's pollers in
  one place. A call that cannot go on waits for the count to move, then
  looks again.
- **A socket closed while its connection is open sends its FIN and
  leaves the connection to its interface**, as an orphan the thread
  takes apart once it has gone, or resets after a minute. Connections a
  listening socket never gave out are reset.
- **Copies to and from a program's memory never hold a stack's lock
  while a page is read in**: they copy only what is there (`get_now`,
  `put_now`), and a page that is not is made after the lock is let go,
  then the copy is tried again. A datagram is copied in whole, under
  the socket's lock alone, before it is given to smoltcp, which sends
  whatever it is given; one received is taken only once it is copied.
- The errors are POSIX's: `address_in_use`, `address_not_available`,
  `reset`, `unreachable`, `in_progress`, `already`, `message_too_long`
  and `no_buffers` join the kernel's (EADDRINUSE, EADDRNOTAVAIL,
  ECONNRESET, ENETUNREACH, EINPROGRESS, EALREADY, EMSGSIZE, ENOBUFS).
  `socket_option` holds SO_REUSEADDR, SO_KEEPALIVE, TCP_NODELAY,
  SO_RCVBUF, SO_SNDBUF, SO_ERROR, IP_TTL, SO_BROADCAST, SO_TYPE,
  SO_RCVTIMEO and SO_SNDTIMEO.

## Consequences

- A program cannot reach the network unless something gave it a
  network capability, and cannot listen on a port outside the range it
  was given, whatever library it uses. Spark's grants are the whole of
  the network policy until the firewall (12c).
- A program that may only connect out still receives what is sent to
  the ports it was given: replies, and any datagram that reaches a
  datagram socket's port from anywhere, as on Linux. Turning away what
  no connection asked for is the firewall's work.
- The calls differ from Linux's in a few places, each kept small for
  the C library to bridge: listening needs a socket bound through a
  capability that allows its port, so `listen` on an unbound socket
  fails with missing_rights rather than picking a port; SO_REUSEADDR is
  kept and changes nothing; at most four handshakes per interface are
  under way at once for a listening socket, whatever its backlog; and a
  datagram socket bound to any address is on the interfaces there were
  when it was bound.
- Every waiting call looks again whenever anything on the network
  changes. That is cheap at 12a.2's rates and costs at a server's: 12a.3,
  which measures against Linux, gives each socket its own waits if the
  benchmark rows need it.
- A datagram is copied once more than a stream's bytes are, through a
  buffer of the socket's own of up to 32 KiB, made when it first sends.
- IPv6 sockets fail with not_supported until 12c.
- The `sockets` boot test runs TCP and UDP over `lo` and TCP through a
  virtio card to QEMU, and checks each refusal of the capability's.
