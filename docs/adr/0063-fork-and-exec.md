<!--
SPDX-FileCopyrightText: 2026 The Dukovo contributors
SPDX-License-Identifier: Apache-2.0 OR MIT
-->

# 0063. fork copies what may be copied, and exec loads a program from a file into a fresh address space

- Status: Accepted
- Date: 2026-10-22

## Context

Radix's own way to start a program is to build a process from outside
with `create` and its methods and start it (ADR 0047): the maker decides
every capability the new program gets. POSIX programs, shells first of
all, start programs with `fork` and `exec` instead, or with
`posix_spawn`, and expect a child to inherit open files and to keep
running when its parent ends. Building that in the C library alone
would need every capability, every page and every register of the
caller copied by hand, through calls the caller cannot make on itself
mid-way.

## Decision

- **`fork`** (radix-process(2)) makes a child charged to the caller's
  domain that does not end with the caller: it holds its own original
  capability and joins the process tree (ADR 0061). It gets a
  capability space of the caller's size with a copy of every capability
  that has the **copy and transfer rights**, in the same slot with the
  same slot flags (a file's copy is the same open file description,
  offset and all); copies of the caller's kept capabilities; the
  caller's memory, copy-on-write (ADR 0060); the caller's signal
  actions, controlling terminal, limits and file mode mask; the calling
  thread's signal mask, alternate stack and scheduling; and one thread
  that goes on from the call with the caller's registers, extended state
  and thread pointer. It gets nothing pending and no POSIX timers.
- **`exec`** (radix-process(2)) runs a program from a file capability
  and a block of arguments, environment and extra strings (the first is
  the working directory). It reads the program a page at a time, checked
  by `radix-elf`, and maps each segment as anonymous memory filled from
  the file when touched. A program that names an interpreter
  (`PT_INTERP`) gets it mapped at a random place and starts there; a
  script that starts with `#!` gets its interpreter run, one level deep.
  The stack is laid out as the System V ABI and Linux lay it out, with
  the auxiliary vector, below a random top; the start-up page is mapped
  as for any program (radix-startup(2)).
- Everything that can fail is done in the new address space before the
  caller is touched. Then the caller's other threads stop, the new space
  takes the old one's place, slots marked `close_on_exec` are emptied,
  handled signals go back to their defaults, POSIX timers are deleted,
  and a thread starts in the new program.
- `exec` on a process that `create` made and that has not started makes
  it the caller's child instead and returns, `posix_spawn`'s way: a
  native program builds the child's capability space from outside, then
  starts a POSIX program in it. Spark starts the `posix` boot test's
  runner so (spark(8)). mlibc's own `posix_spawn` is musl's, on `fork`
  and `exec`.
- Slot flags live in the capability store with the slot rather than the
  capability (radix-capability(2)'s SlotFlags): `close_on_exec` is
  POSIX's FD_CLOEXEC.

## Consequences

- Capabilities without the copy or transfer right, such as one a
  service handed out sealed, never leak into a child, so `fork` widens
  nothing a capability did not already allow.
- `exec` keeps the process's number, parent, group, session, limits and
  file mode mask, so shells and job control work as on Linux.
- A program starts as fast however large it is, and only the pages it
  touches take memory; the price is a page fault per page on first use.
- `vfork` is `fork`; `exec` from a process with threads ends them, as
  POSIX says.
