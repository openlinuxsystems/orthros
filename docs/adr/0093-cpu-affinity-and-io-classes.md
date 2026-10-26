<!--
SPDX-FileCopyrightText: 2026 The Dukovo contributors
SPDX-License-Identifier: Apache-2.0 OR MIT
-->

# 0093. Threads have Linux's CPU affinity, and processes Linux's I/O classes

- Status: Accepted; an NVMe disk has a queue for each CPU since [0115](0115-nvme-from-the-specification.md)
- Date: 2026-10-23

## Context

Phase 9's administration command set has `taskset`, which pins a
process to some CPUs, and `ionice`, which sets how urgently its disk
requests are served, as on Linux. Administrators use both: a backup in
the idle I/O class, a latency-sensitive service kept off the CPUs that
take a NIC's interrupts. Both need the kernel's help.

The scheduler already keeps a mask of allowed CPUs for each thread
(ADR 0027), for the kernel's own pinned threads, but nothing let a
program set it. Disks served requests in the order they came, whoever
made them.

Linux's interfaces are `sched_setaffinity` and `sched_getaffinity`, by
thread, with a byte mask, and `ioprio_set` and `ioprio_get`, by
process, process group or user, with four classes (none, real-time,
best effort, idle) and eight levels in the first two.

## Decision

radix-process(2)'s `set_affinity` and `affinity` set and read the mask
of a thread, named by its number (0 for the caller, a process's number
for its first thread), as Linux's calls do: bit n % 8 of byte n / 8
for CPU n, up to the kernel's 256. Bits past them are ignored. The mask
is limited to the CPUs online, and one with none of them fails with
invalid_argument. A thread starts with the mask of the thread that made
it, through `fork`, `exec` and the native `start`, as Linux's
inheritance through `clone` and `execve` has it.

radix-process(2)'s `io_priority` reads, and with `set` changes, the I/O
class and level of a process, of a process group's members, or of a
user's processes, as `ioprio_set` and `ioprio_get` do, with Linux's
class numbers. Reading returns the most urgent among them, as Linux
does. A forked child has its parent's, and `exec` keeps it. A process
starts with none, which is served as best effort at level 4, the
middle, as Linux serves it; the kernel's own threads (the flusher) are
served at that level too.

A disk serves them by rank: the real-time class's levels, then best
effort's, then idle. A request takes one of the disk's slots only while
no more urgent request waits for one, and an idle-class request only
while the disk has nothing else waiting or in flight, as Linux's idle
class is served. Once in flight, every request is the device's alike.

Changing another process's affinity or I/O class needs what `kill`
needs, as `setpriority` does (ADR 0074). Linux lets only root set the
real-time I/O class; until users come in Phase 13 there is no root to
tell apart, so any process that may signal another may set it, and
Phase 13 adds the check. `taskset` and `ionice` call these methods
directly (ADR 0096).

## Consequences

- `taskset -c 2-3 make` and `ionice -c idle tar ...` work as on Linux,
  and a pinned process's children stay pinned.
- Ranks decide only who takes a free slot: a stream of real-time
  requests can keep best-effort ones waiting, and best effort can keep
  idle waiting indefinitely, as on Linux. There is no time slicing
  between classes as Linux's BFQ scheduler has; it can come with NVMe's
  many queues in Phase 10 if measurements call for it.
- The C library does not declare `sched_setaffinity` and
  `sched_getaffinity`, which mlibc keeps in its Linux option that Radix
  builds without; C programs that need them get them when that option
  is revisited. Rust's `std::thread::available_parallelism` counts
  the online CPUs through `sysconf` on Radix, so a pinned Rust program
  still sees all of them, where on Linux it counts its mask.
- Interrupts still go where the kernel sends them: steering a device's
  interrupts away from some CPUs comes with the drivers that need it.
