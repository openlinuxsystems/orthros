<!--
SPDX-FileCopyrightText: 2026 The Dukovo contributors
SPDX-License-Identifier: Apache-2.0 OR MIT
-->

# 0040. The release kernel is built with link-time optimisation, and its hot paths avoid locked instructions, string moves and 128-bit division

- Status: Accepted
- Date: 2026-10-21

## Context

The first run of the performance gate (ADR 0039) on the CI runners (AMD
EPYC 7763, KVM) put 16 of its 19 rows at or better than Linux in the same
kind of virtual machine. Three were behind: a capability method (1.43
times Linux's `fcntl(F_GETFD)`), a TLB shootdown (1.14) and a channel
round trip on one CPU (1.06). Tracing one operation instruction by
instruction (QEMU with its debugger stub, a breakpoint on the path and
single steps, with `nokaslr` so the symbols match) showed where the time
went:

- A capability method took 584 instructions in the kernel against 159
  for the null system call. Small functions of one crate (the ABI's
  decoders and encoders, a lock's acquire) were calls from another, with
  their results copied through memory. Copying the arguments, a length
  known only at run time, called `memcpy`, whose string moves start up
  slowly on many CPUs even for no bytes at all; and without SSE, which the
  kernel does not use, any copy of more than 64 bytes (the ten result
  registers) compiles to a string move. The per-CPU capability cache took
  a lock, a locked instruction, though only its own CPU ever uses it.
- An endpoint round trip on one CPU took 7,500 instructions. About 1,000
  were calls into the lock checker's hooks, made by every lock even with
  the checker off; the EEVDF arithmetic divided 128-bit numbers, which
  x86-64 does in a library call, a dozen times; the domain shares were
  rebuilt as a tree on every wake-up and sleep; and each switch cloned
  and dropped reference counts it did not need.
- The CPU that receives a shootdown's interrupt leaves the virtual
  machine to write the APIC's EOI register. Linux guests avoid that exit
  with KVM's paravirtual EOI.

## Decision

- The release kernel is built with link-time optimisation over all its
  crates and one code generation unit (`kernel/Cargo.toml`).
- A per-CPU value that only its own CPU uses is a `percpu::Own<T>`,
  reached through `PerCpu::with_own` with preemption held off: no lock
  and no locked instruction, and a plain flag turns away an interrupt
  handler that comes in during a use, which then takes the slow path. The
  capability cache and the per-CPU random generators are `Own` values.
  This is one more allowed `unsafe` site in the `radix` crate, with the
  safety argument beside it.
- Hot paths copy fixed sizes of at most 64 bytes: the method arguments
  are copied word by word up to their count, and the ten result words go
  back as eight and two.
- The kernel tells `radix_sync` whether the lock checker is on
  (`set_checking`); when it is off, locks no longer call its hooks. The
  scheduler's `after_switch` does nothing either.
- The scheduler's multiply-divides and `avg_vruntime` divide in 64 bits
  when the numbers fit, which gives the same answer (host tests compare
  the two, with a property test); `Rq::reweigh` skips building the tree
  when every queued thread is in one domain, where the tree gives each
  thread its part of the nice weights anyway; `reschedule` borrows the
  running thread instead of cloning a reference to it.
- Under KVM with `KVM_FEATURE_PV_EOI`, every CPU turns on paravirtual EOI
  (`MSR_KVM_PV_EOI_EN`, after Linux's
  `Documentation/virt/kvm/x86/msr.rst`; no code is taken): the
  hypervisor sets bit 0 of a per-CPU word when an interrupt needs no EOI
  write, and the handler clears it in one instruction instead of writing
  the register.

## Consequences

- A capability method takes about 400 instructions in the kernel and a
  same-CPU endpoint round trip about 5,500, a third and a quarter fewer.
  The `ob bench` report under KVM is what counts; the instruction counts
  only say where to look.
- Building the release kernel takes longer, since one LLVM unit now
  optimises it whole. Debug builds are unchanged.
- Code on a hot path is checked for the costs above when it changes:
  copies of more than 64 bytes or of a run-time length, locked
  instructions for data no other CPU touches, 128-bit division, and
  reference counts taken and dropped in one function.
- Paravirtual EOI applies only under KVM. Bare metal and other
  hypervisors write the register as before, after one extra load.
