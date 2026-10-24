<!--
SPDX-FileCopyrightText: 2026 The Dukovo contributors
SPDX-License-Identifier: Apache-2.0 OR MIT
-->

# 0101. Spark supervises services from simple key-value files, in a domain each, and answers a control socket

- Status: Accepted
- Date: 2026-10-23

## Context

Spark v1 (Phase 6) started one program and waited. Phase 9 makes it the
system's supervisor: it starts the services at boot, starts them again
when they end, stops them in order at shutdown, and plays the role the
out-of-memory policy expects of a supervisor (ADR 0098). Administrators
need to see and control the services, with the commands they know.

systemd's unit files and D-Bus interface are large and Linux-specific;
runit's and s6's directories of scripts need a shell for every service.
Dukovo needs a small, declarative format that Spark parses itself, in
`no_std` Rust, before any shell runs.

## Decision

- A service is a file `/system/etc/spark/services/NAME.service` of
  `key = value` lines and `#` comments, with the keys program,
  arguments, description, order, restart (always, on-failure or
  never), weight, memory, oom (service or app), environment and
  enabled. The system's own come from the defaults Spark copies into
  `/system/etc` (spark(8)), so a file is turned off by editing it, not
  by removing it.
- Spark starts the enabled ones at boot, lowest order first, each as a
  POSIX program in a domain of its own made with the file's weight,
  memory limit and class, with `/dev/null` as input and a pipe as output
  whose lines Spark copies to the kernel log after the service's name.
- It hears of its children's ends from a notification the kernel
  signals (radix-process(2)'s `notify_children`), and waits in `poll`
  for that, the services' output, the control socket and the program a
  boot test named, so it never polls on a timer.
- A service that ends is started again as its file says, after 0.1 s,
  doubling for each quick end in a row (less than 10 s after its start)
  up to 30 s, so a crashing service costs little.
- Spark listens at the Unix socket `/run/spark/control` for one-line
  requests: poweroff, reboot, halt (with a delay or `--force`), cancel,
  status, and start, stop or restart of a service. systools' shutdown(8),
  reboot(8), poweroff(8), halt(8) and service(8) are its clients, with
  systemd's and Debian's command lines.
- Shutting down stops the services by order groups, the highest first:
  SIGTERM, then after 5 s the domain's `stop`, then a second more; then
  every service domain is stopped and the power capability called (ADR
  0099).
- A boot test that names a program on the kernel command line starts
  only that program, unless `--services` comes first.

## Consequences

- A normal boot reaches consoled and a shell with no script run; the
  only service in Phase 9 is consoled, and Phase 12's network services
  and Phase 13's sshd are files beside it.
- There are no dependencies between services beyond their order; a
  service waits for what it needs itself, as Spark starts the next
  group without waiting for the last to be ready.
- Anyone can send Spark requests until users and permissions come in
  Phase 13, when the socket's permissions will limit them to the
  administrators.
- The format is Dukovo's own; a service written for systemd needs a
  short file of its own.
