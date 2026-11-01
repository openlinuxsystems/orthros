<!--
SPDX-FileCopyrightText: 2026 The Dukovo contributors
SPDX-License-Identifier: Apache-2.0 OR MIT
-->

# 0193. resolved caches every program's lookups, and asks over TLS when it can

- Status: Accepted
- Date: 2026-11-01

## Context

The roadmap's 12a asks for `resolved`: "caching, multiple resolvers,
DNS-over-TLS and HTTPS". Until now each program's C library read
`/etc/resolv.conf`, which led to the file netd writes from DHCP (ADR
0191), and asked those servers itself, over plain DNS: every lookup of
every program went over the network in the clear, nothing was kept
between programs, and a server that did not answer cost each program
its own timeout.

Linux distributions solve this with a local stub resolver: systemd's
systemd-resolved on 127.0.0.53, or dnsmasq or Unbound on 127.0.0.1,
which `/etc/resolv.conf` names. The C library and every program that
reads that file, Go's and others', then ask it without change.

DNS over TLS (RFC 7858) and DNS over HTTPS (RFC 8484) keep lookups from
the network on the way. Few DHCP-given servers speak them, home routers
least of all; the public resolvers that do are named by their
certificate's name, as `9.9.9.9#dns.quad9.net`, systemd-resolved's
syntax. RFC 7858 describes an opportunistic profile for servers known
only by address: TLS without checking the certificate, which stops
whoever reads the network but not whoever pretends to be the server.

## Decision

- **resolved is the machine's one resolver,** a service of systools
  (ADR 0096) Spark starts just after netd (order 21). It answers on
  127.0.0.53, port 53, over UDP and TCP, and writes
  `/system/live/resolved/resolv.conf`, which names that address and
  netd's search domains. The default settings' `/etc/resolv.conf`
  leads there now, not to netd's file, which resolved reads instead.
  Every service's view holds `/system/live/resolved` read-only, beside
  netd's folder (service.toml(5)), so that every C library finds it.
- **The servers are the settings' or else DHCP's,** up to eight, asked
  in turn: one that fails or does not answer within two seconds is
  passed over for half a minute. `/system/settings/resolved`
  (resolved(5)) names them as systemd-resolved does: `ADDRESS[:PORT]`,
  `ADDRESS#NAME` for DNS over TLS checked for that name, and, beyond
  systemd's syntax, `https://HOST[:PORT]/PATH#ADDRESS` for DNS over HTTPS
  by POST over HTTP/1.1. Connections over TLS are kept for the next
  queries, by four worker threads.
- **A server named by address alone is asked over TLS first,**
  opportunistically, on port 853, and over plain DNS if it does not
  answer there, trying TLS again an hour later: `dns-over-tls =
  opportunistic`, the default. `yes` asks such servers over TLS only,
  their certificates checked for their addresses; `no` over plain DNS
  only. TLS is rustls on ring with the system's roots and the
  settings' `certificates` (ADR 0192).
- **Answers are kept** for as long as their records' times to live say,
  a day at most, and negative ones as long as their SOA says (RFC
  2308), an hour at most, 4096 by default (`cache-size`), keyed by the
  question and its DNSSEC flags; an answer from the cache has its times
  counted down. SIGHUP empties the cache with the settings read again,
  and SIGUSR2 alone (resolvectl's `flush-caches`).
- **Plain queries are hard to answer falsely:** each from a port the
  kernel picks at random, with a random identifier, an answer taken only
  for the question asked; over UDP they offer 1232 bytes (the DNS flag
  day 2020's size), and an answer cut short is asked for again over
  TCP. A client's answer is cut to the size its query offered.
- **localhost, .local and invalid never leave the machine** (RFC 6761
  and 6762): localhost and the names below it, and their reverse names,
  are answered as 127.0.0.1 and ::1; `.local` and `invalid` names as not
  there, until mDNS answers `.local` (12e).
- **resolvectl(1)** shows `/system/live/resolved/status`, which
  resolved writes as it changes (each server, where it came from, how it
  was asked last, its answers and failures, and the cache), looks names
  up through resolved as dig(1) shows them (`query`, `-t TYPE`), and
  empties the cache, with systemd's command names.
- **The C library asks as glibc's does** (mlibc patch 0052): mlibc's
  lookups found DNS's port in `/etc/services`, which the system did not
  have, sent every query with the same identifier, and read answers
  without checking their offsets. Now they ask port 53 with a random
  identifier and EDNS(0), take only an answer from the server asked to
  the question asked, ask again over TCP for one cut short, follow
  CNAMEs, and use resolv.conf's three nameservers, search list and
  `ndots` as glibc does. The default settings now hold a `hosts` file
  naming localhost and a `services` file of the well-known ports
  programs name, from IANA's registry.
- **The DNS messages are a library crate, netproto,** with no unsafe
  code, which resolved, resolvectl, timed (ADR 0194) and the boot test's
  servers share.

## Consequences

- A name a program looks up again is answered from the machine, in
  microseconds; a server that stops answering costs one timeout, not
  one in each program.
- On a network whose DHCP server speaks no DNS over TLS, which is most,
  lookups still go over plain DNS, as before, after one failed
  connection to port 853 an hour. Encrypted lookups need servers named
  in the settings; the handbook shows how. Nothing yet offers them in
  the first-boot setup.
- An install from before this change keeps its `/etc/resolv.conf` link
  to netd's file, as Spark never replaces a setting that is there
  (spark(8)): its programs ask DHCP's servers directly, as before, and
  resolved idles. netd's folder therefore stays in every view, and an
  update can move the link once updates exist (Phase 13).
- resolved does not validate DNSSEC; it passes the DO and CD bits on
  and keeps such answers apart, so that a validating program still can.
  IPv6 servers are read but reachable only with IPv6 (12d).
- Lookups name which servers a machine trusts: the privacy page
  (dukovo-privacy(7)) lists what resolved sends, only what programs ask.
- Tests: netproto's and resolved's unit tests (settings, servers,
  resolv.conf, the cache, answers cut and aged), and the
  `network-services` boot test: lookups through the C library over
  plain DNS, TLS, HTTPS and opportunistic TLS against servers of its
  own on lo, the cache and negative cache, answers too long for UDP from
  a server and to a client, localhost and `.local`, a certificate for
  another name and one from an untrusted authority refused, and
  `flush-caches`. The `dhcp` test checks the new link.

## Alternatives considered

- **systemd-resolved:** it needs systemd and D-Bus, which Dukovo does
  not run, and its C would parse what the network sends.
- **Unbound or dnsmasq:** C that parses what the network sends, again;
  Unbound is a full recursive resolver with DNSSEC, more than a client
  machine needs today, and dnsmasq's DNS over TLS needs a proxy beside
  it. resolved shares rustls, the roots and its DNS code with the rest
  of the system.
- **A cache in the C library:** one per process, so a program's first
  lookup always goes out, and TLS in every program that looks a name up.
- **Strict DNS over TLS by default, to public resolvers:** private, but
  it sends every lookup to a third party the user did not choose, and
  breaks the names a local network's own server answers. The user
  chooses that in the settings.
