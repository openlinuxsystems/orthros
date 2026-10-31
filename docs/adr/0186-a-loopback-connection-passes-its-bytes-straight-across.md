<!--
SPDX-FileCopyrightText: 2026 The Dukovo contributors
SPDX-License-Identifier: Apache-2.0 OR MIT
-->

# 0186. A loopback connection passes its bytes straight across

- Status: Accepted
- Date: 2026-10-31

## Context

The gate's TCP throughput row (ADR 0185), 256 MiB over a connection on
`lo` in 64 KiB writes, cost 1.35 times Linux's on CI's Zen 5 runner,
91.1 µs per MiB against 67.9, though `lo`'s packets were written and
read in place by then. The other network rows cost half Linux's or
less.

Counting the copies of each byte says why. Through packets, a byte is
copied four times: from the sending program into its socket's transmit
buffer, from there into a packet, from the packet into the other
socket's receive buffer, and from there to the receiving program. On
Linux it is copied twice: into an skb, which `lo` hands to the
receiving socket as it is, and out of it. smoltcp's sockets each have
buffers of their own and no way to share one, so packets cannot do
better than four. Making the copy into a packet go backward where Zen's
4K aliasing would stall it (the kernel's own `memcpy`) changed nothing
CI could measure, and was taken back.

Solaris 10 met the same cost and answered it with TCP fusion: once both
ends of a TCP connection are on the same machine, the sender puts its
data straight on the receiver's queue, and the stack carries only what
is not data. Windows has a loopback fast path that does much the same
for programs that ask for it.

## Decision

- **A TCP connection whose other end is a socket of the same stack, on
  `lo`, passes its bytes straight across.** The sending system call
  copies them from the program into the other end's receive buffer, at
  once, under the stack's lock; the receiving program copies them out,
  as before. A byte is copied twice, as on Linux, and no packet is made,
  sent, received or taken apart for it. The kernel finds the other end
  among `lo`'s sockets by its addresses and ports, the first time, and
  remembers where.
- **Both ends count what goes across as a segment sent, received and
  acknowledged at once.** The kernel's patch to smoltcp (ADR 0182) adds
  the calls: the sender's sequence numbers move past the bytes as if
  their ACK had come, the receiver's as if it had sent one, and both
  take the receiver's window and count the other end as heard from, for
  the keep-alive and timeout timers. Everything else about the
  connection still goes in packets, in step with what went across: its
  handshake, its FIN or reset, and any bytes that could not go across.
- **Bytes go across only when nothing is on its way between the two
  ends.** The sender has no byte queued or unacknowledged, the receiver
  expects the next byte, has acknowledged every one it received and has
  none out of order, and `lo`'s ring holds no packet. Otherwise the
  bytes go in packets, as they did. A connection whose bytes went in
  packets goes across again once their ACK has come, after the
  receiver's delayed-ACK timer at most (10 ms).
- **A read tells the other end straight of the window it opened**, in no
  packet, when the two are in step in the same way, and wakes a sender
  waiting for it when the window had shut to half the buffer or less,
  as a poll's packet would have.
- **A sender whose other end has no room waits for it to read**, rather
  than filling its own transmit buffer, as Solaris's fused sender is
  held back by the receiver's queue: a byte that went into the sender's buffer
  would be copied three times. `poll` says such a connection is
  writable when the other end has room. While its bytes go across, a
  connection on `lo` so holds 64 KiB each way, the receiver's buffer,
  where in packets it held 128 KiB, that and the sender's.

## Consequences

- The TCP rows on `lo` cost what a pipe's do. Under QEMU's instruction
  count, 1 MiB over a connection takes 1,515,375 instructions, where a
  pipe takes 1,498,888 and packets took 1,990,584; a byte there and back
  takes 9,274, where packets took 17,527. Their budgets in `ob bench`
  follow.
- No packet carries the bytes that go across, so nothing that watches
  `lo`'s packets sees them. The packet filter of 12c will see a
  connection's handshake and end, not its data; a rule that looks at
  `lo` must keep the connections it matches in packets, as Solaris kept
  connections that IPsec or its filter looked at out of fusion. Packet
  capture, when it comes, must do the same.
- A program that writes more than 64 KiB to a connection on `lo` before
  the other end reads, at both ends at once, waits where it went on
  before, at 128 KiB. Linux's buffers are larger again; a program that
  needs more room than the buffers give waits on any system.
- radix-net's tests pass bytes across and in packets on one connection
  in a random order, 20,000 steps of both directions, and check that
  every byte comes once and in order and that the connection stays
  open; others check that bytes wait for room, that the window goes
  across, that queued bytes go first, and that a connection that passed
  bytes across ends as any does, with no reset.
