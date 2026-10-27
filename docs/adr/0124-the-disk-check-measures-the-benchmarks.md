<!--
SPDX-FileCopyrightText: 2026 The Dukovo contributors
SPDX-License-Identifier: Apache-2.0 OR MIT
-->

# 0124. The disk check also measures the benchmarks on the machine

- Status: Accepted
- Date: 2026-10-27

## Context

`ob bench` measures Dukovo and Linux in two virtual machines set up the
same way, and its gate on every pull request holds each operation at
Linux's cost or under (ADR 0107). A virtual machine is not the machine
it runs on: under a hypervisor every timer write, interrupt sent and halt
is an exit to the host, and Linux in a virtual machine pays them too. So
each sub-phase of Phase 10 also ends with numbers from a real machine,
the Dell Inspiron 15 5570 (ADR 0122), measured on Dukovo and on Linux
in turn.

The Dell has no serial port, and Dukovo drives no USB device before
Phase 11, so the stick Dukovo boots from cannot take its numbers. The
disk check already carries what it finds from Dukovo to Linux: it saves
a report in a MiB of the disk it checks, which Linux's side reads back
after a restart.

## Decision

The disk check also measures the benchmarks of `ob bench`, on both
systems, and carries Dukovo's numbers to Linux in its report.

- The disk check's boot entry runs the kernel's benchmarks
  (`selftest=bench`) and then the disk check. The new kernel flag
  `init-after-selftest` starts the first program after a self-test,
  which otherwise has the machine to itself.
- On Dukovo, `disktest --bench PROGRAM`, once the disk check has saved
  its report, reads the kernel's version and every `BENCH` and
  `BENCHCOUNT` line of the kernel log (`/devices/kmsg`), runs the POSIX
  benchmarks (`userspace/posix/bench.c`), keeps their lines too, and
  saves the report again with all of them. The POSIX benchmarks get no
  disk: the disk check measures the disk's transfers itself.
- On Linux, `run.sh` runs `disktest --bench-out FILE`, which writes
  Dukovo's lines, each after `dukovo: `, and then `linux-bench` with the
  same POSIX benchmarks built for Linux, whose lines follow `linux: `.
  Both go to `dukovo-bench.txt`, with the machine's processor and
  Linux's version. `ob image --disk-check` puts both programs on the
  stick, built as static programs, as `ob bench` builds them for its
  Linux virtual machine.
- `ob bench --from FILE` reads such a file and writes the report of
  `ob bench`, with Linux on the same machine as the baseline, the ratio
  of each operation, and the operations over 1.00 named.
- The live system is booted with `mitigations=off`, as the virtual
  machine's Linux is, since Dukovo has no mitigations of CPU
  vulnerabilities yet; `run.sh` says in the file whether it was.
- Linux measures as Dukovo runs, the harder baseline, for the disk's
  transfers as for the benchmarks. Dukovo sets no CPU performance states
  before Phase 11c (Intel HWP), so its processor runs at the clock the
  firmware left it; it idles in C1 (`MWAIT` with hint 0); and its AHCI
  driver keeps SATA links at full power. While Linux measures, `run.sh`
  turns its turbo off and holds every CPU at the highest clock without
  it, turns off every idle state deeper than C1, and sets every SATA
  link's power policy to `max_performance`; it puts each setting back at
  the end. The first disk check on the Dell, before this, measured Linux
  with its power saving on, and every transfer took Linux 1.3 to 2.8
  times Dukovo's time, most likely for waking from deep idle states and
  bringing the SATA link back from low power on each transfer.
- The POSIX benchmarks also time a chain of dependent additions, one
  clock cycle each (`cpu_cycle`), so the report shows the clock each
  system ran at; it is not one of the rows the gate compares.
- The disk check writes over a whole disk, so nothing on it may be
  mounted. Its boot entry gives Spark `--no-volumes`, which leaves every
  disk's filesystems unmounted, and disktest, on either system, refuses
  a disk with a mounted filesystem. On the Dell's first check, Spark had
  mounted the FAT partition of the disk being checked; nothing wrote to
  it, but a filesystem's write-back could have overwritten the check's
  places, or the check the filesystem.

## Consequences

- One boot of each system gives the disk check and the benchmarks, in a
  few minutes more than the disk check alone.
- The kernel log holds 512 records, so the kernel's lines must still be
  in it when disktest reads it; disktest says so when one was lost.
- The machine's numbers are a check, not a gate: they come from one
  machine, at the end of a sub-phase, so `--from` takes no `--gate`.
  Their report goes in the pull request of the sub-phase's version, and
  an operation slower than on Linux is worked on as in CI.
- The Framework Desktop runs the same check once it is free (Phase 20).
