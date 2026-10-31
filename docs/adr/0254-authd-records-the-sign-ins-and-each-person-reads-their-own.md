<!--
SPDX-FileCopyrightText: 2026 The Dukovo contributors
SPDX-License-Identifier: Apache-2.0 OR MIT
-->

# 0254. authd records the sign-ins, and each person reads their own

- Status: Accepted
- Date: 2026-10-31

## Context

Phase 13b.7. The plan's administration command set lists `last` and
`lastlog` with the user tools of Phase 13. On Linux, login, sshd and
systemd write records to three files: `/var/log/wtmp` (each sign-in,
sign-out, start and stop, read by last(1)), `/var/log/btmp` (each
failed sign-in, read by lastb(1)) and `/var/log/lastlog` (each
account's last sign-in, read by lastlog(8)). pam_lastlog then prints
"Last login: ..." and the failed tries since at each sign-in. wtmp and
lastlog are world-readable, so anyone learns when everyone else signed
in and from where; only btmp is root's. Their binary records end in
2038, which is why Debian and SUSE now move to wtmpdb and lastlog2,
SQLite files.

Dukovo has no utmp: who(1) and w(1) read the sessions from
`/processes`. Sign-in goes through authd (ADR 0250), which starts each
session as its own child and sees it end, and checks each password.

Chase accepted, for the logs, that each person reads their own and
administrators everything (ADR 0252), and a person's comings and goings
are theirs as much as their programs' log lines are.

## Decision

- **authd writes the records**, a line of text each, in
  `/system/state/sign-ins` (`accounts::signins`): the time in seconds,
  what happened, and its fields, as `1793440100 in alice pts/0 - 61`.
  It records:
  - the computer's start, with the kernel's release, as it starts
    itself, once per start, and a stop when it is asked to end, which
    Spark does as the computer stops, in `system`;
  - each sign-in and sign-out, with the terminal, where from (none at
    the computer itself, an address once SSH comes, 13c) and the
    session's first process, which pairs them;
  - each wrong password typed at a sign-in.
- **Each account has its own file**, named by its number, root's and
  the account's own group's, mode 0640, with its sign-ins, sign-outs and
  wrong passwords. The directory is 0711, so no one but root lists it.
  A person reads their own file and the computer's starts and stops;
  root reads everything, so an administrator runs `elevate last`. The
  wrong passwords typed for a name that is no account's go in
  `unknown`, root's alone, as Linux's btmp, with the name typed: on
  Linux too it is root who sees a password typed as a name. A number is
  never given again (ADR 0220), so a later account of the same name
  never reads the removed one's file; a line keeps the name the account
  had then, so a rename changes nothing.
- **Bounded.** A file that grew past 512 KiB keeps its newest 4096
  records, and its newest sign-in with its sign-out and the newest
  start if they are older, so that a flood of wrong passwords does not
  lose an account's last sign-in.
- **The commands**, in systools: last(1) and lastb(1) show the records
  as util-linux's do, with its options (`-n`, `-F`, `-x`, `-R`, `-a`,
  `-w`, `-s`, `-t`, `-p`, `--time-format`) and its columns, ends
  (`still logged in`, `down`, `crash`, `gone - no logout`) and
  lengths; lastlog(8) shows each account's last sign-in, as shadow's
  does, with `-u`, `-t` and `-b`, for root and the people's accounts.
  To anyone but root, each prints log(1)'s kind of hint on the
  standard error: `Hint: you see only your own sign-ins. An
  administrator sees everyone's with: elevate last`.
- **At each sign-in**, before the session's programs start, authd
  writes on the terminal the person's last sign-in and the wrong
  passwords typed for them since, as pam_lastlog does:
  `Last sign-in: Sat Oct 31 12:40 2026 on pts/0` and `Since then, 1
  wrong password was typed for bob, the last on Sat Oct 31 12:44 2026
  on pts/1.` Nothing is said at the first sign-in unless wrong ones
  came before it.
- **Words.** The commands say "sign-in", as the rest of Dukovo does:
  last ends with `sign-in records begin ...` where util-linux says
  `wtmp begins ...`, and lastlog says `**Never signed in**`.

## Consequences

- Someone trying passwords at a person's account is seen by that
  person at their next sign-in, not only by an administrator who looks.
- A person's sign-in times are no longer everyone's to read, as they
  are on Linux.
- Stopping authd records a stop, as stopping systemd-update-utmp does
  on Linux; the sessions it started end with it (ADR 0250), so they show
  `down`, as they should. Started again, it records no second start.
- The records are text, so `grep` reads them, and they have no 2038
  limit.
- The `signin` boot case has bob told of his last sign-in and the wrong
  password since, read his own sign-ins with last, lastb and lastlog,
  and not alice's, with the hint; alice reads every wrong password
  through `elevate lastb`, the unknown name's too. The `elevate` case
  has her read everyone's with `elevate last` and `elevate lastlog`.
- **Not yet:** SSH's sign-ins (13c), with the address they came from;
  the graphical sign-in's (Phase 14); a setting to keep the records for
  a time rather than a size.

## Alternatives considered

- **Linux's binary files.** Their layout is glibc's, with 32-bit times
  and fixed field sizes, and no tool here reads them that does not run
  on Dukovo.
- **Everyone reads everything, as on Linux.** It tells every user when
  the others come and go, and from where.
- **authd answers who asks, as logd does (ADR 0252).** It would need a
  longer answer than authd's one line, and root could not read the
  records while authd is not running; the owners and modes of the files
  give the same rule.
- **The audit log alone.** authd's audit lines say the same, but only
  administrators read it, its wording is for people, and it is trimmed
  by logd's rules.
