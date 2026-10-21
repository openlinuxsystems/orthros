<!--
SPDX-FileCopyrightText: 2026 The OrthrOS contributors
SPDX-License-Identifier: Apache-2.0 OR MIT
-->

# 0041. A yield gives up the rest of its slice, and a TLB shootdown's interrupt ends before the flush

- Status: Accepted
- Date: 2026-10-21

## Context

After ADR 0040, two rows of the performance gate (ADR 0039) were still
behind Linux in the same kind of virtual machine on some CI runners.

- **Context switch.** Two programs on one CPU yield to each other. On
  some runs Radix switched almost never, and the row came out hundreds
  of times slower than Linux. A yield moved only the yielder's EEVDF
  deadline one slice later. When the other program had run ahead of its
  share (negative lag, which it keeps across a sleep), the yielder was
  the only eligible entity and kept the CPU until the average virtual
  runtime caught up, up to the lag limit of 10 ms: at under 100 ns a
  yield, about as many yields as the benchmark makes. Linux had the same
  problem and, since Linux 6.18, an eligible entity that yields forfeits
  the rest of its slice: its virtual runtime moves to its deadline
  ("sched/fair: Forfeit vruntime on yield").
- **TLB shootdown.** Unmapping a page while its address space runs on
  the other CPU took 1.14 to 1.17 times as long as Linux's `mprotect`.
  Timestamps taken in the kernel around each step (temporary
  instrumentation, since removed) showed that most of the time is the
  host's, sending the interrupt and delivering it, which Linux pays too;
  but about 38 % of the receiving CPU's EOIs also left the virtual
  machine although paravirtual EOI was on (ADR 0040). KVM offers
  paravirtual EOI only when no other interrupt is pending as it enters
  the guest (`apic_sync_pv_eoi_to_guest` in Linux's `lapic.c`). The
  handler answered the shootdown before its EOI, so the initiator's next
  request was often already pending. Linux's handlers of interprocessor
  interrupts end the interrupt first.

The report also had no row to tell the system calls of a shootdown from
the interrupt.

## Decision

- `FairQueue::yield_running` follows Linux 6.18 (no code is taken): an
  eligible yielder's virtual runtime moves to its deadline, and its new
  deadline is one slice after that. The yielder loses ties at the next
  pick. An entity that is not eligible changes nothing but the tie; an
  entity alone on its queue keeps its slice. The property tests of the
  fair queue now include yields among their random steps, with unit
  tests for taking turns and for an entity owed time.
- The TLB shootdown handler writes the EOI before it serves the request.
  A handler whose work lets another interrupt of its kind be sent ends
  the interrupt first.
- `ob bench` gains `tlb_local`: the same map and unmap in an address
  space no other CPU runs, against `mprotect(2)` with no other thread.
  The difference from `tlb_shootdown` is the shootdown itself.

## Consequences

- In CI run 39646815477 (AMD EPYC 7763, KVM), every row passed the gate.
  A context switch took 400 ns against Linux's 713 ns (0.56), with the
  expected 2 switches a round; a shootdown 14.0 µs against 14.4 µs
  (0.97), with 99.3 % of its EOIs paravirtual; `tlb_local` 578 ns
  against 859 ns (0.67).
- A program that yields while another can run lets the other run first,
  even if it has run less than its share: yielding gives the share up.
- What remains of a shootdown's cost under KVM is the host's. Linux
  guests also skip preempted virtual CPUs with KVM's paravirtual TLB
  flush; Radix could add it if a shootdown row falls behind again.
