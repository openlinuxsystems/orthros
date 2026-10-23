<!--
SPDX-FileCopyrightText: 2026 The Dukovo contributors
SPDX-License-Identifier: Apache-2.0 OR MIT
-->

# 0095. A trace is a pipe the kernel writes a record to for each call

- Status: Accepted
- Date: 2026-10-23

## Context

`strace` is how an administrator finds out why a program fails without
saying why: which file it could not open, which call hangs. Phase 9's
command set has `rtrace` for this, and it needs the kernel to tell it
what a process asks.

Linux's `strace` uses `ptrace`: the tracee stops at each system call's
entry and exit, and the tracer reads its registers and memory with more
calls, several context switches for each traced call. `ptrace` also
serves debuggers, which need to stop, change and resume a process,
and it is among the most intricate interfaces in Linux. Radix's calls
are mostly messages to capabilities (`cap_call`), described by ridl
(ADR 0033), so the kernel knows each one's interface, method and
arguments, and which arguments are bytes.

## Decision

radix-process(2)'s `trace` makes a trace of a process and puts its file
in a slot: the reading end of a pipe the kernel writes (radix-trace(2)).
For each system call of the process's threads, the kernel writes a
record as the call returns: the thread, when the call began and how
long it took, the call's number, label and capability kind, its
argument and result registers, and its status. With the `data` flag the
record also carries the bytes of the call's byte arguments, up to 256
of each: what each input argument held as the call began (a path, a
write's data), and what the call wrote to its first output argument (a
read's data). The kernel finds them, and the length of the output, from
the catalog ridl generates (`radix_abi::catalog`). Signals delivered,
programs started, children forked and the process's end are records
too. With the `children` flag, children forked from then on are traced
by the same trace, as `strace -f` follows them.

A record is one write the pipe takes whole. A thread that would add a
record to a full pipe (64 KiB) waits for the tracer to read, as a pipe's
writer does, but leaves room for one record: the records of a process's
end and of a signal that ends or stops it never wait, since they may be
written by the tracer itself, and are lost if there is no room. A
signal cuts a wait short, and the next record says records were lost.

The traced process does not stop and cannot tell it is traced, but by
the time its calls take. Only it pays: the system call path looks at one
flag of the process, and a traced process takes a separate, cold path.
A process has one trace at a time, and a second fails with busy; a
process cannot trace itself; tracing needs what `kill` needs. Closing
the trace file ends the trace: each process it follows goes on untraced
from its next record.

`rtrace` (ADR 0096) prints the records as strace prints its lines, with
each method by name and its arguments as its interface describes them.

## Consequences

- Tracing costs the traced process one write to a pipe a call, not
  stops and context switches, so a traced server keeps nearly its
  speed; a tracer that cannot keep up slows it, rather than losing
  records, except for the end and fatal signals.
- The trace shows what Radix's calls are, messages to capabilities by
  method, not Linux's system calls: `open` is `directory.open` on a
  directory's capability. Administrators read the method names that
  radix-ipc(7) and the generated pages document.
- A trace only observes. A debugger that stops a process and changes
  its registers or memory needs another interface, which comes with the
  developer tools; the kernel debugger (radix-debug(7)) serves the
  kernel's own needs until then.
- A call that does not return (an `exec` of the caller's own process, an
  `exit`) has no call record; its event record says what happened.
