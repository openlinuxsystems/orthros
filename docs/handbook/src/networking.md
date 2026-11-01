<!--
SPDX-FileCopyrightText: 2026 The Dukovo contributors
SPDX-License-Identifier: Apache-2.0 OR MIT
-->

# Networking

This chapter describes networking in Dukovo as of Phase 12a.5 of the
roadmap, with the Intel cards of 12b. Phase 12 builds it up in steps:
sockets for programs, TCP that matches Linux, `netd` with DHCP, DNS and
`curl`, the time service, the Realtek, Intel and USB Ethernet cards,
IPv6, the firewall and WireGuard. This page grows with each.

## What works now

- **virtio network cards**, the ones QEMU and clouds on KVM give a
  virtual machine. The PCI scan finds each card and gives it a kernel
  thread, `net0`, `net1` and on, up to eight cards in all.
- **Intel's, VMware's, Amazon's and Google's cards**: Intel's e1000 and
  e1000e, the cards QEMU, VirtualBox and VMware give a machine by
  default, its igb, igc at 2.5 Gb/s and ixgbe at 10 Gb/s; VMware's
  VMXNET3; Amazon's ENA, on EC2; and Google's gVNIC, on Compute Engine
  (e1000(4), igb(4), igc(4), ixgbe(4), vmxnet3(4), ena(4), gve(4)).
  Every card is driven behind the IOMMU, through the same interface,
  and set up as a virtio card is
  ([ADR 0197](https://github.com/openlinuxsystems/orthros/blob/main/docs/adr/0197-network-cards-share-one-driver-interface-and-copy-frames-through-slots.md)).
  QEMU emulates the e1000, e1000e, igb and VMXNET3, which the
  `network-cards` boot test runs; the others are checked against
  models of the cards, and the first boot on each kind of machine is
  their real test.
- **The TCP/IP stack runs in the kernel**, built on
  [smoltcp](https://github.com/smoltcp-rs/smoltcp) in safe Rust
  ([ADR 0180](https://github.com/openlinuxsystems/orthros/blob/main/docs/adr/0180-the-tcp-ip-stack-runs-in-the-kernel-on-smoltcp.md)).
  It speaks Ethernet, ARP, IPv4, ICMP, TCP and UDP: it answers pings,
  the kernel's self-test pings with it, and programs connect, listen
  and send datagrams through it.
- **The loopback interface, `lo`**, at 127.0.0.1 and every card's own
  address: what a program sends to the machine itself goes through it,
  without touching a card. As on Linux, the program that sends a packet
  over it delivers it too, before its call returns, with no switch to
  another thread, and a program may connect to itself as often as it
  likes
  ([ADR 0185](https://github.com/openlinuxsystems/orthros/blob/main/docs/adr/0185-loopback-runs-in-the-senders-thread-and-the-network-joins-the-gate.md)).
  A TCP connection over `lo` passes its bytes straight from the sender
  into the receiver's buffer, in no packet, as Solaris's TCP fusion
  does, so a byte is copied twice, as on Linux
  ([ADR 0186](https://github.com/openlinuxsystems/orthros/blob/main/docs/adr/0186-a-loopback-connection-passes-its-bytes-straight-across.md)).
  `ob bench` holds TCP and UDP over `lo` to Linux's speed.
- **TCP behaves as Linux's** where a program or the other end can tell
  ([ADR 0187](https://github.com/openlinuxsystems/orthros/blob/main/docs/adr/0187-tcp-keeps-quiet-connections-and-controls-congestion-as-linux-does.md)):
  CUBIC congestion control from a window of ten segments, a 200 ms
  floor on the retransmission timeout, timestamps from a random start
  for each connection, and keep-alive probes after two hours of quiet,
  nine of them 75 seconds apart, which TCP_KEEPIDLE, TCP_KEEPINTVL and
  TCP_KEEPCNT change. A connection with nothing to send stays open
  however long it is quiet; one whose data goes unacknowledged for 15
  minutes is reset.
  A window that loses several segments resends each of them in about
  one round trip, from the SACK blocks the other end sends, and old
  duplicates are dropped by their timestamps (PAWS)
  ([ADR 0188](https://github.com/openlinuxsystems/orthros/blob/main/docs/adr/0188-tcp-recovers-from-losses-with-sack-and-an-interface-holds-thousands-of-sockets.md)).
- **Thousands of sockets on each interface**: up to 16,384, in slots the
  interface adds as they fill. A connection in TIME-WAIT that its
  program closed keeps none of its buffers, and a client may connect
  again from the same port at once, as on Linux (ADR 0188).
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
- **Tested against broken and hostile networks**: `ob test` gives two
  stacks 100,000 frames each, changed at random, alongside their own
  traffic; neither may panic or send a malformed frame, and a new
  connection between them must work afterwards. Kani proves that the
  stack reads any Ethernet, ARP, IPv4, ICMP and UDP header, and any TCP
  option, without a panic and writes it back the same
  ([ADR 0189](https://github.com/openlinuxsystems/orthros/blob/main/docs/adr/0189-frames-changed-at-random-break-neither-stack-and-kani-proves-the-headers.md)).

- **The C library's BSD sockets**: `socket`, `bind`, `listen`,
  `connect`, `accept`, `send`, `recv`, `sendto`, `recvfrom`, `sendmsg`,
  `recvmsg`, `getsockname`, `getpeername`, `getsockopt`, `setsockopt`
  and `shutdown` work on `AF_INET` TCP and UDP sockets, as radix-posix(7)
  describes. A call the program's capabilities do not allow fails with
  `EACCES`
  ([ADR 0184](https://github.com/openlinuxsystems/orthros/blob/main/docs/adr/0184-services-and-the-c-library-use-the-network-capabilities-they-were-given.md)).
- **Services get the network their files ask for**: under
  `[permissions]`, a service file's `network.outbound = true` lets it
  connect out, and `network.listen` names the ports it may listen on
  (service.toml(5)). A service that asks for
  neither cannot use the network.

- **`ip` and `ss`**, with iproute2's words and output: `ip address`,
  `ip link` and `ip route` show the interfaces, their addresses and
  routes, `ip address add` and `ip route add default via` set a card
  up, and `ss` lists the TCP and UDP sockets
  ([ADR 0190](https://github.com/openlinuxsystems/orthros/blob/main/docs/adr/0190-interfaces-are-shown-as-linux-shows-them-and-set-up-through-the-network-capability.md)).
  The interfaces are in `/kernel/class/net` and the counts, routes and
  sockets in `/processes/net`, in Linux's formats.
- **`netd` sets every card up by DHCP** at boot, or as
  `/system/settings/network` says, keeps each lease across restarts and
  reboots, and writes the DNS servers where `/etc/resolv.conf` leads
  ([ADR 0191](https://github.com/openlinuxsystems/orthros/blob/main/docs/adr/0191-netd-sets-each-card-up-by-dhcp-or-as-its-settings-say.md)).
  What a DHCP server says is checked before it reaches the routes or
  the resolver.
- **TLS for programs in Rust**, rustls on ring, and the root
  certificates Mozilla trusts in
  `/system/resources/certificates/ca-certificates.crt`, which
  `/etc/ssl/certs/ca-certificates.crt` and `/etc/ssl/cert.pem` lead to
  ([ADR 0192](https://github.com/openlinuxsystems/orthros/blob/main/docs/adr/0192-tls-is-rustls-on-ring-and-mozillas-roots-are-a-file-of-the-systems.md)).
  dukovo-certificates(7) says how to trust an authority of your own.
- **`resolved` looks every name up**, at 127.0.0.53, which
  `/etc/resolv.conf` names: it keeps answers as long as they say, asks
  DHCP's servers or your own in turn, over DNS over TLS when they
  answer it, and over DNS over HTTPS when you name an `https://` URL
  ([ADR 0193](https://github.com/openlinuxsystems/orthros/blob/main/docs/adr/0193-resolved-caches-every-programs-lookups-and-asks-over-tls-when-it-can.md)).
- **`timed` keeps the clock right** over Network Time Security, which
  authenticates every answer, and writes the real-time clock; the IANA
  time zone database and English's locales come with it
  ([ADR 0194](https://github.com/openlinuxsystems/orthros/blob/main/docs/adr/0194-the-clock-is-a-capability-and-timed-sets-it-over-nts.md)).
- **curl and libcurl**, 8.23, on rustls, with zlib and zstd
  ([ADR 0195](https://github.com/openlinuxsystems/orthros/blob/main/docs/adr/0195-curl-and-libcurl-use-rustls-through-rustls-ffi.md)).
- **`netlog` sends the log to another computer** over UDP as it is
  written, to debug a machine whose screen shows nothing
  ([ADR 0196](https://github.com/openlinuxsystems/orthros/blob/main/docs/adr/0196-netlog-sends-the-kernel-log-over-udp.md)).

Native programs use the sockets through radix-network(2) and
radix-file(2).

## Giving a service the network

A service file asks for what it needs, and gets nothing else:

```toml
service_version = 1
description = "A web server"
program = "/system/commands/httpd"

[permissions]
network.outbound = false
network.listen = [80, 443]
```

Spark derives one capability for each range from its own and puts them
in the service's network slots. The service can bind to and listen on
ports 80 and 443, and nothing more: binding another port, connecting
out or sending a datagram anywhere fails with `EACCES`.

## Setting the cards up

netd(8) runs on every machine and asks DHCP for each card's address,
router and DNS servers, with nothing to configure. The system's log
says what it got:

```
netd: net0: asking for an address by DHCP
netd: net0 has 10.0.2.15/24 from 10.0.2.2 for 1 day, router 10.0.2.2, DNS 10.0.2.3
```

To give a card an address of its own, name it in
`/system/settings/network` (network(5)), by its name or its MAC
address, and have netd read the file again:

```
# cat >> /system/settings/network
[net0]
address = 192.168.1.10/24
router = 192.168.1.1
dns = 192.168.1.1
# service netd reload
```

`dhcp = no` in a card's section, with no address, has netd leave it
alone. netd changes only the addresses it gave, so one added with ip(8)
stays. netd writes the DNS servers it was given to
`/system/live/netd/resolv.conf`, which resolved asks (see [DNS](#dns)
below); to use DNS servers of your own, give a card `dns =` settings, or
name them in `/system/settings/resolved`.

The `dhcp` boot test has netd set a card up by QEMU's DHCP server,
restarts netd, and changes the settings to an address of their own and
back:

```
./ob test --case dhcp
```

## Giving a card an address at boot

The first card can also take its address from two kernel flags
(dukovo-boot(7)), and netd then leaves it as it is:

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
does. QEMU's `e1000`, `e1000e`, `igb` and `vmxnet3` cards work the same
way in place of `virtio-net-pci`, and `ob test --case network-cards`
runs them all at once, with the e1000's 82545EM, each on a network of
its own.

## Setting a card up by hand

At the console, root's shell may set a card up with ip(8), as on
Linux; a service may when its file asks for `admin.network = yes`
(spark(8)). With QEMU's user network:

```
# ip address add 10.0.2.15/24 dev net0
# ip route add default via 10.0.2.2
# ip -br address
lo               UNKNOWN        127.0.0.1/8
net0             UP             10.0.2.15/24
# ip route
default via 10.0.2.2 dev net0
10.0.2.0/24 dev net0 proto kernel scope link src 10.0.2.15
```

Each card takes up to four addresses, the first the one it sends from,
and a router. `ip address del`, `ip address flush` and `ip route del
default` take them away again; `ss -tl` lists what listens. The
`interfaces` boot test does the same with the address QEMU's DHCP
server offers, which a socket bound to the bare card asks for:

```
./ob test --case interfaces
```

## DNS

Every program looks names up through resolved(8): the C library's
`getaddrinfo` finds a name in `/etc/hosts` first, which names localhost,
and otherwise reads `/etc/resolv.conf`, which names 127.0.0.53, where
resolved answers. resolved asks the servers DHCP gave unless
`/system/settings/resolved` (resolved(5)) names others, and keeps each
answer as long as it says. `resolvectl` shows what it asks and how, and
looks names up through it:

```
$ resolvectl query dukovo.org
dukovo.org.	300	IN	A	192.0.2.10
-- 1 record in 31 ms
$ resolvectl
Listening on: 127.0.0.53:53 (UDP and TCP)
DNS over TLS: opportunistic
Cache: 112 of 4096 answers
Queries: 431, 302 from the cache, 6 answered here, 0 failed
Server: 10.0.2.3 (from DHCP): plain DNS over UDP; 123 answered, 0 failed
```

A server DHCP gave is asked over DNS over TLS first, on port 853, and
over plain DNS if it does not answer there. To have every name go
encrypted to servers you trust, name them with the name their
certificate carries, or by an `https://` URL for DNS over HTTPS, and
read the file again:

```
# cat >> /system/settings/resolved
servers = 9.9.9.9#dns.quad9.net https://cloudflare-dns.com/dns-query#1.1.1.1
# service resolved reload
```

`localhost` is answered on the machine, and `.local` names never leave
it. `resolvectl flush-caches` empties the cache.

## The time and the time zone

timed(8) sets the clock from the network at boot and keeps it right,
asking Cloudflare's, Netnod's and PTB's servers over Network Time
Security, so that nobody on the way can change the time; it writes the
real-time clock too. `timedatectl` shows the time, the zone and what
timed did, and sets the zone:

```
$ timedatectl
               Local time: Sun 2026-11-01 12:00:00 UTC
           Universal time: Sun 2026-11-01 12:00:00 UTC
                 RTC time: Sun 2026-11-01 12:00:00
                Time zone: UTC (UTC, +0000)
System clock synchronized: yes
              NTP service: active
          RTC in local TZ: no
# timedatectl set-timezone Europe/Berlin
$ date
Sun Nov  1 13:00:00 CET 2026
```

`timedatectl list-timezones` lists the zones, and the `TZ` variable
sets one for a single program, as `TZ=America/New_York date`. Other time
servers, or none, go in `/system/settings/timed` (timed(5)). The
locales are English's in each of its countries, `en_US.UTF-8`,
`en_GB.UTF-8`, `en_IN.UTF-8` and the others, with their dates,
numbers and sorting order; `LANG=en_GB.UTF-8` picks one.

## Fetching with curl

curl(1) and libcurl fetch over HTTP and HTTPS with the system's root
certificates, as on Linux:

```
$ curl -sI https://dukovo.org
HTTP/1.1 200 OK
```

A port links libcurl, a static library, from the sysroot, where
`libcurl.pc` gives pkg-config(1) its flags. HTTP/2, HTTP/3 and
international domain names are to come.

## Sending the log to another computer

A machine whose screen shows nothing can still send its log. On
another computer on the network, listen:

```
nc -klu 6666
```

and boot the machine with `netlog=192.168.1.20` on its kernel command
line (dukovo-boot(7)), or name the target in `/system/settings/netlog`
(netlog(5)) to keep it. netlog(8) sends every record from the start of
boot, each as one line with its priority and time, as a syslog server
takes them. The log goes unencrypted, so send it only across a network
you trust with it.

## Trying the network services

The `network-services` boot test runs, on `lo`, a DNS server, a DNS
over TLS and an HTTPS server, a Network Time Security server and a
log sink, and checks resolved, resolvectl, curl, timed, timedatectl,
netlog, the time zones and the locales against them:

```
./ob test --case network-services
```

## Trying TLS

The `tls` boot test runs `tls-test`, which checks ring's hashes and
ciphers against answers OpenSSL gave, OpenSSL's signatures, rustls's
handshakes over `lo` with each version, cipher suite, key exchange
group and kind of key, the certificates a client must refuse, and the
system's root certificates:

```
./ob test --case tls
```

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
