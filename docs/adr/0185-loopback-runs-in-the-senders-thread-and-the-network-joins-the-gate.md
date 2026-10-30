<!--
SPDX-FileCopyrightText: 2026 The Dukovo contributors
SPDX-License-Identifier: Apache-2.0 OR MIT
-->

# 0185. Loopback runs in the sender's thread, and the network joins the gate

- Status: Accepted
- Date: 2026-10-30

## Context

Every row of `ob bench` must cost no more than the same operation on
Linux in the same virtual machine (ADR 0107), and networking had no
rows yet. Its rows have to measure the stack, not a card or a host, so
they go over the loopback interface, as most of Linux's own network
benchmarks do.

The loopback interface (ADR 0183) ran like a card: a thread of its own
polled its stack, and whoever queued work on it woke that thread. Each
packet a program sent to itself cost a switch to that thread and back
on one CPU, or an interrupt to another. Linux runs loopback in the
sender's context instead: `loopback_xmit` queues the packet, and the
softirq on the way out of the same system call receives it, on the
same CPU.

Short connections ran out of room. Each interface has 128 socket slots,
and a connection a program closes keeps its slot, as an orphan, until
it has gone: through TIME-WAIT, ten seconds in smoltcp. A program that
connects to itself 2,000 times in a row was refused after about a
hundred. And a FIN that came in FIN-WAIT after data smoltcp had not yet
acknowledged waited for its delayed-ACK timer, 10 ms, so the other end
sat in LAST-ACK for that long.

## Decision

- **Whoever queues work on the loopback interface polls it**, in its
  own thread, before its system call returns: the packet a program sends
  to itself is received, and the program it wakes is woken, by then.
  `Interface::kick` does it for `lo`, and wakes a card's thread as
  before.
- **`lo`'s thread only wakes for timers.** It keeps when it next means
  to wake, set under the stack's lock; a poll in another thread that
  leaves a timer due sooner (a retransmission, a delayed ACK, an
  orphan's deadline) wakes it.
- **`lo` makes and checks no checksums, and takes packets of 65,535
  bytes**, as Linux's loopback skips them and has an MTU of 65,536: its
  packets never leave memory, and a socket's whole window of 64 KiB goes
  as one packet. Every card's packets still have theirs made and
  checked.
- **An orphan on the loopback interface goes once it reaches
  TIME-WAIT**, with its memory and slot. TIME-WAIT guards against a lost
  last ACK and against stray segments of the old connection, and the
  loopback link loses and delays nothing. A card's orphans keep their
  TIME-WAIT.
- **smoltcp acknowledges a FIN in FIN-WAIT at once**, as Linux's
  `tcp_fin` does. The patch of ADR 0182 does it, to be offered upstream;
  it is also what makes taking TIME-WAIT apart at once safe, since the
  ACK has gone by then.
- **Six rows join the gate**, from `bench.c` with `--network`: a
  connection made, accepted and closed (`posix_tcp_connect`), a byte
  there and back over TCP and a datagram over UDP, on one CPU and on two
  (`posix_tcp_roundtrip_same_cpu` to `posix_udp_roundtrip_cross_cpu`),
  and TCP throughput in 64 KiB writes (`posix_tcp_mib`), against the
  same program on Linux. Spark gives the benchmark a network capability;
  `linux-bench`, the whole of Linux's user space in its virtual machine,
  brings `lo` up first.

## Consequences

- A packet over `lo` costs no switch and no interrupt, as on Linux; a
  round trip between two programs on one CPU costs the two switches
  between them and no more.
- The stack's lock is now taken by every sender to `lo`, in its own
  thread, and its poll runs there: `kick` is called with no stack's lock
  held (it may be called with a socket's, which comes first in the lock
  order).
- A program can connect to itself as fast as it likes. On a card, short
  connections are still bounded by 128 slots over TIME-WAIT's ten
  seconds; 12a.3b, which hardens the TCP for production, gives the
  stack more slots and TIME-WAIT a smaller form.
- The inet test checks that a socket bound to port 0 has its port at
  once and that `TCP_NODELAY` may be set before connecting, as the
  benchmark does; radix-net's tests check that a loopback connection
  ends without waiting for a timer.
