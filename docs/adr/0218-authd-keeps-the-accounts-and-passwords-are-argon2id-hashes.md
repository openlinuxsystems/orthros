<!--
SPDX-FileCopyrightText: 2026 The Dukovo contributors
SPDX-License-Identifier: Apache-2.0 OR MIT
-->

# 0218. authd keeps the accounts, and passwords are Argon2id hashes only it reads

- Status: Accepted
- Date: 2026-10-31

## Context

Phase 13b.5a. Since ADR 0214 every process and capability acts for a
name, and the kernel checks owners and modes against it, but every name
is still root: nothing makes an account, and nothing checks a password.
Sign-in (13b.5b) and elevate (13b.6) need both. The security design
keeps accounts plain: a name the owner chooses and a password, which
may be empty, with no rules forced on it (the project's principles),
and never an age, a birth date or an identity (ADR 0126).

On Linux the hashes are in `/etc/shadow`, root's and of mode 0640, and
programs that are not root check a password through a set-user-ID
helper (`unix_chkpwd`). Dukovo has no set-user-ID programs, and since
ADR 0217 every service reads `/system/settings` as root, so the hashes
cannot live there. One program has to hold them and answer the others,
and it has to know who asks.

## Decision

- **authd** (`systools`, authd(8)) is a service, order 8, restarted
  always, with a 160 MiB memory limit, that alone changes the account
  files and alone reads the hashes. Until services have names of their
  own (13d) it runs with Spark's root (`runs-with-your-access`), as
  consoled does.
- **The files.**
  - `/system/settings/passwd` and `group` keep Linux's formats, so the
    C library, `ls -l`, `id` and every ported program read them as
    they are; they hold no password (`x`).
  - `/system/state/authd/passwords` holds `name:hash:` per account, as
    `/etc/shadow` begins; root's, 0600, in a 0700 folder that no other
    service's view holds.
  - authd writes each file anew, syncs it and renames it over the old
    one: the hashes first, the passwd file last, so an account is there
    once its user is. Lines it does not read are kept.
- **The hashes** are Argon2id (RFC 9106) at the RFC's second
  recommendation, for machines without memory to spare: 64 MiB, 3
  passes, 4 lanes, a 16-byte salt from `/devices/urandom` and a 32-byte
  tag, a few tenths of a second on a laptop. They are PHC strings, as
  libxcrypt and argon2-cffi write them, so a hash moves to and from
  Linux; a hash says its cost, so authd checks one of any cost up to
  1 GiB, 16 passes and 16 lanes, and a later Dukovo can raise the cost
  without breaking old ones. An empty hash is an empty password; an
  account with no line, or one authd cannot read, is locked. The code is RustCrypto's
  `argon2` 0.5.3 (MIT OR Apache-2.0), checked against the reference
  implementation's output in the unit tests.
- **The socket** `/system/live/authd/socket` takes anyone's connection.
  A request is NUL-ended fields, `check`, `passwd` or `add`, and the
  answer one line, `ok` or `no why`. authd knows who asks from the
  connection: `SO_PEERCRED` now gives the user and group the other end
  acted for when it connected, which the kernel keeps on each Unix
  socket end (radix-file(2) v8, `socket_state`). Root checks any
  account's password, as sign-in will; another program only its own
  user's, as a screen lock does, with the same refusal whether or not
  the account is there. A user changes their own password given the
  current one; root changes anyone's, and makes accounts. Until elevate
  (13b.6), an administrator is root.
- **Wrong passwords** are answered only after a wait, per account and
  per asking user: 2 seconds for each of the first three in a row, then
  doubling up to a minute, with a try made during a wait waiting its
  turn. A right password, or 15 quiet minutes, ends the run. A name
  there is not is hashed against a decoy and answered alike. authd takes
  64 connections, 4 per process, and 10 seconds to send a request.
  Accounts made, passwords changed and wrong passwords are security
  events (ADR 0211); a name from a request shows in them with each
  control character as `?`, so that it cannot forge a line.
- **No rules.** `user` and `passwd` warn about an empty password, or one
  shorter than 8 characters, and ask whether to keep it; authd refuses
  none.
- **Accounts** follow Debian: names of lowercase letters, digits, `_`,
  `-` and `.`, starting with a letter or `_`, up to 32; numbers from
  1000, each with its own group; `nukesh` as the shell. Administrators
  are in `admins`, number 10, as Linux's `wheel` often is. A home is
  `/users/NAME`, 0700, with the security design's folders, 0755, and
  `app-data`, 0700, all the user's: made while the home is still
  root's, which is given to its user last.
- **The tools.** `user add` and `user list` (user(8)) and `passwd`
  (passwd(1)) ask authd. They read passwords unseen and twice at a
  terminal, and from the standard input's lines in a script.
- **Images.** `ob image --user NAME` and `ob run --user NAME` make an
  administrator's account in the image's root, hashed on the build host
  with the same crate, so a machine starts with someone to sign in as.
  An image's files are all root's (ADR 0213), so authd makes the home of
  each account that has none when it starts. The first-boot owner
  account, for images made without `--user`, comes with sign-in.

## Consequences

- No program but authd, root's tools and the kernel's bypass right can
  read a hash, and none can guess a password faster than one try every
  few seconds through authd; a copy of the hashes costs 64 MiB of memory
  per guess.
- `ob` takes BSD-3-Clause crates too (`subtle`, which Argon2 compares
  with), as the kernel's and the programs' policy already did.
- The `accounts` boot test makes accounts with `user`, checks the files,
  their modes, the homes and what `getpwnam`, `getgrnam`, `id` and `user
  list` read; asks authd as sign-in will, with the wait after a wrong
  password and tries in turn; and starts itself as alice, who changes
  her own password but not bob's, makes no account, checks no other
  password and reads no hash.
- **Not yet:** sign-in on the consoles and sessions (13b.5b); elevate
  and administrators acting as root (13b.6); removing and renaming
  accounts, and the account settings (13b.7); names of their own for
  services (13d).

## Alternatives considered

- **A set-user-ID helper, as Linux's.** Dukovo has no set-user-ID
  programs, and one service that answers is easier to throttle and to
  audit.
- **yescrypt**, Debian's default since 11. Argon2id is an RFC and
  OWASP's first choice, and has a maintained, permissively licensed
  Rust implementation; libxcrypt reads both.
- **Hashes in `/system/settings`**, readable by root only. Every service
  acts for root there since ADR 0217, so every service could read them.
