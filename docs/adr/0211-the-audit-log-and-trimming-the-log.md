<!--
SPDX-FileCopyrightText: 2026 The Dukovo contributors
SPDX-License-Identifier: Apache-2.0 OR MIT
-->

# 0211. The audit log, and trimming the log

- Status: Accepted
- Date: 2026-10-30

## Context

Phase 13a.2. logd keeps every record of the kernel log for good (ADR
0210), with no limit: a busy machine would fill its disk in time. The
plan asks for trimming (`logtrim`) and for an audit log of security
events, and the prep doc fixed the limits with Chase: the log keeps 90
days, at most 4 GiB or 10 % of its file system, whichever is smaller,
and the audit log keeps a year (questions 3 and 4).

An audit log is only worth reading if what it holds is true: a program
must not be able to put an event in it, as a program cannot pass its
lines off as a service's. Users, sign-ins, `elevate` and permission
grants come later in Phase 13; the events there are today are Spark's
(a service started or stopped, a shutdown asked for, the power button),
the kernel's (its log cleared) and logd's own (records lost, the log
trimmed).

## Decision

- **Facilities.** The kernel keeps the syslog(3) facility of each
  record. A program's line takes the facility its `<N>` gives (N >> 3),
  as Linux's `/dev/kmsg` does, or the user facility, 1, without one; it
  cannot take the kernel's, 0. `/devices/kmsg` shows the facility times
  eight plus the level, and `dmesg` shows it (`-x`), filters on it (`-f`)
  and keeps `-k` and `-u` for the kernel's own records and programs'.
- **Security events.** A record of the authpriv facility, 10 (or auth,
  4), is a security event when the kernel or Spark wrote it. A service
  writes one by putting `<86>` in front of a line of its output, which
  Spark passes on (ADR 0210), so it is the service's by Spark's word;
  a program's record of that facility is a claim, kept in the log like
  any other line and not in the audit log. The kernel logs its own with
  `radix_klog::audit`.
- **What is audited now.** Spark's answers to requests that change
  something (start, stop, restart, reload, power off, restart or halt
  the machine, cancel), each with the number of the process that asked,
  which the kernel gives for the control socket's connection
  (`socket_state`); the power button; the kernel log cleared, with the
  process that cleared it; records the ring lost before logd read them;
  and every trim of the log. Sign-ins, `elevate`, accounts and
  permission grants come with 13b and 13d, written the same way.
- **The audit log.** logd keeps each security event in the log, marked
  `"audit": true`, and in the audit log's file of its day,
  `/system/state/log/audit/YYYY-MM-DD.jsonl`, writing the audit log's
  first: a logd that ends between the two keeps the event twice rather
  than not at all. `log --audit` and journalctl's `_TRANSPORT=audit`
  read it, with every other choice of records.
- **Trimming.** `/system/settings/log` (log(5)) sets `keep-days` (90),
  `max-size` (4G), `max-percent` (10) and `audit-keep-days` (365); 0
  lifts a limit, so a user who wants the log kept for good can have it.
  logd removes whole day files: first those past their days, then the
  oldest of the log's, then of the audit log's, while the two take more
  than the smaller of the size limits. It never removes today's file or
  one it is writing. It trims when it starts, when a day's file begins,
  after each 64 MiB it writes, when its settings are read again
  (`service logd reload`), and when asked.
- **Asking for a trim.** `logtrim` (with `--size`, `--days` and
  `--dry-run`) and journalctl's `--vacuum-size` and `--vacuum-time` send
  a trim request to logd's socket, which logd's writer carries out
  between two records; only the writer removes files. Limits given that
  way apply to the log, never to the audit log, which keeps to its own
  setting. Every removal is a security event naming the files and, for
  a request, the process that asked.

## Consequences

- The log stays within its limits without anyone looking after it, and
  the security events of the machine are kept for a year in files of
  their own, which no program can write to through the log.
- The audit log is as safe as the files under `/system/state/log`: an
  administrator can delete them (their trims are audited, a hand's
  deletion is not). A chain of hashes or a copy kept elsewhere can
  follow if Dukovo needs evidence that survives its own administrator.
- A day's file that alone outgrows the limits is not cut, since logd is
  writing it; trimming waits for the next day. A burst of lines that
  large is a fault to rate-limit at its source.
- Older boots and `--list-boots` still read every day file; an index of
  boots can come if large logs make that slow.
- Services keep writing security events through Spark once they lose
  the kernel log capability (13b.4), since the facility rides on their
  output.
