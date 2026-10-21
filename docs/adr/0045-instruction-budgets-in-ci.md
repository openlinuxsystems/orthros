<!--
SPDX-FileCopyrightText: 2026 The OrthrOS contributors
SPDX-License-Identifier: Apache-2.0 OR MIT
-->

# 0045. CI holds every operation on one CPU to an instruction budget, counted by QEMU

- Status: Accepted
- Date: 2026-10-21

## Context

The performance gate (ADR 0039) compares Radix's timings with Linux's in
the same kind of virtual machine on a shared CI runner, and fails beyond
1.05 times Linux to leave room for the runners' noise of a few percent.
A hot path that grows by less than its row's margin passes, and a few
such changes add up before any one fails. Finding the costs of ADRs 0040
and 0042 took instruction traces made by hand under QEMU, where the
container had no KVM. The audit that closed Phase 5 asked for
performance budgets in CI that catch a regression when it is made.

## Decision

- `ob bench --instructions` boots the benchmark image under QEMU's
  instruction counting (`-icount shift=0,align=off`, no KVM), where each
  instruction moves the guest's clock by one nanosecond. The benchmark
  programs time themselves with the TSC as always, so each `BENCH` line
  gives the user and kernel instructions one operation took. The report
  goes to `out/bench/instructions.md` and the job's summary.
- `BUDGETS` in `tools/xtask/src/bench.rs` gives each operation on one CPU
  a budget: what it took when the budget was set, plus a tenth, rounded
  up. With `--gate` the command fails when an operation goes over, and
  CI's `instructions` job runs it on every pull request and every merge;
  it takes a few minutes and needs no KVM. A change that needs more
  raises the budget in the same pull request and says why, so growth is
  a decision someone writes down.
- Operations on two CPUs depend on how QEMU interleaves the CPUs, and its
  clock runs on while a CPU waits, so they have no budget; the timings
  under KVM cover them.

## Consequences

- The counts are the same on every machine with the same QEMU (CI and
  the build container both have Ubuntu 24.04's 8.2), so a budget can sit
  far closer to the real cost than a timing threshold can.
- Counting instructions misses what does not cost instructions: cache
  misses, contention, VM exits. The timings under KVM, held to Linux,
  remain the gate for those, and the two jobs run side by side.
- A newer QEMU or a different `-cpu max` can shift the counts a little;
  the tenth of headroom absorbs that, and a larger shift is a reason to
  re-measure and reset the budgets in one change.
