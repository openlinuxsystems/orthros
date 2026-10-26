<!--
SPDX-FileCopyrightText: 2026 The Dukovo contributors
SPDX-License-Identifier: Apache-2.0 OR MIT
-->

# 0094. /proc tells the native tools what they show, /dev/kmsg gives the log, and files keep the path they were opened by

- Status: Accepted; `/proc` and `/dev` are `/processes` and `/devices` since [0113](0113-plain-names-for-the-root-folders.md)
- Date: 2026-10-23

## Context

ps, top, pidstat, lsof, vmstat, free and dmesg need to know what the
kernel knows: each process's parent, group, session and terminal, its
arguments, CPU time, scheduling, memory, faults and I/O; each thread's
state; what each process holds open, and by what path; what each CPU
and disk has done; and the kernel's log. Until now `/proc` had a
process's number, name, state and memory, the mounts, memory and
uptime (ADR 0057), and the log was only on the serial line and the
console.

On Linux these tools read Linux's `/proc` files, whose formats grew one
by one: `/proc/<n>/stat` is a line of 52 numbers by position, `status`
mixes units, `fd/` is a directory of links. ADR 0057 chose `key: value`
text for Radix's `/proc` instead, and the native tools are written for
Radix (ADR 0096), so nothing needs Linux's formats.

Linux finds an open file's path from its dentry, which knows its name
and parent and follows renames. Radix's files are nodes of their file
system with no names of their own (ADR 0054); a path exists only as the
walk that opened it.

## Decision

`/proc` grows, in the format of ADR 0057, documented in radix-proc(5):

- `/proc/<n>/status` gains the process's parent, group, session and
  terminal, its run state, start time, CPU time split as ADR 0092 says,
  scheduling policy, priority and nice level, I/O class and level
  (ADR 0093), virtual and resident memory, page faults, bytes read and
  written, context switches, whether it is traced (ADR 0095), and its
  working directory, which the C library tells the kernel at each
  `chdir` with radix-process(2)'s `set_directory`;
- `/proc/<n>/cmdline` is the arguments its program started with, each
  ending in a NUL, as on Linux; the kernel keeps up to a page of them
  when it starts the program, as Linux shows up to a page;
- `/proc/<n>/threads` and `/proc/<n>/files` are a line per thread and
  per capability held, of `key=value` fields: a thread's state, CPU,
  times and scheduling; a capability's slot or descriptor number, kind,
  rights and, for a file or directory, its access, node, size, file
  system, mount, offset and path;
- `/proc/stat` is each CPU's user, system and idle time and switches,
  the threads ready to run and those waiting for a disk, and the disks'
  reads and writes; `/proc/loadavg` the load averages (ADR 0092).

A file object keeps the path it was opened by, for `files`: the path its
open was given, joined to the path of the directory it was opened from,
with `.` and `..` worked out by name and symbolic links not followed,
so it is the path the program named. It lives in the object, 104 bytes,
so an open allocates nothing more; a longer path keeps its start and is
marked cut. A mount's root has its mount's path. A file renamed while
open keeps its old path.

`/dev/kmsg` gives the kernel log as Linux's does: each read returns one
record, `priority,sequence,microseconds,-;text`, the oldest the reader
has not read, and `dmesg -C` clears the log for later readers with
radix-file(2)'s `control` and the write right. A description's offset
is its place in the log. The log may be written with any lock held, even
in an interrupt, so it never wakes anyone: while `/dev/kmsg` is open, a
kernel thread of its own looks for new records every 10 ms and wakes
readers and `poll`. The timer's tick cannot do it, since waking takes
the wait table's lock, which threads take around other locks with
interrupts on.
A reader that falls behind the ring fails once with `EPIPE`, as on
Linux, then goes on from the oldest record kept. Writing a line logs
it, at the level a `<N>` before it gives.

## Consequences

- The native tools read one regular format, and radix-proc(5) is the
  whole contract between them and the kernel. Programs written for
  Linux's `/proc/<n>/stat` or `/proc/<n>/fd/` find neither; Linux binary
  compatibility (Phase 29) will have to present Linux's formats itself.
- `/proc/self` does not exist yet; programs use their own number.
- A file's path in `files` is the one it was opened by, so after a
  rename lsof shows the old one, and a path through a symbolic link
  shows the link, where Linux shows the target. A path past 104 bytes
  is cut, which lsof marks.
- Recording the path costs an open and a close about 190 instructions
  (5%, `ob bench --instructions`): the object is 106 bytes bigger, and
  the directory's path and the new names are copied into it in place.
  `path::parse` was made cheaper for ASCII paths on the way, which pays
  some of it back on every call that takes a path.
- `dmesg -w` sees a new record within 10 ms. The watching thread runs
  100 times a second while `/dev/kmsg` is open, and sleeps otherwise.
- Each process keeps up to a page of arguments on the kernel's heap,
  where Linux reads them from the process's own memory; the heap is not
  charged to its domain, but a page a process is bounded by the
  processes a domain may make.
