<!--
SPDX-FileCopyrightText: 2026 The Dukovo contributors
SPDX-License-Identifier: Apache-2.0 OR MIT
-->

# 0103. Nukesh is checked against POSIX's shell tests and real projects' scripts, run in a sandbox

- Status: Accepted
- Date: 2026-10-24

## Context

ADR 0083 checks Nukesh with scripts of our own, `tests/cases`, each
recorded from bash or dash. Those check what their writers thought of.
Phase 9's exit also asks that `/bin/sh` pass a POSIX shell conformance
suite, and that a corpus of real-world scripts (install scripts,
container entrypoints, the scripts of real projects) give the same
results under Nukesh as under bash or dash.

Such scripts are written to change the machine they run on: they write
to the home directory, call `sudo` and `curl`, and remove what they
made. Their test suites start servers and send signals. Run on a
developer's machine or a CI runner as they are, they could change it,
reach the network, or leave processes behind.

## Decision

- `ob test corpus` runs the corpus that
  `userspace/programs/nukesh/tests/corpus.toml` lists. Each source is a
  git repository pinned by its full commit, with the SPDX license of the
  files run, the directories fetched, and how it runs: smoosh's tests,
  a test tree in urchin's layout (nvm's), scripts with their arguments
  and environment, or a tool other sources' tests run, such as bats.
  Plain `ob test` leaves it out, as it does the Kani proofs, and CI runs
  it in a job of its own.
- The POSIX suite is the tests of smoosh, the executable POSIX shell
  semantics by Michael Greenberg and others (MIT). Nukesh, as `sh`, must
  give each test's expected output, error and status. dash and
  `bash --posix` run them too, and their counts are printed for the
  record.
- Every other source runs under the shell it is written for, dash for
  POSIX `sh` and bash for bash, and again under Nukesh in that shell's
  mode: each test must give the same exit status and standard output.
  Standard error is not compared, as messages are worded differently,
  but its end is shown when a test differs. Tests whose output is TAP
  (bats) count each of their lines as a test.
- The corpus runs in a sandbox. A user namespace (or root) with mount,
  PID and network namespaces of its own copies the corpus into a new
  tmpfs on `/tmp`, puts fresh ones on `/var/tmp` and `/dev/shm`, and
  remounts everything else read-only, or runs nothing if one mount
  stays writable. There is no network, and `curl`,
  `wget` and `sudo` are stubs that fail as they would offline. A source
  can name more stubs, which print their name and arguments: the servers
  an entrypoint starts, so that none starts for real, whether the host
  has it or not. Each test
  runs as `nobody` of a user namespace of its own, in a PID namespace of
  its own whose processes all end with it, with a time limit, and the
  corpus as a whole may have at most 4,096 processes. A script that
  tries to install something gets as far as it would on a machine
  without a network.
- A test whose result is expected to differ is listed with the reason,
  in its source's `known` table. A difference is a bug in Nukesh unless
  the reason is one of these: dash and bash give the same result as
  Nukesh and the test expects another; only the wording of a message
  differs; or POSIX leaves the choice open and nukesh(1) says which one
  Nukesh makes. A listed test that gives the expected result is reported
  too, so that its entry is removed.
- A smoosh test that races the machine's load, and loses under bash
  too, is listed in its source's `timing` table, with the reason, and
  has three tries.

What the corpus found changes these rules of ADR 0083:

- `bash` is a name of Nukesh, as `sh` is: `/system/bin/bash` links to
  it, so that scripts whose first line is `#!/bin/bash` or
  `#!/usr/bin/env bash`, most installers and entrypoints among them, run
  unchanged. Outside POSIX mode Nukesh sets `BASH_VERSION` and
  `BASH_VERSINFO` to bash 5.2's, as scripts check them before they use a
  feature of bash, such as `declare -A`, or refuse to run without bash, as
  Homebrew's installer does. `NUKESH_VERSION` tells the shells apart. A
  real bash can still come as an application. In POSIX mode, which takes
  no `[[`, none of bash's own variables (`BASH_SUBSHELL`, `BASHPID` and
  the other names that begin with `BASH`) are set, as in dash: scripts
  take them for a sign that bash's features are there.
- Errors are bash's outside POSIX mode. An error in arithmetic, a bad
  substitution, an assignment to a read-only variable, an indirect
  expansion that names no variable, or failglob's no match abandons the
  command it is in, and the shell goes on with the next command it
  reads, with status 1. `((`, `let`, `for ((` and `[[`'s comparisons of
  numbers only fail. In POSIX mode these errors, and a special builtin's,
  end the shell with status 2, as dash's do, where ADR 0083 had status 1.
- In POSIX mode, `${!...}` is `$!` with an operator, as in dash.

## Consequences

- Nukesh is checked against code that others wrote, for other shells:
  smoosh's 186 tests, and the scripts and test suites of 20 real
  projects, 952 tests in all. Each bug the corpus finds is fixed in Nukesh, with a case in
  `tests/cases` that records the fix from bash or dash. It finds slowness
  too: a script was parsed again from the start of its command for each
  line read, so neofetch, with a function of 6,000 lines, took 34 seconds
  to read. A file is now read ahead in batches that double, and what was
  read past the command is given back before it runs.
- The corpus needs dash, bash, util-linux's `unshare`, `prlimit` and
  `setpriv`, a C compiler for smoosh's helpers, git and the network to
  fetch the sources, and user namespaces. Ubuntu 23.10 and later allow
  those only with `sysctl kernel.apparmor_restrict_unprivileged_userns=0`,
  which CI sets. `ob doctor` names what is missing.
- It takes about seven minutes on 4 CPUs, most of them in nvm's tests.
- A script that depended on status 1 for these errors as `sh` sees 2,
  as it would under dash.
- A script that tells bash by `BASH_VERSION` takes Nukesh for bash 5.2,
  and one that needs what Nukesh lacks of bash finds out at that point;
  the corpus is where such scripts are found first.
- A source is added by pinning its commit and naming its license; the
  corpus file is checked when it is loaded, and CREDITS.md names every
  source.
