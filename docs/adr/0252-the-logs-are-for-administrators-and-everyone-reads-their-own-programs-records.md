<!--
SPDX-FileCopyrightText: 2026 The Dukovo contributors
SPDX-License-Identifier: Apache-2.0 OR MIT
-->

# 0252. The logs are for administrators, and everyone reads their own programs' records

- Status: Accepted
- Date: 2026-10-31

## Context

Phase 13b.6. Until now every program could read the whole log: the
kernel's ring through `/devices/kmsg`, mode 0666 by default, and logd's
records through its socket, which answered anyone the same (ADRs 0210
and 0211). With sign-in (ADR 0250) more than one person uses a machine,
and the log says who signed in when, which commands each administrator
ran with elevate, and what the services and the hardware did. The Phase
13 prep doc's question 3, which Chase accepted on 2026-10-30, settles
it: the logs and `dmesg` are for administrators, through elevate, and
everyone reads their own programs' logs. That is how Ubuntu does it:
only the `adm` group reads the whole journal, a user's `journalctl`
shows their own, and `dmesg` needs root (`kernel.dmesg_restrict`).

A record of the kernel log carried the process that wrote a program's
line, but not the user it acted for, and a process's number alone does
not say whose it was once the process is gone.

## Decision

- **The kernel marks a program's record with its user.** Each record a
  program writes, through `/devices/kmsg` or the kernel log capability,
  carries the user the writing process acts for, as it carries the
  process. `/devices/kmsg` gives both after the flags:
  `priority,sequence,microseconds,-,process=N,user=N;text`. The kernel
  sets them, so a program cannot pass its line off as another user's.
- **`/devices/kmsg` is root's, mode 0622**: root reads it, and anyone
  writes a line to it, as Linux's with `dmesg_restrict`. Clearing the
  log (`dmesg -C`) needs the read right as well as the write right, so
  a writer cannot clear it. `dmesg` tells anyone else that an
  administrator reads it with `elevate dmesg`.
- **logd answers each user with what they may read.** It names the user
  at the other end of its socket as the kernel gives it (`socket_state`,
  SO_PEERCRED), never as the request says. Root reads every record.
  Anyone else reads only the records of programs that acted for them,
  as logd keeps the user with each record (`"user":1000`): not the
  kernel's, not the services', not another user's, and not a record of
  before this change, which has no user. Only root reads the audit log,
  asks how much the log takes or trims it. The socket is mode 0666, so
  everyone may ask.
- **log and journalctl say so**: run by anyone but root, they print a
  hint on the standard error, as journalctl does for a user outside its
  groups, unless `-q`.
- **logger(1)**, util-linux's command for scripts, writes a line to the
  log. It names its thread after the tag, the user's name by default,
  since the kernel puts the thread's name in front of the record, and
  writes the line with its `<N>` to `/devices/kmsg`.

## Consequences

- An administrator types `elevate log`, `elevate journalctl` or
  `elevate dmesg` for the whole log, as `sudo journalctl` on Linux.
  Root's console shell, without sign-in, reads everything as before.
- A user's own scripts and programs can log and read their lines back
  with `log NAME` without elevate.
- Records kept before this change have no user, so only root reads
  them.
- The services all run as root today, so their lines are root's. When a
  service runs as a user of its own, its lines stay services' lines,
  shown to root only.
- The `logging` boot case runs as root and reads the new field; the
  `signin` case has bob write a line with `logger`, read it back with
  `log`, find no line of authd's or the kernel's among his, be refused
  by `dmesg`, `log --audit` and `log --disk-usage`, and alice read the
  kernel log with `elevate dmesg`.
- **Not yet:** a user's services and their own lines in their sessions'
  log (13b.7 and later), and access lists that let a group read the
  whole log, as Ubuntu's `adm` (13b.7's ACLs).

## Alternatives considered

- **An `adm` group of log readers.** Dukovo's administrators already
  act as root through elevate, and elevate keeps an audit line of each
  use; a second way in would skip it. ACLs (13b.7) can still give a
  group the log later.
- **Filtering in log(1).** log runs as the user, so anything it filters
  the user could read anyway; logd, which reads the files, has to.
- **Checking the user by process number** when a record is read: the
  process may be gone, or its number used again, by then.
- **Keeping `/devices/kmsg` readable by all**, as Linux's default
  without `dmesg_restrict`. The kernel's records name devices, drivers
  and addresses, and the programs' lines would reach every user,
  against the answer to question 3.
