<!--
SPDX-FileCopyrightText: 2026 The Dukovo contributors
SPDX-License-Identifier: Apache-2.0 OR MIT
-->

# 0035. System call and IPC costs are measured on every merge, next to the closest Linux operations on the same machine

- Status: Superseded by [0039](0039-linux-in-the-same-virtual-machine-and-a-performance-gate.md)
- Date: 2026-10-21

## Context

Phase 5 asks for a benchmark harness for system call and IPC round
trips, recording Linux numbers on the same machine as a baseline, and
for CI to publish the numbers on every merge. A number on its own says
little: what matters is how Radix compares with the system people know,
measured the same way on the same hardware, and whether a change made it
slower. CI runners are virtual machines whose speed varies from run to
run, and Radix can only run there inside QEMU.

## Decision

- `ob bench` boots a test image with `selftest=bench` on 2 CPUs, under
  KVM when the machine has it, reads the `BENCH name=… ns=… iterations=…`
  lines the benchmark programs log, then runs `tools/linux-bench` on the
  host, which prints the same lines for the closest Linux operations.
  It writes a Markdown report and `results.json` under `out/bench/`.
- The method follows lmbench (Larry McVoy and Carl Staelin):
  `lat_syscall` for system calls, `lat_pipe` and `lat_unix` for round
  trips. A measurement times a loop of many operations (200,000 system
  calls, 100,000 round trips) and divides; Radix programs read the TSC
  with `lfence; rdtsc`, at the frequency the start-up page gives, and
  `linux-bench` uses `Instant`. Round trips run once with both ends on
  one CPU and once on two, pinned on both systems.
- Each Radix measurement is paired with a Linux one: a null system call
  with `getppid` through `syscall(2)`, `yield` with `sched_yield`, random
  words with a 64-byte `getrandom`, an endpoint call with a pipe round
  trip, a notification ping-pong with eventfds, a channel with a futex
  ping-pong, and a ring round trip with a Unix socket. A capability
  method (`cap_info`) has no Linux counterpart.
- CI's `bench` job runs `ob bench` on every pull request and every merge
  to main, puts the report in the job's summary and keeps the numbers as
  an artifact for 90 days. The job does not fail on a slow number:
  runners vary too much for a threshold to mean anything yet.
- Without KVM the report says that the Radix numbers measure QEMU's
  emulation and nothing about the kernel.

## Consequences

- Every merge records what a system call and each kind of round trip
  cost, with the runner's Linux beside it, so a regression is visible in
  the history of the job's summaries and artifacts.
- Under KVM a system call stays in the guest, so single-CPU numbers
  compare fairly with the host's; a wake-up across CPUs is an interrupt
  the hypervisor delivers, so two-CPU numbers carry its cost.
- Thresholds that fail CI come once there is a quiet machine to run on
  (the test machine chosen before Phase 10), where numbers are stable.
- New mechanisms (Phase 6 processes, I/O rings) add their own `BENCH`
  lines and Linux counterparts; a unit test checks that every name in
  the report's table is one the programs print.
