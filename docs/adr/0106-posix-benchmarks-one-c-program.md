<!--
SPDX-FileCopyrightText: 2026 The Dukovo contributors
SPDX-License-Identifier: Apache-2.0 OR MIT
-->

# 0106. The POSIX rows of `ob bench` are one C program, run on Radix and on Linux

- Status: Accepted
- Date: 2026-10-25

## Context

`ob bench` (ADR 0039) measured Radix's own interfaces: system calls,
capability methods, endpoints, notifications, channels, rings, pages
and files, each next to the closest Linux call that `tools/linux-bench`
makes. Programs ported to Dukovo do not use those interfaces directly.
They make POSIX calls through the C library, and the POSIX layer turns
them into Radix's (ADR 0060 on). Nothing measured that layer: creating a
process, running a program, a pipe's throughput, a signal, `poll` or a
futex, the calls a shell, a build or a server spends its time in. The
audit after Phase 9 asked for every row to be as fast as Linux, and for
these rows too.

The Linux side has to make the same calls the same way, or the ratio
says little: a Rust program with its own loops on Linux and a C program
on Radix would compare two programs as well as two systems.

## Decision

- The POSIX benchmarks are one C program, `userspace/posix/bench.c`. The
  Radix build makes it with the other C test programs, on mlibc, into
  the boot archive at `/system/tests/posix/bench`. `ob bench` builds the
  same file for Linux with the same Clang (or else the host's C
  compiler), at the same `-O2`, statically on the host's C library.
- On Radix, each run boots the test image a second time, with Spark
  running the program (`-- --posix /system/tests/posix/bench`) and no
  services, and reads its `BENCH` lines from the kernel log. On Linux,
  the program is in the virtual machine's initramfs at `/posix-bench`,
  and `linux-bench` runs it last, with every CPU allowed; on the host,
  `linux-bench --posix-bench` runs it.
- The program pins itself to the first CPU it may run on, and the other
  thread of the two-CPU futex row to the second, with
  `sched_setaffinity` and `pthread_setaffinity_np`. mlibc had neither on
  a system without its Linux option: patch 0043 gives the glibc option
  `sched_getaffinity`, `sched_setaffinity` and the `CPU_*` macros, and
  the Radix layer implements the four affinity sysdeps with
  radix-process(2)'s `affinity` and `set_affinity`.
- The rows: `fork` and `waitpid` of a child that exits at once; `fork`,
  `execv` of the program itself and `waitpid`; 256 MiB through a pipe in
  64 KiB writes between two processes on one CPU, per MiB; a signal a
  process sends itself, through its handler and back; `poll` on a pipe
  with data to read; and a turn passed there and back between two
  threads through two semaphores (futex wake and wait in both C
  libraries), on one CPU and on two. The gate holds them like every
  other row.

## Consequences

- The report shows the cost of the POSIX layer next to Linux's, and a
  change that slows it fails the gate like one that slows the kernel.
- Each run takes one more boot of Radix, a few seconds under KVM.
- The C library is part of what is measured on both sides: mlibc on
  Radix, glibc on Linux. That is what a ported program gets on each.
- C programs on Dukovo can pin themselves and their threads to CPUs,
  as on Linux.
