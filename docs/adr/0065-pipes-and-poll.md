<!--
SPDX-FileCopyrightText: 2026 The Dukovo contributors
SPDX-License-Identifier: Apache-2.0 OR MIT
-->

# 0065. Pipes are kernel file descriptions, and poll is one call over capabilities

- Status: Accepted
- Date: 2026-10-22

## Context

Shells connect programs with pipes, and most programs that wait on more
than one thing use `poll` or `select`. A pipe must give its readers
end-of-file when the last writer goes, and its writers SIGPIPE when the
last reader goes, even across `fork` and `exec`, so it has to know when
the last copy of each end is gone. Radix already has rings and
notifications for its own programs (ADR 0032, ADR 0034), but POSIX
programs wait on descriptors, and the C library cannot see when a pipe
written by another process has data.

## Decision

- radix-process(2)'s `pipe` makes a **kernel pipe**: a 64 KiB ring with
  a reading end and a writing end, each a node on a file system mounted
  nowhere, so each end is an open file description like any other
  (ADR 0064). The pipe learns that an end has gone when the last
  description of it goes. Reads take what is there and wait while
  nothing is; with no writer left an empty pipe reads as its end.
  Writes of up to PIPE_BUF (4096) bytes go in whole or not at all; a
  write to a pipe nobody reads raises SIGPIPE and fails with `gone`
  (EPIPE). `non_blocking` and `close_on_exec` can be set at once.
- radix-process(2)'s `poll` takes POSIX's `struct pollfd` array as it
  is, with descriptors as slots, a timeout and a signal mask for
  `ppoll`. A pipe's end and a terminal's side say what they are ready
  for; a notification is readable while bits are set; other files are
  always ready. If nothing is ready the caller waits for a change
  anywhere: whatever a poll looks at moves one global generation and
  wakes the pollers, which look at all their entries again.
- The C library builds `select` and `pselect` on `poll`.
- This departs from the plan, which put `poll` and `select` on top of
  rings (radix-ring(2)): a ring completes operations it was given, each
  on one capability, while `poll` asks whether any of many is ready, so
  one method of the process does it with less state and no ring per
  program.
- The byte ring is `radix-tty`'s `Ring`, shared with terminals and
  tested on the host.

## Consequences

- Pipelines, `popen`, `system` and programs that `poll` on pipes and
  terminals work as on Linux, and the C test `pipe` checks EOF, EPIPE,
  SIGPIPE and PIPE_BUF atomicity across processes.
- No wake-up is lost: a poller counts itself before it reads the
  generation, and a change reads the count after itself, with a full
  barrier on each side.
- Pollers wake for one another's changes: many pollers on a busy system
  pay for that. A wait list per object, as Linux has, replaces the
  global generation when a workload shows the cost
  (docs/design/posix.md).
- Named pipes need a file system that keeps them, so `mkfifo` fails
  with ENOSYS until one does.
