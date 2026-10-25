<!--
SPDX-FileCopyrightText: 2026 The Dukovo contributors
SPDX-License-Identifier: Apache-2.0 OR MIT
-->

# 0107. The performance gate is parity with Linux, and an operation over it is measured again on both systems

- Status: Accepted
- Date: 2026-10-25

## Context

Since ADR 0039 the performance gate failed only when an operation took
Radix more than 1.05 times what it took Linux in the same virtual
machine. ADR 0046 kept that margin for the shared runners' noise once
each side counted its fastest of three runs. The margin let an
operation be up to 5 % slower than Linux and still pass, and the goal
is the opposite: no operation slower than Linux. By the end of the
post-Phase-9 audit's speed work every row but one was at 0.93 or under
on the AMD EPYC runners, and the one left, a signal sent, handled and
returned from, was at 1.01. On an Intel Xeon 8573C runner two rows
were over parity in the same week (a page unmapped while its space runs
on the other CPU at 1.14, and opening and closing a file at 1.06).

At parity the noise ADR 0046 measured matters more: three runs leave
a row that is level with Linux at the mercy of a host that slowed
Radix's three runs of it and not Linux's. A margin would hide that, and
would also hide a real loss of a few percent.

## Decision

- `ob bench --gate` fails if Radix takes longer than Linux in the same
  virtual machine at any operation: `GATE_RATIO` in
  `tools/xtask/src/bench.rs` is 1.00.
- When an operation is over the gate after the first runs, `ob bench
  --gate` boots Radix and the Linux virtual machine in turn again, up to
  `MORE_RUNS` (six) more times, and stops as soon as no operation is
  over. Each side keeps its fastest run of every operation, as in ADR
  0046. The gate fails if an operation is still over after the last
  run.
- A ratio over the gate is never shown as parity: the report gives it
  to three decimals where two would round it to 1.00. The report and
  `results.json` record how many runs there were and how many of them
  were the gate's.

## Consequences

- A loss to Linux, however small, fails CI. It is in every run, so more
  runs do not hide it; a run the host slowed is set aside on whichever
  side it fell, so more runs only remove noise.
- When every row is under the gate after three runs the job takes as
  long as before. Each extra run adds about two minutes; the most is
  twelve more minutes, on a pull request that is slower than Linux or
  on a very noisy runner.
- A row that is level with Linux can still fail on a runner whose
  noise always lands on one side. The answer to that is to make the
  operation faster, as the audit did for every row, never to give it
  room.
