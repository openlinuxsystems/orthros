<!--
SPDX-FileCopyrightText: 2026 The Dukovo contributors
SPDX-License-Identifier: Apache-2.0 OR MIT
-->

# 0050. The kernel starts one program, Spark, from the boot archive, gives it the archive and a pool of memory, and stops if it ends

- Status: Accepted
- Date: 2026-10-21

## Context

Something must start the first program, and everything after it should
be started by programs, not by the kernel (spec 4: the kernel holds
mechanism, user space holds policy). The kernel already has a loader for
its self-tests (ADR 0036). Linux starts `/sbin/init` or the program
`init=` names and passes it the words after `--` on its command line;
Zircon starts `userboot`, which starts the component manager; seL4
starts one root task with every capability.

## Decision

- At the end of boot, when no self-test is asked for and the boot loader
  loaded a boot archive (ADR 0049), the kernel starts the program at
  `init=PATH` in the archive, `/system/bin/spark` by default. The words
  after a lone `--` on the kernel's command line are its arguments, after
  its path, as with Linux's `init`; the kernel's own flags end there.
- Spark is loaded by the kernel's loader like any program (ADRs 0047 and
  0048), with three things more: the archive mapped read-only at a
  random place in the loader's window, its address and size in the
  start-up page; a memory object of 4 MiB in `SLOT_FIRST_EXTRA`, from
  which it makes the memory of the programs it starts; and the kernel's
  promise to keep Spark's process capability for good.
- Spark is the root of every other program: each process it starts is
  made for it (radix-process(2)), so its children end only if it lets
  them go or they exit. **If Spark ends, the kernel panics**, with the
  exit code in the message: a system without its first program cannot
  run, and a loud stop is easier to debug than a silent hang.
- Spark v0 (Phase 6) starts the one program its first argument names
  (`/system/tests/hello` by default), passes it the remaining arguments,
  waits for its exit through `notify_exit`, reaps it, checks that every
  page of the memory it gave came back, and then waits. Phase 7 has it
  mount file systems and create the directory layout; it becomes the
  supervisor of the system's services, with resource domains and
  restarts, in Phase 9, when there are services to supervise.

## Consequences

- The kernel never starts another program, but for the self-tests'
  programs from the `usertests` image (ADR 0036), which stay kernel
  scaffolding so that they test the kernel alone; they are now loaded
  into processes by the same loader as Spark. The `spark` boot test checks
  the whole path under UEFI: the kernel starts Spark, Spark starts
  `hello` with arguments from the command line, `hello` prints, exits
  and is reaped, and all of its memory comes back.
- A different first program can be tested without rebuilding the
  kernel (`init=/system/tests/hello`), as long as it is in the archive.
- Until Phase 9 the kernel's own stand-in still applies the out-of-memory
  policy (ADR 0029), and every process runs in the root domain.
