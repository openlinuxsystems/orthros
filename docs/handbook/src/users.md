<!--
SPDX-FileCopyrightText: 2026 The Dukovo contributors
SPDX-License-Identifier: Apache-2.0 OR MIT
-->

# Users and administration

This chapter describes accounts and signing in as of Phase 13b of the
roadmap. The full references are login(1), authd(8), user(8),
passwd(1) and, for building an image with an account in it, ob(1).

## Accounts

An account on Dukovo is a name you choose and a password, which may be
empty. Nothing else is asked: never an age, a birth date or an identity.
Each account has a number from 1000, a group of its own, and a home in
`/users/NAME` that only it can open, with `documents`, `downloads`,
`desktop`, `development`, `music`, `pictures`, `videos`, `applications`
and `app-data` in it.

An administrator's account is also in the `admins` group. From the
next step of Phase 13b, an administrator runs a command as root with
`elevate`, which asks for their password once per terminal pane every
15 minutes; until then, root administers.

## Signing in

Each console, Alt+F1 to Alt+F6, asks for a name and a password:

```
Dukovo Server 0.10.4 (tty1)

Name: alice
Password:
$
```

Your session runs as you, in your home, until you leave it with `exit`
or Ctrl+D; the console then asks again. Each console signs in on its
own, so two people can work side by side, one on Alt+F1 and one on
Alt+F2, and `who` lists them. A wrong name or password gets the same
answer, after a wait.

### The first boot

A machine made without an account in its image has none, and no
password anyone knows: Dukovo ships none. Its first console asks you to
make the owner's account, an administrator's, and then signs you in:

```
Welcome to Dukovo. This computer has no account yet: make yours, the owner's,
which administers it. Nothing but a name and a password is needed, and the
password may be empty.

Name: alice
Full name (may be empty): Alice Liddell
New password:
The same again:
Made alice, number 1000, home /users/alice
```

### Root's shell without signing in

Someone who can change the kernel's command line at the boot menu can
already run anything as root, so `console-sign-in=off` there gives
each console root's shell with no sign-in, as Linux's `init=/bin/sh`
does: a way back in when no password works. See dukovo-boot(7).

## Making accounts

As root:

```
# user add --admin --full-name "Alice Liddell" alice
Making an administrator's account for alice.
New password:
The same again:
Made alice, number 1000, home /users/alice
# user add bob
```

`user list` lists them. A password is typed twice and never shown. No
rules are forced on it: an empty one, or one shorter than 8 characters,
gets a warning and the question whether to keep it, and you decide.

To have a machine start with an account, make it in the image:

```
./ob image --user alice --full-name "Alice Liddell"
```

`ob` asks for the password, and the image's root holds the account and
the password's hash, never the password; the home is made at the first
boot. `--password-file` takes the password from a file instead, for
scripts.

## Changing a password

`passwd` changes yours, given the one you have; as root, `passwd bob`
changes bob's without his. A wrong password is answered only after a
wait of two seconds, and a longer one after each wrong one in a row, up
to a minute, so a password cannot be guessed quickly.

## Where accounts are kept

| File | What it holds |
| --- | --- |
| `/system/settings/passwd` | The users, in Linux's format, without passwords |
| `/system/settings/group` | The groups, in Linux's format |
| `/system/state/authd/passwords` | The passwords' Argon2id hashes, which only the authd service reads |

`/etc/passwd` and `/etc/group` lead to the first two, so programs from
Linux find them where they look. Only authd changes these files, and it
keeps any line it does not read, so a comment an administrator adds by
hand stays. A hash is in the format Linux's libxcrypt reads, so a
password hash can move between Dukovo and Linux.

Making an account, changing a password and each wrong password are
security events in the audit log: `log --audit` shows them (see
[Logs, monitoring and troubleshooting](logs.md)).
