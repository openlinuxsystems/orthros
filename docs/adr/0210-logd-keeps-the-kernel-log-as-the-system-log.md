<!--
SPDX-FileCopyrightText: 2026 The Dukovo contributors
SPDX-License-Identifier: Apache-2.0 OR MIT
-->

# 0210. logd keeps the kernel log as the system's log

- Status: Accepted
- Date: 2026-10-30

## Context

Phase 13a. Until now Dukovo's only log was the kernel's ring of 512
records (`/devices/kmsg`, `dmesg`): the kernel's own lines, the lines
programs write to it, and the services' standard output and error,
which Spark copies there after each service's name (ADR 0101). The ring
forgets the oldest records as new ones come and everything is gone at a
restart, so what a service said an hour ago, or before a crash, cannot
be read. Administrators coming from Linux expect `journalctl`, and the
security design (the locked plan's sections 11 and 15) asks for a log
that survives boots, that only administrators read, that the audit log
and notifications can build on (13a.2), and whose records say truthfully
who wrote them.

Who wrote a line was not something the log could tell. The kernel
stamped a program's line with its thread's name, which any program sets
(`set_thread_name`), and Spark's copies of a service's lines were Spark's
lines like any other: a program that printed `volumed: ...` to the
kernel log read exactly like volumed.

## Decision

- **One bus: the kernel log.** Nothing new carries log lines: the
  kernel log already receives the kernel's records, programs' lines
  (through `/devices/kmsg` and the kernel log capability) and, through
  Spark, every service's output. A new service, *logd*, started first
  (order 5), reads each record from `/devices/kmsg` as it comes and
  keeps it.
- **The kernel says who wrote a record.** Each record of the ring now
  carries the number of the process whose program wrote it (0 for the
  kernel's own, saturating at `u32::MAX` so that no number aliases
  another). `/devices/kmsg` shows such a record with the user facility,
  1, as Linux marks lines written to its `/dev/kmsg`, and adds
  `,process=N` after the flags, as Linux's `caller=` field does: Linux
  readers skip fields they do not know. `debug_log` and `/devices/kmsg`
  both take a leading `<N>` as the record's level, so Spark passes on a
  service's `<3>` for an error.
- **Services by Spark's word.** A record of process 1, Spark, whose text
  (after the kernel's `spark[thread]:` stamp) starts with a name and
  `: ` is that service's line; any other program's record is that
  program's, under the name the kernel stamped, whatever its text says.
  Spark puts the service's name in front of each line unless the line
  starts with it, so a service cannot pass its lines off as another
  service's either. The program Spark runs from the kernel command line
  (`--posix`), whose lines it copies as they are, is the boot
  configuration's and named like a service.
- **Format and files.** A record is a line of JSON: `time` (RFC 3339,
  UTC, microseconds, from the boot time and the record's time since
  boot), `boot` (32 random hexadecimal digits that the first logd of
  each boot keeps in `/system/live/log/boot`), `seq` (the record's
  number in that boot's kernel log), `level` (`error`, `warning`,
  `info`, `debug`), `from` (`kernel`, `service`, `program`), `name`,
  `process` (for a program's) and `text`. The records go to one file a
  day, in UTC, `/system/state/log/YYYY-MM-DD.jsonl`, mode 0640: the
  `@state` subvolume on an installed system (ADR 0137), so the log
  follows the system's state and not its snapshots of the root. JSON
  lines are what `jq`, scripts and the web console (Phase 15) read
  without a library of ours; serde and serde_json (MIT OR Apache-2.0)
  write and read them.
- **Durability and restarts.** logd flushes a day file 5 s after the
  first record not on the disk yet, and when stopped (SIGTERM). It
  notes in `/system/live/log/writing` which day file it writes; started
  again in the same boot, it reads that file back from its end for the
  last record of this boot it kept and goes on from the next sequence
  number, so a restart keeps no record twice and loses none the ring
  still holds. Records the ring lost before logd read them are counted
  in a warning of logd's own. A line cut short by a crash is ended
  before the next one, and readers skip it.
- **Reading through logd.** Programs never read the files: `log(1)`
  and `journalctl(1)` send one request, a line of JSON, to the socket
  `/system/live/log/query` and read the matching records, a line of JSON
  each, until logd closes the connection or, for a follower, as they
  come. A request names the boots (this one by default, an offset such
  as `-1`, a number, or all), services or programs, the kernel's
  records alone, a level, a time range, an extended regular expression
  and the last N; logd reads only the day files the times or the boot
  allow. 32 requests are answered at once. When Dukovo has users (13b),
  the socket's peer must be an administrator, which makes the log
  administrators' only, as Chase decided (prep doc, question 3);
  `dmesg` follows the same rule then.
- **The commands.** `log` has short options of its own (`-s` service,
  `-k`, `-p`, `-S`/`-U`, `-g`, `-n`, `-f`, `-b`, `--all`, `-r`, `-o`,
  `--json`, `--list-boots`, `--disk-usage`) and shows this boot by
  default. `-s` (and journalctl's `-u`) matches services' records only;
  a name given as an operand (and journalctl's `-t`) matches a service
  or a program by name, and a program's name is its thread's, which it
  chooses, shown with its process number. `journalctl` is the same tool with journalctl's options and
  defaults (every boot unless `-b` or `-k`, `-u` for a unit, `-t`, the
  `_SYSTEMD_UNIT=` and `_COMM=` matches, `-e`, `-n` alone for ten) for
  scripts and habits from Linux. `dmesg` gains `-k` and `-u` and shows
  the facility. All three are systools tools (ADR 0096).
- **Test.** The boot test `logging` runs the default services and
  `logtest`, which writes lines as a program and checks what `log`,
  `journalctl` and `dmesg` show: levels, names and process numbers, a
  forged service line kept as the program's, the kernel's records
  alone, a follower that sees a new line, the boots and the day files;
  then restarts logd and finds no record twice.

## Consequences

- The services' lines, the kernel's and programs' outlive the ring and
  the boot, and say truthfully who wrote them. The audit log, console
  notifications and retention (13a.2) build on logd, as the web
  console's log view will.
- syslog(3) is not taken yet: it writes datagrams to `/dev/log`, and
  Radix's Unix sockets are streams only. Programs that use it lose
  those lines until datagram sockets come; a program can write to
  `/devices/kmsg` meanwhile.
- The log has no limit yet: trimming by size and age (10 % of the
  filesystem, 4 GiB, 90 days, as the prep doc says) comes with 13a.2.
  `--list-boots` and requests for older boots read every day file,
  which is slow on a large log until an index comes with trimming.
- Once services lose the kernel log capability (13b.4 gives each
  service only what its file declares), their output still reaches the
  log through Spark, and only programs given the capability can write
  records of their own.
- Every record crosses the kernel ring before it is kept, so a burst of
  more than 512 records faster than logd reads loses some; logd says how
  many.
