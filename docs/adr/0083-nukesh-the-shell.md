<!--
SPDX-FileCopyrightText: 2026 The Dukovo contributors
SPDX-License-Identifier: Apache-2.0 OR MIT
-->

# 0083. Nukesh is the shell and /bin/sh, written in Rust and checked against bash

- Status: Accepted; its rules for errors in POSIX mode and for `BASH_VERSION` are changed by [0103](0103-the-shell-corpus.md)
- Date: 2026-10-23

## Context

Phase 9 boots to a shell prompt. The system needs one shell for people
to type at, and a `/bin/sh` that runs the system's scripts and the
commands of `system`, `popen` and `wordexp`. The release is server
first (ADR 0078), so the shell has to run real scripts without changes.
Most of those scripts are written for bash, and the rest for POSIX
`sh`.

Porting bash would bring GPL-3.0 code into the most used program of the
base system, and one that parses untrusted input, often as root, in C.
dash covers only POSIX `sh`. The plan names Dukovo's own shell, Nukesh.

## Decision

Nukesh is a shell written in Rust for Dukovo, in
`userspace/programs/nukesh`. It is one program with three names:

- `nukesh` runs the POSIX shell language plus the parts of bash that
  scripts use: arrays, `[[ ]]`, `(( ))`, the parameter expansions,
  process substitution, `local`, `declare`, `mapfile`, `shopt`, the
  `ERR`, `DEBUG` and `RETURN` traps, programmable completion, and the
  rest that nukesh(1) lists.
- `sh` (`/bin/sh`, a link) starts in POSIX mode, as bash's `--posix`
  does. `echo` then takes escapes and no options (`xpg_echo`), as
  dash's does.
- `rnukesh` is restricted, as `rbash`.

Its behavior is bash 5.2's, and dash's for POSIX mode. The rule is
checked, not assumed. A corpus of scripts in `tests/cases` keeps each
script's output and status, recorded from bash or dash. The host tests
run the corpus, and the `posix` boot test runs it again on Radix. A
difference from bash is a bug, unless nukesh(1) names it. Two are named:

- An error that ends a non-interactive shell (a bad expansion, an
  assignment to a read-only variable, a special builtin's error) gives
  status 1, as bash does, also in POSIX mode, where dash gives 2.
- `BASH_VERSION` is not set, so a script can tell the shells apart.
  `NUKESH_VERSION` is set.

At a terminal it reads commands with reedline (MIT), but only when
standard input and standard error are both a terminal and `TERM` is
set to something other than `dumb`. Anything else gets a plain reader.
The plain reader reads one byte at a time, so a script read from a pipe
is never read past the command being run. reedline's terminal library,
crossterm, wakes its event loop from signal handlers through a socket
pair (ADR 0085). crossterm joins the crates patched for Radix (ADR
0080). mio waits with poll(2) on Radix, and once mio has reported an
event it stops watching for it until the event source registers again.
crossterm reads the terminal straight from its file descriptor, so the
patch registers the terminal again each time mio reports it.

Functions pass through the environment in bash's form,
`BASH_FUNC_name%%=() { ... }`, so that they pass between bash and
Nukesh. A value is taken only if it is exactly one function definition,
the lesson of Shellshock. POSIX mode takes none. Start-up files are not
read when the real and effective users differ.

The history file is created readable only by its user. Each shell adds
only its own commands, under a lock. The file is trimmed by writing a
new file and renaming it into place.

Process substitution passes a pipe by its name in `/dev/fd` (ADR 0084).

## Consequences

- Bash scripts run unchanged, and `/bin/sh` contains no GPL code. Its
  dependencies (reedline, crossterm, indexmap, libc and theirs) are all
  permissive, and the license manifest lists them.
- Dukovo maintains a shell of about 20,000 lines. The corpus grows with
  each bug found, and each case is checked against bash before it is
  added.
- Bugs the corpus finds are fixed in Nukesh, never in the expected
  output. The cases use only the programs Radix has; those that need
  `diff`, `grep` or `sed` come with their ports later in Phase 9.
- A script that depends on dash's status 2 for these errors sees 1.
  Any non-zero status is what POSIX asks for.
