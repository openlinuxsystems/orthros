<!--
SPDX-FileCopyrightText: 2026 The OrthrOS contributors
SPDX-License-Identifier: Apache-2.0 OR MIT
-->

# 0046. The performance gate measures Radix and Linux three times in turn and counts each operation's fastest run

- Status: Accepted
- Date: 2026-10-21

## Context

The performance gate (ADR 0039) booted Radix once and the Linux virtual
machine once, and failed when any operation took Radix more than 1.05
times what it took Linux. On the shared CI runners one run is not
enough. After the pull requests of ADRs 0041 and 0042 passed, the same
commits failed on main, and a documentation-only pull request failed
too, each time on a different row:

| Run | Runner | Row | Radix | Linux (VM) | Ratio |
| --- | --- | --- | ---: | ---: | ---: |
| 39653509331 | AMD EPYC | page unmapped while its space runs on the other CPU | 767 µs | 333 µs | 2.31 |
| 39662260495 | Intel Xeon 6973P-C | shared-memory channel round trip, one CPU | 1385 ns | 1267 ns | 1.09 |
| 39673637869 | AMD EPYC 7763 | system call on both CPUs at once | 92 ns | 63 ns | 1.46 |

Each of these rows passed in the runs before and after, and in the same
run a row like it did not move: in the last one a capability method on
both CPUs at once, which makes the same system call and more, took what
it takes on one CPU. What changes from run to run is the host: another
job on the machine, the two virtual CPUs placed on two threads of one
core, a virtual CPU that halted waking up late. The host only ever adds
time, and it rarely hits the same operation twice.

## Decision

- Under KVM, `ob bench` boots Radix and then the Linux virtual machine,
  three times in turn (`--runs` changes the number), and each operation
  counts its fastest run on each side. Measuring in turn means a slow
  stretch of the host falls on both systems, not on one.
- The gate's threshold stays at 1.05: the fastest run is the cost of the
  kernel with the least of the host's noise on top, so the margin is
  there for what remains, not for a bad run.
- The report says it gives the fastest of three runs, and keeps the
  kernel's event counts of the first run; `results.json` records the
  number of runs. Without KVM there is one run, since nothing is
  comparable then anyway, and `--instructions` counts the same every
  run, so it takes no `--runs`.

## Consequences

- The Benchmarks job takes about a minute longer: three boots of each
  system instead of one.
- A regression still fails: it is in every run, so it is in the fastest
  one too. One that shows only some of the time, such as a lock that is
  sometimes contended, can pass, and the instruction budgets of ADR 0045
  and the boot tests remain the checks for those.
- Both systems get the same treatment, so the comparison stays fair: a
  noisy run is set aside on whichever side it fell.
