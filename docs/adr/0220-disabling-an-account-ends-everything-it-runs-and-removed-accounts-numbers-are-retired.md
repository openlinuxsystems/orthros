<!--
SPDX-FileCopyrightText: 2026 The Dukovo contributors
SPDX-License-Identifier: Apache-2.0 OR MIT
-->

# 0220. Disabling an account ends everything it runs, and removed accounts' numbers are retired

- Status: Accepted
- Date: 2026-10-31

## Context

Phase 13b.7. With sign-in (ADR 0250) and elevate (ADR 0219), accounts
are made, but not disabled, removed, renamed or changed. The security
design's third look, row 13, which Chase accepted on 2026-10-26, says:
"disabling an account revokes its capabilities, which stops its
sessions, SSH workers, SMB connections and jobs at once, removing one
asks whether to keep, archive or delete the home". Its exit check: a
disabled account's shell ends at once.

On Linux, `usermod -L` puts `!` in front of the hash in `/etc/shadow`,
and nothing more: what the account runs goes on, and an administrator
follows with `pkill -KILL -u bob`, which a process that forks fast
enough outruns. `userdel` refuses an account that runs anything, unless
`-f`; `-r` deletes the home. A file or directory a process handed to
another stays open. And `useradd` gives a new account the number after
the highest, so a removed account's number can come back, and with it
the files it left: a known way for one person to read another's.

In Dukovo, every process and every file or directory capability acts
for a name (ADR 0214), which only the names capability gives: Spark and
authd hold it.

## Decision

- **The kernel ends a user.** The names capability gets a third method,
  `end` (radix-names(2)). It sends SIGKILL to every process that acts
  for the user, and from then on the second gate turns away, with
  `denied`, every directory and file capability that acts for a name of
  that user given before, wherever it is held: a directory the account
  handed to another program opens nothing more. Each name counts when
  it was given, and the kernel keeps, for each user ended since it
  started, the count at which it was; a name given before is stale.
  Names given afterwards work, so an account enabled again signs in as
  before. Until a user is ended, the gate checks nothing more. A file
  opened already reads and writes as before until it is closed, as on
  Linux. `end` answers how many processes it ended; a process one of
  them made as it ended may still start, so a caller calls it until it
  answers 0. Root is never ended.
- **Disabling** (`user disable bob`, authd's `disable` request): authd
  marks the account with Linux's `!` in front of its hash, keeping the
  password, then ends the elevated commands it started for it, forgets
  the panes unlocked for it, and has the kernel end the user, until
  nothing is left. A disabled account signs in to nothing: the right
  password gets `the account bob is disabled`, which only someone who
  knows it learns, as Linux's login tells of an expired account; a
  wrong one is answered as any wrong one. `user enable` takes the mark
  away. `passwd` keeps it; the startup-trouble menu's reset (ADR 0253)
  enables a disabled administrator, since it is how the owner gets
  back in.
- **Removing** (`user remove`): authd disables the account and ends it
  first, then retires its number in `/system/state/authd/retired` and
  takes its lines out of the account files, its own group with them.
  A retired number is never given again, so whatever files it left
  are no one else's. `user` asks what becomes of its home, or a script
  says: `--keep-home` leaves it where it is, still the retired
  number's; `--archive-home` packs it with tar and zstd into
  `/users/.removed`, root's, mode 0700, and deletes it; `--delete-home`
  deletes it, after a question at a terminal. Only a home of its own in
  `/users` is archived or deleted. A new account of the same name is
  told whose a kept home is, and how to give it the files.
- **Renaming** (`user rename bob robert`) renames the user, its own
  group, its name in the groups' members, its hash's line and its
  notices, and moves its home if it is `/users/bob`, while nothing runs
  for the account, as Linux's `usermod -l` waits.
- **Settings** (`user set`): the full name, which anyone changes for
  themself as chfn(1) lets them, and whether the account is an
  administrator's. Someone made no longer one keeps nothing of the
  admin set: their elevated commands end and their panes lock.
- **Guards that warn and ask.** An administrator does not disable,
  remove or rename the account they are signed in with: another one
  does, as on other systems. Disabling, removing or taking away the
  last administrator who can sign in asks first, or needs `--yes` in a
  script, saying what follows: no one can elevate again.
- `user list` shows each account's state to root.

## Consequences

- A disabled account's sessions, and everything it left running, end
  at once; the folders it handed out open nothing. 13c's SSH workers,
  13f's SMB connections and 16a's jobs act for the user, so they end
  the same way when they come.
- A capability stamped before an end is denied even with the bypass
  right: whoever holds one asks for another.
- The `principals` boot case ends carol, a user with a child, and
  checks that both end, that the kernel answers 0 the second time,
  that a capability stamped for her before is denied, one stamped
  after is not, and that root is never ended. The `accounts` case
  changes bob's full name and makes him an administrator and not;
  checks that alice, the last administrator, is not disabled without a
  yes; disables bob while two of his processes run, which end, and his
  password then answers that the account is disabled; enables him;
  renames him while he runs something, refused, and once nothing does;
  removes him, deleting his home, and checks that his number is not
  given again; archives carol's home and keeps dave's. The `elevate`
  case disables bob while he is signed in on tty2: his session ends,
  and he signs in again only once alice enables him.
- **Not yet:** the setting that lets a user's jobs run while they are
  signed out, which waits for jobs (16a); the owner's limit on what an
  account may install, which waits for the catalog (13e); `last` and
  `lastlog` (13b.7b), a private `/tmp` per session (13b.7c) and access
  lists (13b.7d).

## Alternatives considered

- **Disabling as Linux's `usermod -L`, then `pkill -u`.** A forking
  process outruns the kill, and a directory handed to another program
  goes on opening as the account. The kernel, which names everything,
  ends it in one place.
- **Revoking through the derivation tree** (radix-capability(2)'s
  `revoke`). It deletes the capabilities derived from one, but `stamp`
  makes a new object acting for the name, and so does every open
  through it: a user's capabilities are many trees, rooted in nothing
  authd keeps. Counting names reaches all of them at the gate, for one
  comparison, and only once a user was ended.
- **Re-checking open files at each read and write.** It would cost
  every read the check, and Linux does not either; the gate is where
  access is decided.
- **Reusing numbers, as Linux.** A new account would own whatever the
  old one left behind, outside its home too.
- **Removing accounts in authd, homes included.** Archiving a large
  home takes long, and authd answers on one thread; the `user` command,
  running as root through elevate, does it after authd answers.
