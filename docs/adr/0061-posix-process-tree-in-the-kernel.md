<!--
SPDX-FileCopyrightText: 2026 The Dukovo contributors
SPDX-License-Identifier: Apache-2.0 OR MIT
-->

# 0061. The kernel keeps POSIX's process tree, groups and sessions, beside the capabilities

- Status: Accepted
- Date: 2026-10-22

## Context

Radix's processes are capability-named objects that belong to whoever
holds their capability (ADR 0047). POSIX programs expect more: a parent
that `wait` reports to, process numbers that `kill` and `waitpid` take,
process groups and sessions for job control, and a child that outlives
the process that made it. Some of this spans processes and cannot be
kept by a C library alone: a parent must hear of a child's end even if
the child dies of a fault, and a signal to a process group must reach
every member at once.

## Decision

- The kernel keeps **one process tree**: each member's number (its
  process number, which `getpid` returns), its parent, its process group
  and session, and what it has to tell its parent's `wait`. The rules
  are the `radix-posix` crate's (`tree`), plain data tested on the host
  with property tests for the invariants: numbers are unique, a zombie's
  parent is alive, a group's members share a session, a session leader
  leads its group. The kernel holds the tree behind one lock, taken
  after the capability store's and before any process's own state.
- A process joins the tree when it is forked, when `exec` starts a
  process `create` made (as `posix_spawn` does), or when it first calls
  a method that needs the tree, as the root of a session of its own;
  Spark and the programs it starts the Phase 6 way join so.
- A process with no parent is reaped as it ends: its parent's part
  falls to the kernel. A child whose parent ends loses its parent, so
  there are no orphans to adopt. A child is reaped at once too when its
  parent sets SIGCHLD to SIG_IGN or SA_NOCLDWAIT, as POSIX says.
- radix-process(2)'s methods from 11 on carry POSIX's calls: `fork`,
  `exec`, `wait` (waitpid's options and Linux's status words), `kill`,
  `ids`, `set_group`, `new_session` and the rest. A process may signal
  itself, its descendants and the processes of its session.
- A thread's number comes from the same count as processes, so a
  process's first thread has the process's number (`gettid() ==
  getpid()`), as on Linux.

## Consequences

- `wait`, job control and `kill` work as POSIX programs expect, and a
  Radix-native program that never calls a POSIX method pays nothing for
  them.
- Who may signal whom follows the tree, not user IDs: there are no
  users until Phase 13, which decides how they meet this rule.
- Reaping orphans at once means no process plays init's part for them;
  Spark supervises the services it starts by their capabilities
  (Phase 9), not as their parent.
