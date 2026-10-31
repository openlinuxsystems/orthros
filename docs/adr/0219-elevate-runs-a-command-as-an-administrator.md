<!--
SPDX-FileCopyrightText: 2026 The Dukovo contributors
SPDX-License-Identifier: Apache-2.0 OR MIT
-->

# 0219. elevate runs a command as an administrator, through authd

- Status: Accepted
- Date: 2026-10-31

## Context

Phase 13b.6. Since ADR 0250 people sign in at the consoles, each
session acting for its own user without the bypass right, so the
owner's account, an administrator's, can no longer make accounts, read
another user's files, or set the fans with ectool: only root can, and
root's shell is there only with sign-in turned off. The security design
says an admin is a user in the admins group, "which only means they
may elevate"; that `elevate` runs one command, or one marked shell
pane, with the admin set after the password; that inside it the
process reports number 0, so scripts that check `id -u` behave as with
sudo; that the unlocked minutes belong to one terminal pane, never to
the user or an app; that it runs with a fixed PATH of
`/system/commands` and `/applications/commands` and a reset
environment, as sudo's `secure_path` does; and that every use goes into
the audit log. Chase chose 15 minutes for the unlock (the Phase 13
prep doc's question 6), sudo's per-terminal default on Ubuntu. There is
no setuid program, and Radix has no call that raises a running
process's name: a name is given by the names capability, before a
process starts (ADR 0214).

## Decision

- **authd starts elevated commands.** elevate(8), and sudo(8), the same
  program by sudo's name, ask authd over its socket with a new
  `elevate` request: the mode (a command, root's shell, or root's login
  shell), the password if one was typed, `TERM`, the directory, the
  language settings and the command. authd:
  - refuses an asker who is not an administrator, by the user the
    connection gives, before any password, and audits the refusal;
  - finds the command only in `/system/commands` and
    `/applications/commands`, or as given if it holds a `/`, and
    refuses one not found, before any password, as sudo does;
  - answers `ok password` when the asker's pane is locked and no
    password came, and checks the one that comes, waiting after wrong
    ones as for sign-in;
  - makes a process, its own child, which acts for root as authd does,
    and gives it authd's root, with the bypass right, the random
    source, the log, the platform capability (authd's service file now
    asks for `admin.hardware`), and the three descriptors elevate sent
    as its 0 to 2: the admin set, as root's shell has it;
  - runs `elevate --started` in it, which takes a new session, makes
    the first terminal among its descriptors its controlling terminal,
    goes to the directory and runs the command.
- **The environment** is sudo's with `env_reset` and `secure_path` on
  Ubuntu: `HOME`, `SHELL`, `USER` and `LOGNAME` from root's account,
  `PATH=/system/commands:/applications/commands`, `TERM`, the language
  settings whose values hold nothing but letters, digits and `._-@:`,
  and sudo's own `SUDO_USER`, `SUDO_UID`, `SUDO_GID` and `SUDO_COMMAND`,
  which scripts read. Nothing else passes; `-E` is refused with how to
  use env(1) instead.
- **A pane** is the terminal of the asker's session, with that session
  and when its leader started, as /processes shows them: sudo's
  `timestamp_type=tty`. After its password, a pane is unlocked for 15
  minutes from its last elevated command; another pane, another
  session on the same terminal, and a program with no terminal are
  asked again. `elevate -k` locks it; `elevate -v` unlocks it and runs
  nothing. authd keeps the unlocked panes in memory only.
- **A terminal of the command's own.** A descriptor of elevate's that
  is a terminal is not passed on: elevate makes a pseudo-terminal with
  the same settings and size, passes its other end, and copies between
  them with its own terminal raw, as sudo's `use_pty` does. So the
  command and what it starts are its terminal's foreground: Ctrl+C and
  Ctrl+Z reach them, a shell has job control, and nothing of the
  command's can reach the asker's terminal but through elevate. A
  descriptor that is not a terminal is passed as it is, so pipes and
  redirections work.
- **Signals and stops.** elevate sends authd a pipe's read end with
  the request and writes a line to it for each SIGINT, SIGQUIT,
  SIGTERM, SIGHUP and SIGTSTP it gets; authd sends that signal to the
  command's process group. authd waits for its children with
  `WUNTRACED`: when the command stops it writes `stopped SIGNAL`, and
  elevate stops itself, so the shell that ran it has the terminal back,
  and writes `continue` when `fg` wakes it. When the command ends authd
  writes `ended STATUS`, as for a session, and elevate exits with the
  command's status. If elevate goes first, the pipe or the connection
  ends, and authd hangs the command up with SIGHUP.
- **The password** is asked by elevate on `/devices/tty`, unseen, as
  `[elevate] password for alice:`, or `[sudo]`; three wrong ones end
  it, `-n` refuses to ask and `-S` reads a line of the standard input,
  as sudo's options do. The consoles' own prompt, drawn by consoled
  where no program can draw or read it, comes in 13b.6b.
- **Audit events**: `alice elevated on pts/0 (password typed): process
  81 runs /system/commands/id, for process 77`, `(unlocked)` within the
  15 minutes, `alice's elevated process 81 on pts/0 ended with status
  0x0`, `alice unlocked elevate on pts/0 for 15 minutes`, and `bob, not
  an administrator, asked to elevate on pts/1: dmesg, for process 93`.
  A command line shows as at most 256 characters, with `?` for each
  control character.
- **As root**, which an elevated command and root's shell are, elevate
  runs the command itself with the same path and environment.

## Consequences

- An administrator does what root does through elevate: `elevate user
  add bob`, `sudo passwd bob`, `elevate ectool ...`, `elevate -s` for a
  run of work. A user who is not one cannot, and the attempt is logged.
- The owner's account administers the machine, so root's shell without
  sign-in is needed only to get back in.
- authd holds one more capability, the platform one, and the code that
  runs as root for someone else is about a hundred lines more
  (`elevate::start` and the requests); a flaw there is a flaw of the
  whole machine's, as sudo's is on Linux.
- A program running in an unlocked pane can elevate within its 15
  minutes, as with sudo; `-k` ends them.
- An elevated command is authd's child, in authd's resource domain, and
  ends if authd does.
- The new `elevate` boot case signs alice and bob in, shows bob refused,
  alice's password asked once, root's environment and the fixed path,
  what only root reads, Ctrl+C, an elevated shell on a terminal of its
  own, `-k`, `-n` and a wrong password asked again.
- **Not yet:** the consoles' trusted prompt and Ctrl+Alt+Delete as the
  key no program receives (13b.6b); the logs readable only by
  administrators (13b.6c); resetting an administrator's password from a
  startup-trouble boot entry (13b.6d); the admin set split into named
  areas for delegation (later; in 1.0 an administrator gets them all);
  elevate over SSH (13c) and in Settings and the web console (Phase 14).

## Alternatives considered

- **elevate makes the process and authd names it.** elevate would hold
  a capability to its own child, which, named root, would hold the
  bypass right; its maker could read its capabilities and trace it.
  authd making it keeps everything of root's out of the asker's hands.
- **The asker's terminal as the command's own.** The command would not
  be its terminal's foreground, so Ctrl+C would stop elevate and not
  it, and an elevated shell would have no job control. sudo moved to
  `use_pty` by default in 1.9.14 for this and because a command sharing
  the terminal can push input to it.
- **A sudoers file.** Every administrator may run any command, as the
  security design says the admin set amounts to the whole machine;
  delegation by named areas is a later step, not a rule language.
- **Remembering the unlock per user, sudo's `timestamp_type=global`.**
  Any program the user runs anywhere could ride it; the design says
  the minutes belong to one pane.
