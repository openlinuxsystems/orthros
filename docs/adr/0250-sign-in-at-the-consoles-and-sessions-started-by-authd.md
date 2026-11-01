<!--
SPDX-FileCopyrightText: 2026 The Dukovo contributors
SPDX-License-Identifier: Apache-2.0 OR MIT
-->

# 0250. Sign-in at the consoles, and sessions started by authd

- Status: Accepted
- Date: 2026-10-31

## Context

Phase 13b.5b. Since ADR 0218 authd keeps the accounts and checks
passwords, and since ADR 0214 the kernel checks every file against the
name a process acts for. But each console still runs root's shell with
no password asked, and nothing starts a process that acts for anyone
else: only the holder of the names capability can, and Spark gives it
to no service.

On Linux, agetty opens the console and runs login(1) as root, which
checks the password through PAM and then, still root, calls
setgroups, setgid and setuid before it runs the shell. Radix has no
set-user-ID calls that lower a process's own name: a name is given by
the names capability, to a process that has not started yet
(radix-names(2)). Whoever starts the session must hold that capability,
and it should be the one program that already knows the passwords.

The security design asks for sign-in with a name and a password only,
no rules forced on the password (the project's principles), never an
age or an identity (ADR 0126), and no built-in account or known
password, ever. A machine made without `ob image --user` has no
account, and its owner must make one at the first boot.

## Decision

- **login(1)** (systools) is the sign-in. consoled runs it as root on
  each console's pseudo-terminal, in a session of its own, after the
  issue text. It asks for `Name:` and `Password:` (unseen) and sends
  both to authd with the terminal itself, as a descriptor
  (`SCM_RIGHTS`), in a new `session` request. A refusal is shown as
  authd words it, the same for a wrong name or password, after authd's
  wait, and the name is asked again.
- **authd starts the session.** Its service file asks for `names =
  true`, a new permission (`service.toml(5)`) through which Spark hands
  it a copy of the names capability, as the descriptor after its
  mounts, named by `NAMES_DESCRIPTOR`; no other shipped service asks
  for it. For a right password authd:
  - stamps a copy of its root for the user, their own group and the
    groups of `/system/settings/group` that list them, and derives one
    without the bypass right or the control right from that: the
    session's root;
  - opens `/system/commands/login` through that root, so the program is
    one the user may run;
  - makes a process, names it for the user and groups, and gives it that
    root, the random source, the log and the terminal as descriptors 0
    to 2: nothing else of authd's, not its root, not the names
    capability, not the bypass right, not the control right;
  - runs `login --session` in it, with Linux login's environment (`HOME`,
    `SHELL`, `USER`, `LOGNAME`, `PATH`, `TERM` from the console, sanitised,
    `LANG`, `EDITOR`).

  The process is authd's child, in authd's session, so `login
  --session` can call `setsid`, make the terminal its controlling
  terminal (`TIOCSCTTY`) and run the user's shell as a login shell in
  their home.
- **The session's end.** authd answers `ok PROCESS` and keeps the
  connection; when the session's first process ends it writes `ended
  STATUS` and closes it. login, which gave up its own descriptors of the
  terminal once the session began, then exits, and consoled clears the
  console and runs login again. So the terminal hangs up when the
  session's programs are done with it, and no session outlives its
  console's sign-in unnoticed. Each sign-in and sign-out is an audit
  event: `alice signed in on pts/0: session process 63, for process 58
  (user 0)` and `alice signed out on pts/0: session process 63 ended
  with status 0x0`.
- **The first boot.** While no person's account exists (numbers from
  1000) and root's is locked, which is every machine made without
  `--user`, authd answers `needs-owner` with `yes`, and login makes the
  owner's account first: a name, a full name that may be empty, and a
  password twice, warned about but never refused if it is empty or
  short. authd's new `owner` request makes it, an administrator's, only
  from root and only while that holds, so two consoles cannot both make
  one. login then signs the owner in.
- **Each console signs in on its own**, so two people, or one twice, can
  be signed in on two consoles side by side. who(1), w(1) and users(1)
  now name each session's own user.
- **Root's shell without sign-in** stays for those who can already run
  anything as root: consoled's `--shell` option, and the kernel flag
  `console-sign-in=off` (dukovo-boot(7)), which anyone who can edit the
  kernel's command line at the boot menu could replace with
  `init=` anyway, as with Linux's `init=/bin/sh` or systemd's
  `systemd.debug_shell`. The boot tests that type at root's shell set
  it; a new `signin` case tests the sign-in.

## Consequences

- No console gives a shell without a password, or without its owner
  making one, unless the kernel's command line says so.
- authd is the only service that can act for another user, and the code
  that does is about a hundred lines (`signin.rs`); a flaw in authd is a
  flaw of the whole machine's, as PAM's and login's are on Linux.
- A session's processes are charged to authd's resource domain, as the
  shells were to consoled's, so authd's service file sets no memory
  limit and twice the default CPU weight, as consoled's did. A process
  made with `create` ends with its maker, so a restart of authd ends
  every session, as one of consoled's did before; a domain of its own
  for each session, and sessions that outlive authd, can come with the
  graphical session in Phase 14.
- A signed-in session is not consoled's child, so consoled's platform
  capability (`admin.hardware`) does not reach it: `ectool` in a
  person's session waits for elevate (13b.6).
- A person's session holds no control right: with it they could mount
  over, or unmount, any folder they may walk to, `/tmp` or
  `/system/settings`, for everyone, as the walk checks only that they
  may pass. So they mount and unmount nothing, as on Linux without
  udisks: `eject` of a USB stick is root's, through elevate from 13b.6,
  until volumed takes people's requests (Phase 11).
- The `signin` boot test, on UEFI, makes the owner's account at tty1,
  signs alice in, has root make bob, signs bob in on tty2, checks each
  session's identity, terminal and home, `who`, that neither reads the
  other's home, that bob mounts and unmounts nothing, signing out and
  in again and a wrong password, and that root sees both sessions as
  leaders on different terminals.
- **Not yet:** elevate and administrators acting as root (13b.6);
  `last` (ADR 0254), a private `/tmp` for each person (ADR 0255), and
  the account settings (13b.7);
  graphical sign-in (Phase 14); remote sign-in over SSH (13c), which
  will ask authd for its sessions the same way.

## Alternatives considered

- **login with the names capability, as Linux's login is root.** Every
  console's login would hold the power to act for anyone while it reads
  untrusted input from the keyboard; authd already must be trusted with
  the hashes, so one program holds both.
- **Lowering a process's own name** (`setuid`). Radix names a process
  before it starts, so that nothing runs with a name it was not given;
  adding a lowering call would add a second way to change names to
  audit.
- **A test password in the test images.** Shipping any known password is
  against the security design, and test boots keep `/system/state` in
  memory; the kernel flag keeps the old tests at root's shell instead.
