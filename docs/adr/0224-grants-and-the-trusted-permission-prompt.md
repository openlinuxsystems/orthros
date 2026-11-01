<!--
SPDX-FileCopyrightText: 2026 The Dukovo contributors
SPDX-License-Identifier: Apache-2.0 OR MIT
-->

# 0224. Grants and the trusted permission prompt

- Status: Accepted
- Date: 2026-11-01

## Context

Phase 13d.4, the rest of 13d. Bundles install sealed (ADR 0223) and run
in a sandbox of their own (ADR 0225), but get only the permissions given
at install. The security design (sections 6, 8 and 10, locked by Chase
on 2026-10-26) asks that:

- a permission asked "on first use" or "with a warning" (your folders,
  drives, `/shared`, all of your files, other apps' data, serial ports)
  is asked the first time the app wants it, in the person's own session;
  "Always allow" is remembered, also across updates, and an update asks
  only for something new;
- every permission question is drawn by the component that owns the
  screen, consoled on the console, never by the program that wants
  something, which can neither answer it nor read what is typed;
  changing grants goes through the same prompt, so a script cannot run
  `bundle grant` for itself;
- revoking works at once: the app loses the permission while it runs,
  not at its next start;
- the grant store lives in `/system/state/grants`, for each user, and
  only the bundle runtime writes it; a grant belongs to one publisher's
  key;
- `bundle info` lists the grants.

Phase 13's exit asks for it: a user installs a sample bundle into
`~/applications`, approves `documents: read-write`, launches it, and it
can write `~/documents` but not `~/downloads`. The console already asks
elevate's password on its bottom line, where no program draws (ADR 0251).

## Decision

- **Grants.** bundled keeps, for each person, what they said of each
  app's permissions, in `/system/state/grants/UID`, mode 0600 in a 0700
  folder, root's, written whole through a rename: a line for each, the
  app's ID, the publisher's key, the permission, `allow` or `never`,
  since when, the words it gives and the words the app asked for then.
  Words are a permission's objects: `NAME=read` or `NAME=read-write` for
  a folder, an ID for an app. A file bundled cannot read is set aside as
  `UID.bad`, so its person is asked again rather than given anything.
- **What a program gets.** `bundle run` asks bundled which bundle to
  run; bundled answers with what the asker said of it, for its key
  only. For each permission the program keeps:
  - given at install or always: given, unless the person took it away;
  - asked on first use or with a warning: given as the grant says, if
    the grant answered at least what the app asks for now; otherwise
    asked (below);
  - one a grant says `never` to: left out; a program that requires it
    does not run, and says `bundle grant ID PERMISSION` gives it;
  - an administrator's, `network.local` (until discovery, 12c),
    `processes.debug`, or reserved: not given; required, the program
    does not run.

  A folder given to read only is read only, whatever the app asks for;
  a version that asks for more than the grant answered is asked again.
  A `your-access` command needs `runs-with-your-access`, asked with a
  warning the first time it runs.
- **Asking.** The runner sends bundled `ask ID PERMISSION LIST`, with
  the SHA-256 of the signed list it checked, so a bundle changed since
  is refused. bundled reads the permission from the sealed manifest
  itself, words it in the system's own words, finds the terminal of the
  process that asks (its session's, from /processes, if it acts for the
  same user), and sends authd a new root-only request, `choose TERMINAL
  NAME QUESTION TEXT`. authd passes it to consoled as it passes
  elevate's password prompts, `choose ID TERMINAL NAME QUESTION TEXT`,
  and consoled shows it on that console's bottom line:

  ```
   [permission] Notes (org.example.notes) asks to: use your documents folder, to read and change it      O: once    A: always    D: don't allow
  ```

  in black on cyan; a permission asked with a warning says `[warning]`
  in white on red. The keys are consoled's while it shows: O allows it
  this run, A from now on, D never; Esc or Ctrl+C close it, which leaves
  it out this run; no other key does anything, and Ctrl+Alt+Delete
  brings the real prompt back. The text keeps the keys and is cut with
  `...` where the console is narrower. consoled answers authd `prompted
  ID once|always|never|cancelled`, and authd answers bundled. A
  terminal that is not a console's, or no consoled, is answered
  `elsewhere`, and the program runs without the permission, or does not
  run, saying to give it with `bundle grant` at a console. Always and
  never are recorded; each answer is an audit event.
- **`bundle grant ID PERMISSION [WORD...]`** gives a permission the app
  asks for, as it asks for it or with less (`documents=read`), after
  the same prompt's Y (`[permission] Give Notes (org.example.notes): use
  your documents folder, to read it? Y: yes N: no`). It refuses on a
  terminal that is not a console's, and anything an administrator gives.
- **`bundle revoke ID PERMISSION`** records `never` with no question.
  A run that holds a permission keeps a `watch ID` connection to
  bundled, opened before it starts; bundled tells each of them
  `revoked PERMISSION`. A view keeps what is attached to it, as nothing
  is ever detached (ADR 0215), and a socket outlives the network
  capability it was made with, so the runner stops the program and the
  processes it started: SIGTERM, and SIGKILL two seconds later to each
  that has not ended. bundle exits 143. bundled never waits to write to
  a run: one that does not read what it is told is forgotten.
- **`bundle info ID`** lists what the person said of each permission and
  since when. **`bundle remove --purge`** forgets their grants with the
  data; `--keep-data` keeps both.
- **The view** gets, each at its real path: `files.folder`'s folders of
  the person's home, read-only or not, made mode 0700 if missing;
  `files.home`, the whole home, which holds the data folder and, for a
  bundle of the person's own, the bundle's folder; `files.volumes`,
  `/volumes`; `files.shared`, its folders of `/shared`;
  `files.app-data`, the other apps' data folders, read-only; and
  `devices.serial`, the `ttyACM` and `ttyUSB` terminals there are when
  it starts, those the person can open.

## Consequences

- The Phase 13 exit's bundle is the `bundle-grants` boot test: asked
  about alice's documents on its first run, it writes in them after A,
  not in her downloads, is stopped at once when they are taken away,
  and reads them only after `bundle grant ... documents=read`.
- No program draws, answers or reads a permission question, and none
  can give itself a permission: bundled writes grants only after the
  prompt, and `bundle grant` goes through it. Taking a permission away
  is never asked.
- "On first use" means the program's first run that wants it: a view
  cannot ask at the moment of a file's open. Until the desktop's portals
  (1.5), an optional permission is asked once too, then remembered.
- At 80 columns the question is cut, with its keys still shown; the
  person reads it whole with `bundle info` or a wider console.
- A program that leaves its process tree (a daemon re-parented to
  Spark) before a revoke is not found by the runner; per-app resource
  domains, with service bundles (13e), will stop it whole.
- **Not yet:** asking over SSH (13c) and on the desktop (1.5); grants an
  administrator makes for everyone and before install (`--grant`,
  section 8); `network.local`, `processes.debug` and the reserved
  permissions; when each grant was last used, for Settings; serial
  ports plugged in while the app runs.
