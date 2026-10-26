<!--
SPDX-FileCopyrightText: 2026 The Dukovo contributors
SPDX-License-Identifier: Apache-2.0 OR MIT
-->

# 0116. Idle polling learns only from wake-ups that other CPUs send

- Status: Accepted
- Date: 2026-10-26

## Context

An idle virtual CPU without MWAIT polls before it halts (ADR 0037), so
that a waker on another CPU writes a flag instead of sending an
interprocessor interrupt. The limit learnt from every idle period alike,
as Linux's cpuidle-haltpoll governor does. A thread waiting for a disk
is woken by the disk's interrupt on its own CPU, so its CPU's idle
periods all ended soon after they began, and the CPU polled through
every disk wait.

Polling cannot spare that interrupt: the hypervisor delivers it whether
the CPU polls or halts. In a virtual machine it reaches a halted CPU
sooner. The host watches a halted virtual CPU for interrupts and enters
it at once, but a running one it must first stop with an interrupt of
its own, which in a nested virtual machine (the CI runners) goes
through the outer hypervisor too. Linux does not poll in a guest unless
told the host's CPUs are its own, so its CPUs halt while they wait for
a disk. The first NVMe rows of `ob bench` (ADR 0115), whose doorbell
writes and interrupts QEMU's traces show to be the same as Linux's,
were 1.05 to 1.24 times Linux.

## Decision

- A waker on another CPU marks the CPU it wakes (`called`) before it
  sets `need_resched`. The idle loop clears the mark when an idle period
  begins and reads it when the period ends.
- The limit doubles only for an idle period that another CPU's wake-up
  ended, after the limit and within 200 µs. A period that this CPU's own
  interrupt ended (a device's, or the timer's) halves it, as one longer
  than 200 µs does. So a CPU that waits only for its devices learns to
  halt at once, and one that other CPUs wake keeps polling.
- A transfer of several requests (ADR 0111) waits for its last request
  first, and each request's thread waits on its own slot, so the
  completions of the earlier requests wake no thread.

## Alternatives

- **Poll only when no thread of the CPU waits for a device.** Needs the
  scheduler to know why each thread sleeps, and still polls for a CPU
  that both kinds of wake-up reach; learning from how each period ended
  covers both.
- **Stop polling under nested virtualization.** The cross-CPU rows of
  `ob bench` gain from polling there too; only device interrupts do not.

## Consequences

- An idle period ended by a device interrupt costs a halt, which the
  hypervisor sees while the CPU waits, not after the interrupt.
- A CPU woken alternately by other CPUs and by its own interrupts keeps a
  limit between the two; the bench rows of either kind stay as they were
  (`ob bench` reports each row's halts and polls).
- This changes ADR 0037's policy, which was Linux's governor unchanged.
