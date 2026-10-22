<!--
SPDX-FileCopyrightText: 2026 The Dukovo contributors
SPDX-License-Identifier: Apache-2.0 OR MIT
-->

# 0067. Clocks are read through the process, and one kernel thread runs every timer

- Status: Accepted
- Date: 2026-10-22

## Context

POSIX programs read several clocks (`clock_gettime` on the real-time,
monotonic, boot-time and CPU-time clocks), sleep, and ask for signals
later: `alarm` and `setitimer`'s three interval timers per process,
and `timer_create`'s timers on any clock, with overruns counted when a
signal waits. A timer must fire while its process is busy elsewhere or
asleep, and a CPU-time timer must follow time the process spends on any
of its threads.

## Decision

- radix-process(2)'s `clock` reads Linux's clocks, by Linux's numbers:
  the real-time clocks from the hardware's wall clock, the monotonic and
  boot-time clocks from the scheduler's clock, and the process's and the
  thread's CPU time from what the scheduler counts. Nobody sets the
  clock yet (`clock_settime` fails with EPERM); Phase 12 brings NTS.
- A sleep is a timed futex wait on a word nobody wakes, which reports
  how long was left when a signal ends it, so `nanosleep` gives
  `rem` as POSIX says.
- The three interval timers (`set_timer`: real, user and profiling
  time, sending SIGALRM, SIGVTALRM and SIGPROF) and POSIX timers
  (`timer_create` to `timer_delete`, on the real-time, monotonic,
  boot-time and process CPU clocks) are kept in each process's POSIX
  state. **One kernel thread** runs them all: it sleeps until the next
  timer on the kernel's clock is due, or looks every 10 ms while a
  CPU-time timer is armed, since CPU time is counted at ticks. The
  processes with an armed timer are on one list, by weak reference, so
  a process that ends drops off it.
- An interval timer is set again by its interval at once; a POSIX timer
  with an interval waits until its signal is taken, and then counts the
  expiries missed meanwhile as the signal's overrun, as Linux's do. A
  POSIX timer can also signal one thread or start a thread
  (SIGEV_THREAD, which the C library runs on a helper thread). `exec`
  deletes POSIX timers and keeps interval timers, and a fork's child
  starts with no timer of either kind, as POSIX says.
- Setting a timer holds the armed list's lock until the timer is set,
  so that the timer thread cannot take the process off the list between
  (a race the `signal` C test's `rearms` part catches).

## Consequences

- `sleep`, `alarm`, `setitimer`, `timer_create` and the CPU-time clocks
  work as on Linux; the `time` and `signal` C tests check them,
  overruns included.
- Reading a clock is a capability call, not a read of a shared page as
  Linux's vDSO makes it, so `clock_gettime` costs a system call. A clock
  page mapped read-only into each process comes when a program's
  profile shows it matters; the instruction budgets (ADR 0045) will
  show it.
- A CPU-time timer fires within a tick (10 ms) of when it is due.
