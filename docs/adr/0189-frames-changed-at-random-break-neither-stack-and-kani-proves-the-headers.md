<!--
SPDX-FileCopyrightText: 2026 The Dukovo contributors
SPDX-License-Identifier: Apache-2.0 OR MIT
-->

# 0189. Frames changed at random break neither stack, and Kani proves the headers

- Status: Accepted
- Date: 2026-10-31

## Context

The kernel's network stack (smoltcp's, patched: ADRs 0180, 0182) reads
every frame that comes to a card, from any machine on the network. A
frame that made it panic would stop the machine, as the kernel aborts on
a panic, and anyone who could reach it could send that frame. smoltcp's
own tests, and radix-net's, cover traffic that follows the protocols:
none sends what a broken or hostile network does.

The kernel's other readers of what comes from outside are proved or
fuzzed: Kani proves the file systems' and NVMe's parsers (NVMe's in ADR
0115) and a fuzzer makes random system calls (ADR 0043). The roadmap's
12a.3 asked the same of the network: fuzzing, Kani on its parsers, and
virtio-net's offloads.

## Decision

- **A fuzzer in radix-net's tests**, which `ob test` runs with the other
  unit tests. Two stacks on a simulated wire open a connection and send
  bytes each way, close another from each end in turn, have a third
  refused, keep a socket listening, send a datagram each way and ping
  each other. The frames each sent are what the fuzzer starts from: in
  each of 100,000 rounds, one frame each sent goes to the other, sent
  again as it was or changed one to four times (a bit flipped; a byte or
  a 16-bit field set to an edge such as 0, 0x7fff or the frame's length;
  a sequence number moved by up to 2,048, so that it may still land in a
  window; the frame cut short or made longer; a piece of another frame
  copied over it), with the real traffic, and time moves by a
  millisecond, now and then by 200 ms or 3 s, for the timers. Neither
  stack may panic; every frame either sends must be one any stack would
  take: ARP or IPv4, no longer than the stack's frame, its headers
  whole, with checksums that check; and once the network is sound again
  and the stacks have forgotten what it told them, a new connection
  between them carries bytes each way. It runs twice, on a link with
  checksums, as a card's, and on one without, as `lo`'s, so that every
  change gets past them to the sockets. Its seeds are fixed, so a
  failure comes back on every run.
- **Kani proofs in radix-net**, which `ob test kani` checks: for any
  bytes, of any length up to a bound, each header the stack reads from
  a card (Ethernet, ARP, IPv4 with its options, ICMPv4 echoes and
  errors, UDP) and each TCP option is read without a panic, and what is
  read is written back, in no more room than it came in, as one that
  reads the same. A seventh proves the loopback ring (ADR 0185):
  whatever packets are sent, read and answered in place, of whatever
  lengths, it hands back each it took, whole and in order, takes none
  longer than its MTU, and an empty ring takes any other. The proofs
  leave out checksums, and whole TCP segments: with checksums, Kani ran
  out of memory on a segment of 44 bytes, and without them did not
  finish one of 32 in 15 minutes. smoltcp reads a segment's options one
  after another, as the option proof has them, and the fuzzer checks
  the checksums, and the segments, of every frame the stacks send.
- **What the fuzzer found is fixed in the patch**: an ICMPv4 error, such
  as the port unreachable a datagram to a closed port draws, left its
  four unused bytes as its buffer held them, from a frame sent before:
  four bytes of earlier traffic went to whoever sent the datagram.
  They are zero, as RFC 792 has them.
- **virtio-net's offloads move to 12d**, with the server cards' (receive
  checksums, segmentation). QEMU offers them only with a backend that
  passes virtio-net's headers on, tap or vhost; with the user network
  the boot tests use, and CI can run, it offers none, so they could not
  be tested here.

## Consequences

- A local run of twelve million rounds, six seeds more with and without
  checksums, found nothing more.
- The fuzzer takes about a second of `ob test`; the proofs add about six
  and a half minutes to the Kani job here, most of it the loopback
  ring's (three minutes) and ICMPv4's (two).
- The fuzzer reaches smoltcp through radix-net's stack; the kernel's
  sockets above it (`fs/inet.rs`) see only what the stack makes of the
  frames, and the system call fuzzer (ADR 0043) does not open network
  sockets yet.
- Kani's bounds: frames of 16 to 64 bytes, enough for each header
  whole (an IPv4 header with any options, an ICMPv4 error quoting one
  with up to 12 bytes of them), a TCP option of up to the 40 bytes a
  header holds, and a loopback ring of 44 bytes with packets of up to
  8 over five steps.
