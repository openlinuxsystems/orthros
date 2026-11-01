<!--
SPDX-FileCopyrightText: 2026 The Dukovo contributors
SPDX-License-Identifier: Apache-2.0 OR MIT
-->

# 0196. netlog sends the kernel log over UDP

- Status: Accepted
- Date: 2026-11-01

## Context

The roadmap's 12a asks for netlog: it "streams kernel and service logs
over UDP to another machine for debugging on the reference machines".
A laptop whose screen stays dark, or that hangs before logd has
written the log to disk, tells nothing; the Dell has no serial port.
Linux has netconsole, which sends the kernel's messages from the
network driver itself, as UDP datagrams, to a host and port named on
the kernel command line (`netconsole=`), port 6666 by convention; a
listener as simple as `nc -lu 6666` shows them.

Dukovo's kernel log already holds the services' output: Spark copies
each service's lines into it (logd(8)), and `/devices/kmsg` gives every
record, with its priority and time, from the first the kernel still
holds, as Linux's does.

## Decision

- **netlog(8) is a service of systools** (ADR 0096) that Spark starts
  after resolved (order 23), holding the kernel log to read and the
  network to send through. It reads `/devices/kmsg` from the first
  record the kernel holds, and sends each record as one UDP datagram:
  `<PRIORITY>[SECONDS.MICROSECONDS] TEXT`, a syslog server's priority
  and dmesg(1)'s time, cut to 1400 bytes at a character's end.
- **The target** is `netlog=HOST[:PORT]` on the kernel command line, for
  one boot, which the kernel lists as a flag of netlog's and leaves to
  it, or else `target` in `/system/settings/netlog` (netlog(5)); the port
  is 6666 unless it is given. `level` sends only records as urgent as it
  or more. Without a target, the default, netlog sends nothing and
  waits for SIGHUP, which reads the settings and the command line again.
- **Nothing is lost while the network comes up:** a record that cannot
  be sent yet, before the card has an address or a route, is tried
  again each second, and the records after it wait behind it, so that
  the target sees the boot from its start. Records go at most a
  thousand a second: the kernel holds up to 512 when netlog starts, and
  a listener whose queue they overran at once would lose them, as UDP
  drops what a full queue cannot take. A target whose port is closed
  loses what is sent to it, as with any UDP listener.

## Consequences

- With `netlog=192.168.1.20` on its kernel command line, the Dell's whole
  boot, the kernel's and the services' lines, reaches another computer
  as soon as netd has an address, which is what Phase 12's driver work
  on the reference machines needs.
- netlog runs in userspace, so it sends nothing before Spark starts it,
  or after a panic or once Spark has stopped; netconsole, in the
  kernel, sends those too. The crash log (radix-debug(7)) still keeps
  the last boot's log in memory for the next one.
- The datagrams are neither encrypted nor authenticated: the log goes
  across the network in the clear, so netlog is for a network the user
  trusts with it. It is off unless named, and the privacy page lists it
  (dukovo-privacy(7)).
- Records the kernel dropped before netlog read them, on a very busy
  boot, are lost, as from dmesg.
- IPv6 targets wait for IPv6 (12d).
- Tests: netlog's unit tests (settings, `netlog=`, targets, the
  datagram's format and its cut at a character's end), and the
  `network-services` boot test, whose sink on lo must get the oldest
  record the kernel still holds, written before netlog had a target,
  and a line logger(1) writes, with its priority.

## Alternatives considered

- **netconsole in the kernel:** it would reach a panic's last lines,
  but needs UDP sending from the kernel's network thread without
  sockets or allocation, and a second way of naming targets. It can come
  after 1.0, beside the crash log.
- **syslog's own protocol (RFC 5424) over TCP or TLS:** reliable and
  private, but a log server to set up first, where netconsole's format
  needs only `nc`. logd may forward to a syslog server later.
- **Only the services' lines, from logd:** logd starts later than the
  kernel's first records, and keeps them in its own format.
