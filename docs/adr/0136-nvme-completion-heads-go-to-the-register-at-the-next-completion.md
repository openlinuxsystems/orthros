<!--
SPDX-FileCopyrightText: 2026 The Dukovo contributors
SPDX-License-Identifier: Apache-2.0 OR MIT
-->

# 0136. An NVMe completion head the controller asks for goes to its register at the next completion

- Status: Accepted
- Date: 2026-10-28

## Context

On main at f174a86 the Benchmarks job ran on an AMD EPYC 9V45 runner
and stayed over the gate after 9 runs on three raw NVMe rows: a 4 KiB
read took 54.8 µs against Linux's 50.3 (1.09), a 4 KiB write 1.03 times
Linux and a 64 KiB read 1.01. The virtio rows of the same sizes were
0.94 to 0.96, so the cost was in the NVMe path.

A lane with shadow doorbells (ADR 0117) writes a completion head to its
register when the move passes the controller's event index. Each time
QEMU's controller writes a completion, it first sets that event index
to the head it last heard of, then reads the shadow doorbell. A head the
lane writes to the register at once is then the head the controller
asks for next. With one command at a time in flight, every completion
passes the event index again, and every one is a register write: an
exit to QEMU. ADR 0117 expected the heads to stay in memory once one
went unannounced, as Linux's did in its trace, but nothing made the
first one go unannounced. QEMU's trace of `ob bench` showed 51,351
completion head register writes for 52,004 I/O completions.

## Decision

- A completion head the controller asks to hear of goes to its register
  when the lane next finds a completion, before reading it, rather than
  at once. By then the controller has written again, and set its event
  index from the head it had heard of before. Being one head behind, it
  asks for none from then on while commands come one at a time.
- The exception is when the controller could see its completion queue
  full before then. A submission writes the head first if it leaves as
  many commands in flight, counted with the completions read but not
  yet announced, as the lane's depth (63). A controller that waits on
  the head register for room never waits on a head the lane holds back.
- A reset forgets a held-back head, with the queues.
- Without shadow doorbells, every head goes to its register at once, as
  before.

## Alternatives

- **Write the head at once, as the event index asks (before).** Every
  completion of a one-at-a-time stream exits under QEMU.
- **Start each lane with two commands in flight, so that QEMU's event
  index falls behind.** That relies on QEMU's timing. A lane that has to
  ring again later is back where it started, while a held-back head
  gets out of that by itself at the next completion.
- **Never write the head register with shadow doorbells.** ADR 0117
  rejected it: the head doorbell is the controller's flow control.

## Consequences

- QEMU's trace of `ob bench` with the change showed 453 completion
  head register writes for the same 52,004 I/O completions.
- A head the controller asks for reaches its register one completion
  later. QEMU reads the shadow doorbell before it writes each
  completion, so it never waits for it. The depth check keeps any
  controller from seeing a full queue because of it.
- Real drives do not take shadow doorbells, so nothing changes on
  hardware.
- The submission doorbell still rings when the controller is idle, as
  Linux's does.
