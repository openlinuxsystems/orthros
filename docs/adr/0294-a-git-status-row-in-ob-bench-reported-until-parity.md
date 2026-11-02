<!--
SPDX-FileCopyrightText: 2026 The Dukovo contributors
SPDX-License-Identifier: Apache-2.0 OR MIT
-->

# 0294. A git status row in ob bench, reported until parity

- Status: Accepted
- Date: 2026-11-01

## Context

Step 5 of the git support plan asks for a benchmark of git itself, so
that git on Dukovo can be compared with git on Linux. The command people
run most, and wait on in every shell prompt that shows a repository's
state (ADR 0288), is `git status`. It spends its time on `lstat` of
every tracked file, reading the index and directories, and a little
hashing, so it measures the file system calls a developer's day is made
of rather than one call in a loop.

Every row of `ob bench` must be at most 1.00 times Linux in the same
virtual machine (ADR 0107). A new row of a whole program, never tuned
for Radix, is likely to start over that line, and a gate that fails on
day one blocks every pull request until it is fixed.

## Decision

**`ob bench` gets a `posix_git_status` row.** `bench.c --git <git>` makes
a repository of 1,000 committed files in 10 folders, warms the caches
with one `git status`, then times `git status --porcelain` with nothing
changed, 50 times, per run. git runs with no system or user settings
(`GIT_CONFIG_NOSYSTEM`, `GIT_CONFIG_GLOBAL=/dev/null`) and a fixed
identity and date. Radix runs `/bin/git`; Linux runs the
same git release, built static on glibc by `ob bench` from the port's
source and put in the initramfs at `/git`, which linux-bench passes on.

**The row is reported only until Radix reaches parity** (Chase,
2026-11-01). A row has a `gated` flag; this one starts with it off, so
its ratio shows in the report, marked "not gated yet", and never fails
the gate or calls for more runs. The first time CI shows it at or under
1.00, the flag turns on, in its own pull request, and stays on: the row
is then held to the gate like every other.

## Consequences

- The git row's ratio is visible on every pull request from now on,
  and the work to bring it to parity is tracked like the gate's other
  rows (the nightly bench of ADR 0290 records it on main).
- Linux's git is built on the CI runner with its clang, once per git
  release, so the first `ob bench` after a git upgrade takes about 20
  seconds longer.
- No other row may be ungated this way without a decision of its own.
