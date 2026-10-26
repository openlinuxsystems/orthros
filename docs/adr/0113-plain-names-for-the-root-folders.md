<!--
SPDX-FileCopyrightText: 2026 The Dukovo contributors
SPDX-License-Identifier: Apache-2.0 OR MIT
-->

# 0113. The root's folders have plain names, and Unix's names are hidden links to them

- Status: Accepted
- Date: 2026-10-26

## Context

Dukovo is meant to explain itself: anyone looking at the system should
be able to tell what each thing is without being taught (the plan's
design principles). Its own folders already have plain
names, `/system`, `/applications`, `/users` and `/volumes`, but the rest
of the root kept Unix's: `/dev`, `/proc` and `/sys` are abbreviations,
`/run` says nothing about what is in it, and `/system/etc` and
`/system/var` are only clear to those who know Unix. `/data` read as
the user's own files, though it is the place users and services share.

At the same time, ported programs and scripts look for the Unix names:
`/dev/null`, `/proc/mounts`, `/etc/hosts`, `/var/log`, `/run/*.pid`,
and those who know Unix type them. ADR 0081 already met the same need for
`/bin`, `/usr`, `/etc` and `/var` with links into `/system`.

The names are cheap to change now, while every program that uses them
is in this repository; once programs ship in bundles (Phase 13) and
disks hold the tree (Phase 10), every change costs a migration.

## Decision

- **The folders are renamed:**

  | Was | Is | What it holds |
  | --- | --- | --- |
  | `/dev` | `/devices` | Device files |
  | `/proc` | `/processes` | The processes, mounts, memory and uptime, as text |
  | `/sys` | `/kernel` | The kernel's view of the hardware and its settings |
  | `/run` | `/system/live` | The running services' state: sockets, locks, process IDs |
  | `/system/etc` | `/system/settings` | The system's settings |
  | `/system/var` | `/system/state` | The system's changing data: logs, caches |
  | `/data` | `/shared` | Files users and services share |

  The kernel's own files at the top of `/sys/kernel` move to the top of
  `/kernel` (`/kernel/version`, `/kernel/cmdline`, `/kernel/cpus`), as
  `/kernel/kernel/version` would say the same word twice. Linux has no
  such files, so nothing ported reads them there.
- **Unix's names stay, as links** that Spark makes at boot, beside
  ADR 0081's: `/dev -> devices`, `/proc -> processes`, `/sys -> kernel`,
  `/run -> system/live`; `/etc` and `/var` now lead to
  `system/settings` and `system/state`; and `/system/state/run ->
  ../live`, so that `/var/run` is `/run`, as on Linux. `/system/etc`,
  `/system/var` and `/data` were Dukovo's own names and get no links.
- **Listings leave the links out.** Spark writes a `.hidden` file in
  each directory that holds links, one name a line, the convention GNOME
  Files and Nemo already read. ls(1) (a Dukovo patch to uutils) and the
  shell's completion leave those names out unless asked for every name
  (`ls -a`, `ls -A`), or, in completion, until the whole name is typed;
  the file manager will do the same. So `ls /` shows `applications`,
  `devices`, `kernel`, `processes`, `shared`, `system`, `tmp`, `users`
  and `volumes`. A path through a link works everywhere, and a program
  that lists a directory itself sees the links, as on any Unix.
- **What Dukovo reports uses the new names.**
  - The C library names terminals `/devices/pts/N` (ttyname, ptsname,
    and `/devices/tty` from ctermid), gives `/devices/fd/N` for fexecve,
    and accepts `/devices/fd/N` and `/dev/fd/N` alike (ADR 0084).
    `paths.h` names the new folders (`_PATH_DEV` is `/devices/`), so a
    program that takes `_PATH_DEV` off a terminal's name still gets
    `pts/0`.
  - Dukovo's own programs, the kernel's log, the manual pages and the
    handbook use the new names. The native tools take either prefix off
    a terminal's name. The shell's process substitution gives
    `/devices/fd/N`.
  - Filesystem kinds keep Linux's names (`proc`, `sys`, `dev`, `tmpfs`
    in `/processes/mounts`), as `find -fstype` and df(1) expect them.
- **The Linux compatibility layer** (release 1.3) shows the Linux
  layout inside its own view, so Linux programs see `/dev`, `/proc` and
  `/sys` as directories, not links.

## Consequences

- `ls /` lists nine folders, each named for what it holds.
- Unix programs and scripts keep working through the links: opening
  `/dev/null` or reading `/proc/mounts` costs one more step in the
  kernel's walk of the path, and nothing after.
- `find /proc` without `-H` or `-L`, and `ls -l /dev`, show the link,
  not the folder, as `/bin` already did; `find /processes` is the way to
  walk it.
- `getcwd` after `cd /dev` says `/devices`: the C library keeps the
  working directory with its links followed (ADR 0081).
- The folders inside `/system` (`bin`, `lib`, `share`) are renamed
  separately, with the program paths that go with them.
