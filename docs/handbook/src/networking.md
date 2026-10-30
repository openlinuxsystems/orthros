<!--
SPDX-FileCopyrightText: 2026 The Dukovo contributors
SPDX-License-Identifier: Apache-2.0 OR MIT
-->

# Networking

This chapter describes networking in Dukovo as of Phase 12a.2 of the
roadmap. Phase 12 builds it up in steps: sockets for programs, TCP
that matches Linux, `netd` with DHCP, DNS and `curl`, the time service,
the Realtek, Intel and USB Ethernet cards, IPv6, the firewall and
WireGuard. This page grows with each.

## What works now

- **virtio network cards**, the ones QEMU and clouds on KVM give a
  virtual machine, up to eight of them. The PCI scan finds each one and
  gives it a kernel thread, `net0`, `net1` and on.
- **The TCP/IP stack runs in the kernel**, built on
  [smoltcp](https://github.com/smoltcp-rs/smoltcp) in safe Rust
  ([ADR 0180](https://github.com/openlinuxsystems/orthros/blob/main/docs/adr/0180-the-tcp-ip-stack-runs-in-the-kernel-on-smoltcp.md)).
  It speaks Ethernet, ARP, IPv4, ICMP, TCP and UDP: it answers pings,
  the kernel's self-test pings with it, and programs connect, listen
  and send datagrams through it.
- **The loopback interface, `lo`**, at 127.0.0.1 and every card's own
  address: what a program sends to the machine itself goes through it,
  without touching a card.
- **Sockets for programs, through network capabilities**
  ([ADR 0183](https://github.com/openlinuxsystems/orthros/blob/main/docs/adr/0183-programs-reach-the-network-through-capabilities-of-its-ports.md)).
  A program uses the network only if it was given a network
  capability, a range of ports with two rights: *send*, to connect out
  and send datagrams anywhere, and *receive*, to bind to and listen on
  the ports in its range. The kernel checks the capability on every
  bind, connect and send, so no library can skip it. The kernel gives
  Spark one for all ports; Spark hands out narrower ones.
- **Each card reaches only its own memory** behind the IOMMU: the
  driver copies frames through slots of DMA memory that belong to the
  card
  ([ADR 0181](https://github.com/openlinuxsystems/orthros/blob/main/docs/adr/0181-virtio-net-copies-frames-through-slots-of-its-own.md)).
- **smoltcp is patched** so that a socket's memory goes back to the
  kernel when the socket closes, and so that a full socket table or a
  stale socket never panics the kernel
  ([ADR 0182](https://github.com/openlinuxsystems/orthros/blob/main/docs/adr/0182-the-kernel-patches-smoltcp-so-socket-memory-comes-back.md)).
  `./ob crates` fetches it from crates.io and applies the patch, and
  every build does that first.

- **The C library's BSD sockets**: `socket`, `bind`, `listen`,
  `connect`, `accept`, `send`, `recv`, `sendto`, `recvfrom`, `sendmsg`,
  `recvmsg`, `getsockname`, `getpeername`, `getsockopt`, `setsockopt`
  and `shutdown` work on `AF_INET` TCP and UDP sockets, as radix-posix(7)
  describes. A call the program's capabilities do not allow fails with
  `EACCES`
  ([ADR 0184](https://github.com/openlinuxsystems/orthros/blob/main/docs/adr/0184-services-and-the-c-library-use-the-network-capabilities-they-were-given.md)).
- **Services get the network their files ask for**: a service file's
  `network.outbound = yes` lets it connect out, and `network.listen`
  names the ports it may listen on (spark(8)). A service that asks for
  neither cannot use the network.

Native programs use the sockets through radix-network(2) and
radix-file(2). Nothing configures the network by itself yet: there is
no DHCP until `netd` lands.

## Giving a service the network

A service file asks for what it needs, and gets nothing else:

```
description = A web server
program = /system/commands/httpd
network.outbound = no
network.listen = 80, 443
```

Spark derives one capability for each range from its own and puts them
in the service's network slots. The service can bind to and listen on
ports 80 and 443, and nothing more: binding another port, connecting
out or sending a datagram anywhere fails with `EACCES`.

## Giving a card an address

The first card takes its address from two kernel flags
(dukovo-boot(7)):

```
ip=10.0.2.15/24 gateway=10.0.2.2
```

Add them to `boot.cmdline` in `dukovo.toml`, or pass them with
`./ob run --cmdline`. QEMU's user network, which `ob run` does not add
by itself, hands out 10.0.2.15 and routes through 10.0.2.2. To try it:

```
./ob run --cmdline "selftest=net" -- -netdev user,id=n0 \
    -device virtio-net-pci,netdev=n0,disable-legacy=on
```

The log shows the card, its address and four pings to the router:

```
Net: net0 is a virtio network card at 0000:00:03.0, MAC 52:54:00:12:34:56, 256 receive and 256 transmit slots, MSI-X vector 0x52
Net: net0 has the address 10.0.2.15/24, through the router 10.0.2.2
Net: 10.0.2.2 answered ping 1 in 7.792 ms
selftest net: passed; the router answered 4 pings, the fastest in 1.390 ms
```

`ob test --case net` runs the same test behind QEMU's IOMMU, as CI
does.

## Trying the sockets

The `sockets` boot test starts a test program with Spark's network
capability (`--network`). The program derives one capability that may
only connect out and one that may only listen on ports 8000 to 8009,
then runs TCP and UDP over `lo` and TCP through a virtio card to an
echo server QEMU runs, and checks that each call refuses what its
capability does not allow:

```
./ob test --case sockets
```

Its log reads:

```
sockets: the network capability covers 65536 ports from 0
sockets: derived one capability to connect out and one to listen on ports 8000-8009
sockets: tcp: connected from port 52311 to port 7000 over lo, and the connection taken
sockets: tcp: 1048576 bytes went one way and came in order
sockets: udp: datagrams went each way over lo, kept their bounds, and said where they came from
sockets: rights: each method refused what its capability does not allow (process 12)
sockets: virtio: a connection to 10.0.2.100 port 7 echoed what went through the card
sockets: every check passed
```
