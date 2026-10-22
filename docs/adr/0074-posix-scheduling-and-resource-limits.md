<!--
SPDX-FileCopyrightText: 2026 The Dukovo contributors
SPDX-License-Identifier: Apache-2.0 OR MIT
-->

# 0074. Nice levels, POSIX scheduling and resource limits follow Linux's rules for a process without privileges

- Status: Accepted
- Date: 2026-10-22

## Context

POSIX programs set their own scheduling and limits: `nice` and
`setpriority`, `sched_setscheduler` and its kin for real-time threads,
`setrlimit` and `getrlimit`, `umask`, and thread names with
`pthread_setname_np`. Radix's scheduler already has a fair class with
nice levels and FIFO and round-robin real-time classes (ADR 0027), but
nothing named them as POSIX does, and processes had no limits. Users and
privileges come with Phase 13, so until then every process is
unprivileged, and the question is only what an unprivileged process may
do. Linux has answered that for years, and programs expect its answer.

## Decision

- radix-process(2)'s **`priority`** and **`scheduling`** give each
  program thread a POSIX policy (other, batch, idle, FIFO, round robin),
  a real-time priority from 1 to 99 and a nice level from -20 to 19,
  which the scheduler carries out. As on Linux they belong to threads: a
  process's number names its first thread, and `setpriority` on a
  process group or the caller's user changes every thread of every
  process in it. A thread starts with its maker's scheduling.
- Changing another process's threads needs what `kill` needs
  (ADR 0062). **Asking for more of the CPU** needs a limit, as on Linux
  without `CAP_SYS_NICE`: a nice level below 20 less `RLIMIT_NICE`, a
  real-time policy or a higher real-time priority within
  `RLIMIT_RTPRIO`, and leaving the idle policy within `RLIMIT_NICE`.
  Both limits are 0 by default, so a process can only lower itself.
- **Resource limits** (`limits`) are kept in each process's POSIX state,
  with Linux's defaults and its numbering of the sixteen resources;
  `fork` copies them and `exec` keeps them. A hard limit can only be
  lowered until Phase 13 brings privileges. The kernel enforces
  `RLIMIT_STACK` (the size of a new program's stack, from 128 KiB to
  1 GiB), `RLIMIT_NICE` and `RLIMIT_RTPRIO`; the C library enforces
  `RLIMIT_NOFILE`, since it numbers descriptors, never allowing more
  than a capability space has slots for. The others are kept for
  programs to read, and radix-posix(7) says so.
- The **file mode mask** (`file_mask`, `umask`) is kept beside the
  limits and inherited the same way; files' modes have no effect until
  Phase 13's users.
- **Thread names** (`set_thread_name`, `thread_name`) are up to 15
  bytes of UTF-8 per thread, Linux's limit, starting as the process's
  name, and the kernel log and the debugger show them.

## Consequences

- Programs that lower their own priority, build systems that run jobs
  under `nice`, and Rust's thread names work as on Linux; the `limits`
  and `thread` C tests check the rules, the limits and the names.
- No POSIX program can raise its own hard limits or real-time priority
  yet. Phase 13 gives privileged users the right to.
- Limits nobody enforces may surprise a program that relies on one (an
  `RLIMIT_CPU` that never ends it). Resource domains (ADR 0029) are
  Radix's own way to bound memory and CPU, and later phases can enforce
  more limits through them.
