<!--
SPDX-FileCopyrightText: 2026 The Dukovo contributors
SPDX-License-Identifier: Apache-2.0 OR MIT
-->

# 0066. Terminals are kernel pseudo-terminals with POSIX's line discipline and job control

- Status: Accepted
- Date: 2026-10-22

## Context

A shell needs a terminal: a device that edits a line before the program
reads it, echoes, turns Ctrl-C and Ctrl-Z into signals for the
foreground job, stops background jobs that read it, and hangs up when
it goes. Phase 9 brings Nukesh and `consoled` with virtual terminals,
and Phase 16 a graphical terminal; all of them need the same terminal
underneath. The signals a terminal sends must reach a process group the
moment a key is typed, and the rules that decide who may read and write
span sessions, so the terminal has to be where the process tree is.

## Decision

- The kernel has **pseudo-terminals** (radix-terminal(2)): opening
  `/dev/ptmx` makes one and gives the opener its master side, and
  `/dev/pts/N` is the terminal side until the master goes, which hangs
  it up. Whatever drives a screen or a serial line (`consoled`, a
  terminal app, Spark's copy to the kernel log) holds a master; programs
  hold the terminal side. There is no terminal in the kernel for the
  console itself.
- Between the sides is **POSIX's general terminal interface**, as
  Linux's N_TTY behaves for the flags it supports: canonical lines with
  their editing characters, echo, the signal characters, `IXON` flow
  control, CR and NL translation both ways, `VMIN` and `VTIME`, and
  window sizes with SIGWINCH. Its rules are `radix-tty`'s `Ldisc`, safe
  code tested on the host; the kernel feeds it and copies between its
  rings and programs. Settings are Linux's `struct termios`, and the
  requests are Linux's `ioctl` numbers, so mlibc's Linux headers serve.
- A terminal may be the **controlling terminal** of one session: the
  one whose leader opens it first without `O_NOCTTY` while it has none,
  or asks for it (TIOCSCTTY). It keeps its session and foreground
  process group; a background process that reads it gets SIGTTIN, and
  one that writes with TOSTOP or changes its settings SIGTTOU.
  `/dev/tty` opens the caller's controlling terminal. When the session
  leader ends or the master goes, the foreground group gets SIGHUP and
  SIGCONT, and the terminal belongs to no session.

## Consequences

- The `tty` and `job` C tests drive a shell's job control through a
  pseudo-terminal, and Spark runs the `posix` test runner on one.
- Phase 9's `consoled` is a program holding masters, one per virtual
  terminal, rather than a kernel console driver, so a crash in it
  cannot take the kernel down.
- Packet mode, `TIOCSTI` and the BSD line disciplines are left out;
  they are added only when a program needs them.
