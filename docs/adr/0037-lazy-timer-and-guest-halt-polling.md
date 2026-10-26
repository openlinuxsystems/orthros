<!--
SPDX-FileCopyrightText: 2026 The Dukovo contributors
SPDX-License-Identifier: Apache-2.0 OR MIT
-->

# 0037. The timer is written only to fire earlier, and idle virtual CPUs poll before they halt

- Status: Accepted; polling learns only from wake-ups that other CPUs send since [0116](0116-idle-polling-learns-from-other-cpus.md)
- Date: 2026-10-21

## Context

The first KVM numbers of `ob bench` (ADR 0035) showed system call entry
and exit close to Linux, but a `yield` with nothing else to run at 4.1 µs
against Linux's 0.2 µs, and an endpoint round trip at 10.4 µs on one CPU
and 31 µs on two, against 2.7 µs and 19.5 µs for a pipe. Under a
hypervisor some instructions leave the guest (a VM exit, around a
microsecond each on the CI runners): writing the APIC timer, sending an
interprocessor interrupt, halting, and CPUID. The scheduler wrote the
timer at every context switch, usually with the deadline it already had;
an idle CPU halted at once, so waking it cost an exit to halt, an exit to
send the interrupt and the hypervisor's own work; and mapping a user page
ran CPUID to ask whether the CPU has NX. The performance gate of the plan
asks for Linux parity, measured the same way.

## Decision

- The timer is armed lazily: a switch writes the timer only when the
  next deadline is earlier than the one already armed. A timer armed too
  early fires, finds nothing due and is armed again for the real
  deadline. Each CPU counts its timer writes.
- In a virtual machine without MWAIT, an idle CPU polls `need_resched`
  before it halts, and says so in `polling`, so a waker writes the flag
  instead of sending an interrupt. How long it polls adapts as Linux's
  cpuidle-haltpoll governor does, with its defaults: the limit starts at
  0, doubles (from 50 µs, up to 200 µs) when an idle period outlasted the
  limit but ended within 200 µs, and halves when one lasted longer. The
  policy is `radix_sched::haltpoll`, written for Radix after Linux's
  documentation (`Documentation/virt/guest-halt-polling.rst`); no code is
  taken. On bare metal, and with MWAIT, nothing changes.
- The waker sets `need_resched` before it reads `polling`, and the
  poller clears `polling` before it reads `need_resched`, both
  sequentially consistent, so one of the two always sees the other. The
  TLA+ model `kernel/sched/tla/IdleWake.tla` checks that no wake-up is
  lost with polling, MWAIT and HLT, and that removing either ordering
  loses one.
- The answers of CPUID that the paging code needs are read once and
  kept.
- The benchmark self-test logs the switches, timer writes,
  interprocessor interrupts, halts and polls of each benchmark, and
  `ob bench` reports them per operation, so the cause of a slow number
  is visible in the report.

## Consequences

- A same-CPU round trip needs no timer write, and a wake-up across CPUs
  that arrives within the polling limit needs no interrupt and no halt:
  no exits at all.
- A virtual CPU may spin for up to 200 µs before halting, which the
  hypervisor sees as busy time. The limit only grows when polling would
  have caught the wake-up, and shrinks when it does not, which is the
  trade Linux guests make by default.
- A timer that fires early costs one extra interrupt, rarely: deadlines
  usually move later, not earlier.
- Bare-metal power use is unchanged. Phase 11's power work decides
  whether polling ever applies outside a virtual machine.
