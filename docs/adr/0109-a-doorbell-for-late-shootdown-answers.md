<!--
SPDX-FileCopyrightText: 2026 The Dukovo contributors
SPDX-License-Identifier: Apache-2.0 OR MIT
-->

# 0109. Under KVM, a CPU that answers a TLB shootdown late gets a faster timer while it runs

- Status: Accepted
- Date: 2026-10-25

## Context

ADR 0105 took the slow `tlb_shootdown` rows on some CI runners for
virtual CPUs the host had stopped, and let the hypervisor flush those.
The counters added since (`BENCHCOUNT` `shootdown_wait_ns` and
`shootdowns_left`) show it was not the main cause. On PR #43's run
(40208415343, AMD EPYC 9V45 runner) the row took 650 µs per round
against Linux's 330 µs, 1.97 times Linux. Each shootdown waited
748 µs for the other CPU's answer. Only 0.01 targets per round were
stopped and left to the hypervisor; 0.99 got an interrupt. Linux's
other CPU took 1.00 TLB interrupts per round. On other runners the same
row takes about 11 µs, and every other row of that run, cross-CPU
wake-ups included, stayed at 0.86 of Linux or below.

So the interrupt reached a virtual CPU that was running, a user loop
in this benchmark, hundreds of microseconds late. Wake-ups to an idle
CPU were fast, so the delay is particular to a running target. What
fits is a host that holds an interrupt for a running virtual CPU until
that CPU leaves the guest for some other reason. A timer interrupt is
one such exit. A CPU running user code takes few others, and Radix's
tick is 100 Hz, so its CPUs leave the guest less often than Linux's
on a busy CPU. The host's code is not visible, so this is the
explanation that fits the numbers, not a known fact. Sending the
interrupt again would not help: KVM forces a running virtual CPU out
of the guest once per kick, and the kick already sent is the one being
held.

## Decision

- Each shootdown records when it was sent. A target that finds its
  request more than 50 µs old when it serves it (`paging::LATE_NS`)
  rings its doorbell (`time::ring_doorbell`). Where interrupts arrive at
  once, an answer takes a few microseconds.
- While the doorbell rings, the CPU's timer also fires every 250 µs
  (`time::DOORBELL_NS`). Each firing leaves the guest, so a held
  interrupt waits at most that long. A firing with nothing due re-arms
  the timer and returns; it does not count as a tick.
- The doorbell rings for 10 ms after the last late answer
  (`time::DOORBELL_HOLD_NS`). Answers that keep coming late keep it
  ringing, and a CPU that goes idle stops it at once, since a halted
  virtual CPU has left the guest and takes its interrupts promptly.
- It works only under KVM, where it was measured. On bare metal a
  late answer means the target had interrupts masked, which a faster
  timer cannot shorten. Under QEMU's emulator it would only add work.

Two alternatives were set aside:

- A 1000 Hz tick on every busy CPU, as a Linux kernel configured for
  1000 Hz keeps. It costs exits on every busy CPU and every host,
  including the many that deliver at once, and would at best match
  Linux here.
- Skipping the shootdown when the old entry's accessed bit is clear,
  since a CPU caches no translation without setting it. Intel documents
  that rule, but AMD's manual does not state it as plainly, and a wrong
  guess there would leave a stale writable mapping. It is not taken.

## Consequences

- On hosts that hold interrupts for running virtual CPUs, the first
  shootdown to a busy CPU still waits the long time and the next ones
  wait about half of 250 µs on average.
- A CPU whose doorbell rings takes up to 4,000 more timer interrupts a
  second, and writes its timer as often, for at most 10 ms after its
  last late answer. `ob bench` shows it as more timer writes per
  operation in the `tlb_shootdown` row on such hosts.
- Hosts that deliver at once never ring it, and nothing changes there.
- Other interrupts to a running CPU, such as a reschedule IPI, are held
  the same way on such hosts. They are not measured yet, so they do not
  ring the doorbell.
- It is x86-64 code. The aarch64 port has no paging yet; Arm's
  broadcast TLB invalidation sends no interrupt, so it should not need
  one.
