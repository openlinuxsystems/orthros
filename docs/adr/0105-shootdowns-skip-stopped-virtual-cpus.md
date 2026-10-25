<!--
SPDX-FileCopyrightText: 2026 The Dukovo contributors
SPDX-License-Identifier: Apache-2.0 OR MIT
-->

# 0105. Under KVM, a TLB shootdown sends no interrupt to a virtual CPU the host has stopped

- Status: Accepted
- Date: 2026-10-25

## Context

A TLB shootdown sends each CPU that may hold the old mapping an
interrupt and waits until every one has flushed it. In a virtual
machine, a virtual CPU runs only while the host gives it a real one. On
a busy host the target can be stopped for milliseconds, and the
shootdown waits that long. The `tlb_shootdown` row of `ob bench` showed
it: on most CI runs Radix took 10.8 µs per page and Linux in the same
virtual machine 11.2 µs, but on busy runners Radix took 669 µs and
Linux 329 µs (main at cd68e09, run 40144550245), and the gate failed at
2.04 times Linux. It had happened before, on PR #19 (754 µs against
331 µs) and PR #29 (679 µs against 331 µs).

Linux guests under KVM avoid part of the wait with two paravirtual
features (`Documentation/virt/kvm/x86/cpuid.rst` and `msr.rst`):

- Steal time (`KVM_FEATURE_STEAL_TIME`, `MSR_KVM_STEAL_TIME`): each
  virtual CPU gives the hypervisor the physical address of a 64-byte
  record. The hypervisor counts there the time the CPU waited for a real
  one, and sets bit 0 of its `preempted` byte (`KVM_VCPU_PREEMPTED`)
  while the host has the CPU stopped.
- Paravirtual TLB flush (`KVM_FEATURE_PV_TLB_FLUSH`): a guest that finds a
  target stopped sets bit 1 of its `preempted` byte
  (`KVM_VCPU_FLUSH_TLB`) instead of interrupting it. Before the
  hypervisor runs that CPU again, it exchanges the byte for 0 and, if
  bit 1 was set, flushes the CPU's whole TLB.

## Decision

- Every CPU gives KVM its steal-time record when KVM offers steal time
  (`kvm::init_cpu`). The record is a field of the CPU's per-CPU block,
  64-byte aligned, all atomics, since the hypervisor writes it.
- When KVM also offers paravirtual TLB flush and does not hint that its
  virtual CPUs have real ones to themselves (`KVM_HINTS_REALTIME`), a
  shootdown asks it to flush each stopped target
  (`kvm::flush_when_resumed`): a compare-exchange that sets the flush bit
  only while the stopped bit is set. A target it succeeds for gets no
  interrupt. A CPU whose record the hypervisor does not have keeps the
  byte at 0 and always gets one.
- While it waits, the initiator looks again at each target that has not
  answered: one the host stopped after its interrupt was sent is asked
  for the flush too, and the initiator clears its pending bit for it.
  Linux looks only before sending.
- The initiator now sends the interrupts before it flushes its own TLB,
  so its flush overlaps their way to the other CPUs, as Linux's does.
- The boot report says `KVM: steal time and paravirtual TLB flush` when
  it is on. Only the interface is taken from Linux's documentation; no
  code is.

The paravirtual flush is safe where the interrupt was because both end
the same way: the target runs no instruction between the page-table
change and its flush. The compare-exchange succeeds only while the
target is stopped, it orders the page-table writes before it (a locked
instruction on x86), and the hypervisor takes the byte with an exchange
before the target runs again. The hypervisor's flush is the whole TLB,
kernel and every PCID, which covers any range. Radix frees a space's
page tables only once no CPU runs the space, and walks them in software
only under the space's lock, so no code relies on the interrupt to know
that a stopped CPU has finished with a page table.

## Consequences

- On a busy host a shootdown no longer waits for a stopped virtual CPU
  that holds the old mapping, the case where Radix fell behind Linux.
- `ob bench`'s count of interrupts per shootdown can drop below 1 on a
  busy runner, since stopped targets need none.
- Steal time itself, the time a CPU waited, is not used yet; the
  scheduler could charge it to no thread later, as Linux's does.
- None of it applies on bare metal or under other hypervisors, and the
  boot tests under QEMU's emulator never take these paths. They run
  under KVM in CI's boot tests and its Benchmarks job.
