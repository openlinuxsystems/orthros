<!--
SPDX-FileCopyrightText: 2026 The Dukovo contributors
SPDX-License-Identifier: Apache-2.0 OR MIT
-->

# 0225. Bundles run in a sandbox of their own

- Status: Accepted; what a program gets of the permissions asked on
  first use and with a warning is ADR 0224's
- Date: 2026-11-01

## Context

Phase 13d.3b. bundled installs, seals and removes bundles (ADR 0223),
but nothing runs them yet. The security design (sections 9 and 10,
locked by Chase on 2026-10-26) asks that:

- a bundle's program runs in a sandbox of its own: it sees the system's
  commands and libraries, its own folder, read-only, and its own data
  folder, and nothing of the person's files unless a permission gives
  it;
- the network is a permission: `network.outbound` to connect out and
  `network.listen` with the ports it listens on, both given at install,
  and a program asks for nothing it does not use
  (`programs.NAME.permissions`);
- a command that the manifest says `runs_with: your-access` runs as the
  person's own programs do, outside the sandbox, which the install sheet
  says plainly;
- installing never edits a person's `PATH` or shell profile, unlike
  npm's `-g` or pip's `--user`;
- what runs is what was installed: a bundle changed after install, or
  one bundled did not seal, does not run.

Phase 12's exit test asks for the network half: a bundle without
network permission cannot open a socket. Sessions hold the network
(ADR 0257); views (ADR 0215) hold places by capabilities; a process
without a network capability gets EACCES from socket() (ADR 0184).

## Decision

- **Commands on PATH.** bundled keeps a `commands` folder in each
  place, `/applications/commands` and each person's
  `~/applications/commands`, root's, mode 0755. It holds a symbolic
  link by each command of the place's bundles, to
  `/system/commands/bundle`. A session's `PATH` is
  `/system/commands:/applications/commands:~/applications/commands`,
  so the system's commands come first, and a command another bundle in
  reach adds is refused at install. bundled makes each folder again
  from what it recorded after each install and removal and at its
  start, and leaves alone a `commands` that is not a folder of root's.
- **Running.** `bundle run [--command NAME] ID [ARGUMENT...]`, or a
  command by its name through its link (systools runs bundle for any
  name it does not know):
  1. bundle asks bundled which bundle the ID or command names: the
     asker's own first, then everyone's. bundled answers with its place,
     ID, key and the SHA-256 of the signed list it recorded.
  2. bundle believes nothing else. It opens the place's folder, checks
     it is its owner's, and opens the bundle's folder in it, not through
     a link, which must be root's, mode 0555. The list, the signature
     and the manifest must be sealed (root's, not writable), the list's
     SHA-256 the one recorded, its signature by the key recorded, and
     the manifest the one the list names, of the ID asked for.
  3. It chooses the program: the command's, or with none, the first
     command's, or the only program. It opens it one part at a time,
     never through a link: a regular file the list names, root's, not
     writable, executable.
  4. A `your-access` command is executed by its path, with all the
     caller holds. Any other runs in the sandbox.
- **The sandbox** is a process of its own whose root is a view, made
  from the caller's root without the bypass right or control, with each
  place at its real path:
  - the system's own, read-only: `/system/commands`, `libraries`,
    `resources` and `settings`, `/system/live/notify`, `/processes` and
    `/kernel`; and the devices every program has: `/devices/null`,
    which it may write, `zero`, `random` and `urandom`;
  - its bundle's folder, the very one checked, read-only;
  - its data folder, `~/app-data/ID`, which it may change, made mode
    0700 if it is not there; it is the program's `HOME` and its working
    folder;
  - its own `/tmp`, the caller's `/tmp/.app-ID`;
  - the links of `/` and `/system`, so `#!/bin/sh` and `/usr/bin/env`
    lead where they do;
  - with `network.outbound`, `/system/live/netd`, where the resolver's
    settings are.

  The view is given without control, so nothing more is attached to
  it. The process gets the random and log capabilities, the terminal as
  descriptors 0 to 2, and a clean environment: `HOME`, `USER`,
  `LOGNAME`, `PATH=/system/commands`, `TERM`, the caller's language
  settings and time zone, and nothing else (no `LD_PRELOAD`, no
  secrets). bundle waits for it, as system(3) does, and exits with its
  status.
- **The network** is made from the caller's network capabilities
  (ADR 0257), narrowed with `network::restrict`:
  `network.outbound` gives SEND on every port the caller may connect
  to; each `network.listen` range gives RECEIVE on exactly those ports
  from a capability that covers them. A required range the caller
  cannot listen on (below 1024, which only an administrator's commands
  may) stops it; an optional one is left out. Without either, the
  program holds no network capability, so socket() fails with EACCES.
- **Permissions until grants.** A program gets the permissions its
  manifest asks for that are given at install or always given, as many
  as it keeps. One that is asked at first use, with a warning or by an
  administrator is not given until grants and the trusted prompts come
  (13d.4): optional, the program runs without it; required, it does
  not run, saying which permission this version cannot grant yet.

## Consequences

- A bundle that asks for nothing sees only the system's own files, its
  folder and its data: not the person's home, other bundles or the
  network. Phase 12's exit test is the `bundle-run` boot test, whose
  bundle runs netgrant to check that it holds no network capability and
  that socket() fails.
- An administrator or anything else that unseals a bundle's folder, or
  changes its list, stops it from running until it is installed again.
- The sandbox acts for the person, so the owners and modes of files
  still decide inside it; it holds nothing the person does not.
- A bundle's command never shadows the system's, nor another bundle's
  in reach, since install refuses both; `bundle run ID` finds a
  person's own bundle before everyone's of the same ID.
- **Not yet:** grants, the trusted prompts and the permissions they give
  (files, devices, the screen and sound) (13d.4); the services and
  integrations a bundle adds; resource limits for the sandbox;
  a bundle's programs for other architectures than x86-64; the
  desktop's launchers, which run `bundle run` as the shell does.
