<!--
SPDX-FileCopyrightText: 2026 The Dukovo contributors
SPDX-License-Identifier: Apache-2.0 OR MIT
-->

# 0039. Radix is held to Linux in the same kind of virtual machine, and CI fails when it is slower

- Status: Accepted
- Date: 2026-10-21

## Context

ADR 0035 compared Radix, in a virtual machine, with the Linux of the
machine around it, and kept CI from failing on a slow number because the
runners vary too much. Chase then set the performance gate of the plan:
context switches, system calls, IPC, page faults, TLB shootdowns, memory
bandwidth and scaling over several CPUs must match Linux or beat it,
measured in the same CI run, and CI must fail when they stop doing so.
Two things stood in the way. The host's Linux runs without a hypervisor,
so every operation that exits to it (an interrupt sent, a halt, a timer
written) costs Radix more than it costs the baseline, and the comparison
says as much about KVM as about Radix. And several of the operations had
no measurement on one side or the other.

## Decision

- `ob bench` boots Linux in a virtual machine set up exactly like
  Radix's: the same QEMU machine (q35), CPUs (2), memory (1 GiB) and
  KVM, with `-cpu host`. The kernel is the runner's own
  (`--linux-kernel`, `OB_LINUX_KERNEL`, or `/boot/vmlinuz-$(uname -r)`
  when it is readable), booted directly with an initramfs that `ob`
  writes (a cpio archive in the "newc" format) holding only a static
  `linux-bench` as `/init` and `/dev/console`. `linux-bench` notices it
  is process 1, prints its lines and powers the machine off.
- That Linux boots with `mitigations=off`. Radix has none of the
  mitigations of CPU vulnerabilities yet (plan item 3.3), so this is the
  like-for-like comparison, and the harder one. When Radix gains them,
  the baseline gains Linux's defaults.
- Every row of the report pairs a Radix measurement with a Linux one:
  the null system call with `getppid`, a capability method with
  `fcntl(F_GETFD)`, 64 random bytes with `getrandom`, a context switch
  between two programs on one CPU with two processes calling
  `sched_yield`, a fresh page (made, mapped, written and given back)
  with `mmap`, a first-touch fault and `munmap`, a TLB shootdown (a page
  unmapped while its address space runs on the other CPU) with
  `mprotect` off and on while a thread spins on the other CPU, memory
  bandwidth with the same `rep movsb` copy of 32 MiB buffers, the four
  kinds of round trip of ADR 0035 on one CPU and two, and three
  operations run on both CPUs at once for scaling.
- The report shows Radix, Linux in the virtual machine, their ratio and,
  for reference, the host's Linux. `results.json` keeps all three.
- `ob bench --gate` fails if any row is missing or Radix takes more than
  1.05 times as long as Linux in the virtual machine (`GATE_RATIO`):
  parity, with room for the noise of a shared runner. CI's `bench` job
  runs it on every pull request and every merge to main, so a change
  that makes Radix slower than Linux does not merge.
- Without KVM nothing is comparable, so the Linux virtual machine does
  not run and `--gate` refuses to pass.

## Consequences

- A hypervisor's costs fall on both systems alike, and the gate compares
  the kernels, not the kernels plus KVM.
- The gate holds every later phase to the same bar: new mechanisms add
  their own rows and Linux counterparts, and a row without both numbers
  fails.
- A noisy runner can still fail a row by chance. If that happens, the
  remedy is more rounds or the best of several runs, not a looser gate;
  the quiet test machine chosen before Phase 10 will make the numbers
  steadier.
- The bench job needs the runner's kernel, which Ubuntu installs readable
  only by root, so CI copies it first with `sudo`. Local runs on Debian,
  Ubuntu and Mint do the same once.
- ADR 0035 is superseded: its method stays, its baseline and its "no
  threshold" do not.
