<!--
SPDX-FileCopyrightText: 2026 The Dukovo contributors
SPDX-License-Identifier: Apache-2.0 OR MIT
-->

# 0187. TCP keeps quiet connections, and controls congestion, as Linux does

- Status: Accepted
- Date: 2026-10-31

## Context

The kernel's TCP is smoltcp's (ADR 0180), patched (ADR 0182). Before
programs use it on real networks, it must behave as a production TCP
does, and as Linux's does, where a program or the other end can tell.
Looking at it with that in mind found:

- **Quiet connections were reset.** The kernel gives every connection a
  timeout of 15 minutes for answers, as Linux's `tcp_retries2` gives
  about. smoltcp counts it from the last segment received, whether or
  not the socket waits for anything: a connection with nothing to send
  and nothing owed, an SSH session left alone, was reset 15 minutes
  after its last packet, the next time the stack was polled. A test in
  radix-net shows it.
- **No congestion control.** smoltcp controls congestion only with one
  of two features, which the kernel did not turn on: a connection sent
  as fast as the other end's window let it, on any network. smoltcp's
  CUBIC computes in floating point, which the kernel, built without it,
  does in software at every ACK. It also began with a window of two
  segments; Linux begins with ten (RFC 6928).
- **A loss on a fast link cost a second.** smoltcp's RTO is never less
  than RFC 6298's 1 second; Linux's is never less than 200 ms.
- **Keep-alive probed once.** smoltcp sends a keep-alive packet after
  each interval of quiet, so a connection with SO_KEEPALIVE sent one
  probe after two hours and was reset when the timeout ran out after
  it; Linux sends nine, 75 seconds apart, and programs set the three
  numbers with TCP_KEEPIDLE, TCP_KEEPINTVL and TCP_KEEPCNT, which the
  kernel refused.
- **No timestamps.** smoltcp can send them (RFC 7323), but only from one
  clock for every connection, which tells the other end how long the
  machine has been up, and it takes no RTT samples from their echoes.

## Decision

- **The timeout runs only while a connection waits for an answer**: a
  SYN or FIN not acknowledged, data to send or not acknowledged, or
  keep-alive packets with no count of their own. It counts from the
  later of the last segment received and the moment the connection
  began to wait. A connection quiet with nothing owed lasts, as on
  Linux.
- **TCP's congestion control is CUBIC (RFC 9438), as Linux's is**, in
  the kernel's patch to smoltcp's CUBIC: the same curve in integers
  (microseconds, an integer cube root at each loss, the Reno-friendly
  window in 1/65,536 bytes), and an initial window of ten segments.
- **The RTO's floor is 200 ms**, Linux's `TCP_RTO_MIN`; the first RTO is
  still 1 second.
- **Keep-alive works as Linux's**: after TCP_KEEPIDLE seconds of quiet
  (7,200), probes go TCP_KEEPINTVL seconds apart (75), and TCP_KEEPCNT
  unanswered (9) reset the connection. The three options are new in
  `socket_option` and mlibc. A connection with data unacknowledged
  still waits 15 minutes for an answer, as before.
- **Connections send timestamps**, in milliseconds, each from a random
  start of its own (RFC 7323 section 7.1), as Linux's do. An ACK of new
  data that echoes one is an RTT sample (RFC 7323 section 4), so the RTO
  follows every ACK, not one segment a round trip.

## Consequences

- smoltcp's own TCP tests pass with the patch, with CUBIC, with Reno and
  with neither, and new ones check each change: a quiet connection past
  its timeout, a FIN sent after the quiet, the probes and their count,
  setting keep-alive again, a short RTT's RTO, the timestamps' offset
  and echoes, the initial window, and the integer cube root and curve
  against RFC 9438's numbers.
- A connection over a card now grows its window from ten segments, and
  halves it to 70% at a loss, where it sent the whole window before. The
  loopback rows of `ob bench` pass their bytes across (ADR 0186) and see
  CUBIC only in the handshake.
- Each segment carries 12 bytes more, the timestamp option, as on Linux.
- A connection made, accepted and closed over `lo` takes 50,028
  instructions under QEMU's count, 5,200 more than before: the
  timestamps 1,900 of them, CUBIC and the random start the most of the
  rest. Its budget in `ob bench` follows, at 56,000; on CI it took 0.38
  of Linux's time before.
- Still to come for TCP, in 12a.3c: SACK's use in recovery (smoltcp
  sends SACK blocks but ignores those it receives), PAWS, more sockets
  per interface and a smaller TIME-WAIT.
