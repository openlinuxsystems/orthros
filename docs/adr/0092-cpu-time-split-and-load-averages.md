<!--
SPDX-FileCopyrightText: 2026 The Dukovo contributors
SPDX-License-Identifier: Apache-2.0 OR MIT
-->

# 0092. CPU time is split by the timer's samples, and the load averages are Linux's

- Status: Accepted
- Date: 2026-10-23

## Context

The administration tools of Phase 9 (ps, top, pidstat, vmstat, uptime,
w) show where the CPU goes: each process's time in user mode and in the
kernel, each CPU's busy and idle time, and the load averages. `times`,
`getrusage` and `wait`'s rusage give programs the same split.

Radix measures a thread's CPU time exactly, from the clock at each
switch (ADR 0027), but not how much of it the thread spent in user mode.
Measuring that exactly means reading the clock at every entry into the
kernel and every return, two more reads on each system call, whose
budget is a few hundred instructions (ADR 0045). Linux measures it that
way only with `CONFIG_VIRT_CPU_ACCOUNTING_GEN`, for its tickless CPUs,
and pays for it; by default it samples at the timer interrupt.

The load average is a number administrators read without thinking:
1.00 means one CPU's worth of work waiting or running. If Radix's
differed in what it counts or how it decays, the number would mislead.

## Decision

At each timer tick (100 a second), a CPU that is not idle counts one
sample for its running thread and for itself: user if the interrupt
came from user mode, system if it came from the kernel. A thread's
exact CPU time is shared out between user and system in its samples'
proportion when it is read (`radix_sched::split_time`); with no sample
yet, all of it is user time, as Linux counts it. A proportion taken
afresh could take user time back when a kernel sample came, so each
read moves the line between the parts no more than it must for neither
to fall below what the last read gave (`radix_sched::Split`, as Linux's
`cputime_adjust` does). A process's time is shared out the same way,
from the sums of its threads' times and samples with those of the
threads that have ended. A CPU's busy time is shared out the same way
by its own samples, and its idle time is measured exactly, the idle
period under way included.

The load averages are Linux's `calc_load`: every 5 seconds the boot
CPU's tick counts the threads ready to run or running on every CPU, and
those waiting for a disk to carry out their request, and folds the
count into three averages that decay over 1, 5 and 15 minutes, in fixed
point with 11 bits of fraction, with Linux's constants and rounding
(rounded up while the load rises, so that a steady load of n reaches
n). `/proc/loadavg` prints them in hundredths, rounded as Linux rounds
them, and `getloadavg` reads them there.

Each thread also counts its switches: voluntary when it blocks, and
involuntary when it is preempted or yields, as Linux's `nvcsw` and
`nivcsw` do.

## Consequences

- The system call path pays nothing for the split: one counter is
  added at each tick, on the tick's own path.
- The split is a sample: a thread that runs for less than a tick at a
  time, or always in the same phase as the tick, can be misattributed,
  as on Linux. The total is exact.
- CPU-time timers (`ITIMER_VIRTUAL`, `ITIMER_PROF`) fire within a tick.
- A load of 1.00 on Radix means what it means on Linux, so uptime(1),
  w(1) and top(1) need no explanation, and monitoring that alerts on the
  load average works unchanged.
- Every online CPU ticks, idle or not, so the samples and the load's
  5-second clock never stop. If a busy CPU ever stops its tick, as
  Linux's `nohz_full` CPUs do, it must measure at its entries into the
  kernel and returns instead, and pay for it.
