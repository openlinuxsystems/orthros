<!--
SPDX-FileCopyrightText: 2026 The Dukovo contributors
SPDX-License-Identifier: Apache-2.0 OR MIT
-->

# 0188. TCP recovers from losses with SACK, and an interface holds thousands of sockets

- Status: Accepted
- Date: 2026-10-31

## Context

ADR 0187 made the kernel's TCP (smoltcp's, patched: ADRs 0180, 0182)
behave as Linux's in its timers and its congestion control. Four gaps
were left for a production TCP:

- **One loss at a time.** smoltcp offers SACK (RFC 2018) and the other
  end sends SACK blocks, but smoltcp reads them and does nothing with
  them. After the third duplicate ACK it sends the first segment not
  acknowledged again; any other segment lost from the same window waits
  for the RTO, a second at first, and the window falls to one segment.
  The ACK that the first retransmission brings, a partial ACK, sends
  nothing either. Linux retransmits each segment lost in one round trip
  (RFC 6675). As a receiver, smoltcp sends only one SACK block, where
  there is room for three.
- **No PAWS.** With timestamps on (ADR 0187), RFC 7323 has a segment
  whose timestamp is older than the last one recorded dropped as an old
  duplicate: from before the sequence numbers wrapped, which takes 34
  seconds at 1 Gbit/s, or from an earlier connection of the same
  endpoints. smoltcp records the timestamp of every segment, even one
  out of order, and checks none.
- **128 sockets.** Each interface had slots for 128 sockets, fixed, and
  `lo` takes two for each connection to the machine itself: a server
  with a few hundred clients, or a program with that many connections,
  was refused sockets (ENOBUFS).
- **TIME-WAIT held a socket's memory.** A connection closed first by
  this end spends TIME-WAIT, 10 seconds in smoltcp, in its socket, with
  its 128 KiB of buffers, though it sends and receives no data. Linux
  keeps a small record of its own for it. On `lo` it ends at once
  (ADR 0185); on a card, a server that closes its connections held
  128 KiB for each one closed in the last 10 seconds. A client that
  connected again from the same port in that time had its SYN answered
  by the old connection, sent a reset, and waited a second to try
  again; Linux takes such a SYN as a new connection (RFC 6191).

## Decision

In the kernel's patch to smoltcp, and its interfaces:

- **Loss recovery uses SACK, as Linux's does** (RFC 6675). The sender
  keeps the four lowest ranges above the last ACK that the other end
  said it has. A fast retransmit starts a recovery, which lasts until
  every byte sent before it is acknowledged; no other starts meanwhile.
  In it, each ACK that comes sends one retransmission: the next range
  not SACKed with three segments' worth SACKed above it, or, after a
  partial ACK, the first byte not acknowledged, if not already
  retransmitted (RFC 6582, which serves an end that sends no SACK
  blocks too). An RTO forgets what was SACKed (RFC 2018 section 8).
- **The receiver sends up to three SACK blocks**: the one of the
  segment that came, then those the last ACK gave, then the lowest
  (RFC 2018 section 4).
- **PAWS** (RFC 7323 section 5): a segment, other than a reset, whose
  timestamp is older than the one recorded in the last 24 days is
  dropped, and answered with an ACK at most once a second. The
  timestamp is recorded only from a segment at the left edge of the
  window, so that one that comes early does not turn those before it
  away, and every ACK echoes it.
- **An interface starts with 128 socket slots and makes twice as many
  when they are all taken**, up to 16,384: 11 MiB of 688-byte slots.
  The sockets move to the new slots and keep their handles, and the old
  slots' memory comes back through their lease (ADR 0182). With 16,384
  taken, the oldest orphan in TIME-WAIT ends to make room, as Linux
  ends TIME-WAIT past `tcp_max_tw_buckets`.
- **On a card, an orphan connection in TIME-WAIT gives its buffers back
  at once** and keeps only its slot, with empty buffers and no window,
  until TIME-WAIT ends. Segments that come for it are answered as
  before.
- **A SYN that opens a connection goes to a connection of its
  endpoints first**, then to a listening socket, as Linux looks up an
  established connection before a listener. One in TIME-WAIT gives way
  to it and closes, with no reset, when the SYN's sequence number is
  past the last one received or its timestamp is newer, as on Linux.

## Consequences

- A window that loses several segments recovers in about one round
  trip, as on Linux, not one RTO per segment lost.
- smoltcp's tests cover each part: RFC 2018's third case, with three
  blocks, an out-of-order segment and the hole filled; a recovery that
  retransmits each hole once and none twice; NewReno's partial ACK and
  no second recovery; the scoreboard joining, trimming, keeping the
  lowest and refusing blocks of what was not sent; an RTO forgetting
  it; PAWS dropping old segments, letting resets through, recording
  from the left edge and forgetting after 24 days; growing a socket
  set; TIME-WAIT shedding its buffers and still answering; and a SYN
  that a connection in TIME-WAIT gives way to, and one it keeps. I
  checked that each new condition has a test that fails without it.
- radix-net's tests grow a stack's slots under a live connection, shed
  a TIME-WAIT connection's memory and see it end, and connect again
  from the same port to a server whose old connection is in TIME-WAIT,
  in the slot before the listening socket: established within ten
  milliseconds. Without the new lookup order, it waits for the client
  to try again.
- smoltcp still looks through an interface's sockets, one by one, for
  each segment that comes and each time it polls. With thousands of
  sockets on one interface that costs microseconds a segment; a table
  of sockets by their endpoints is left for when a benchmark asks for
  it.
- TIME-WAIT stays smoltcp's 10 seconds, shorter than Linux's 60:
  timestamps and PAWS, and random initial sequence numbers, keep old
  segments out of new connections.
- A connection made, accepted and closed over `lo` takes 51,473
  instructions under QEMU's count, 1,445 more than after ADR 0187: the
  SYN looked for among the connections before the listeners, the PAWS
  check and the SACK scoreboard's look at each ACK. Its budget stays
  56,000. The other network rows move by at most 100 instructions an
  operation, and TCP's throughput over `lo` by 800 a MiB (of 1.5
  million).
