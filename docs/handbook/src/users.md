<!--
SPDX-FileCopyrightText: 2026 The Dukovo contributors
SPDX-License-Identifier: Apache-2.0 OR MIT
-->

# Users and administration

This chapter describes accounts, signing in and administering as of
Phase 13b of the roadmap. The full references are login(1), authd(8),
user(8), passwd(1), elevate(8) and, for building an image with an
account in it, ob(1).

## Accounts

An account on Dukovo is a name you choose and a password, which may be
empty. Nothing else is asked: never an age, a birth date or an identity.
Each account has a number from 1000, a group of its own, and a home in
`/users/NAME` that only it can open, with `documents`, `downloads`,
`desktop`, `development`, `music`, `pictures`, `videos`, `applications`
and `app-data` in it.

An administrator's account is also in the `admins` group, which only
means they may run a command as root with `elevate` (see below).

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

Ctrl+Alt+Delete is the secure attention key: no program ever gets it,
so whatever it shows is the system's own. With no password prompt on
the console it shows a menu on the bottom line, where Enter takes you
to a real sign-in, on a console where nobody is signed in, R restarts
and P powers off.

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

## Administering with elevate

Dukovo has no setuid program and no root account to sign in to. An
administrator runs one command as root with `elevate`, or with `sudo`,
which is the same command by the name scripts and habits use:

```
$ elevate user add bob
Making a user's account for bob.
...
$ sudo dmesg
```

On a console the password is asked on the bottom line, by the consoles
themselves, with the command it is for:

```
 [elevate] Password for alice, to run /system/commands/user add bob:
```

No program draws that line or sees what you type there, and
Ctrl+Alt+Delete, which no program receives, brings it back empty: press
it first if a prompt looks wrong, and type your password only into what
comes up. Esc closes it. Over SSH elevate asks on the terminal, as sudo
does.

The password is asked once in each terminal pane, and that pane stays
unlocked for 15 minutes from its last elevated command; another pane,
or another console, asks again. `elevate -k` locks the pane at once,
and `elevate -s` gives root's own shell, whose prompt ends in `#`, for
a run of work.

An elevated command runs with root's environment, not yours: `PATH` is
always `/system/commands:/applications/commands`, so a command of your
own named like a system one never runs as root by accident, and only
the terminal's type and the language settings pass from yours. Ctrl+C
and Ctrl+Z reach it as they would any command. Each use, and each
refusal, is in the audit log, and someone who is not an administrator
cannot elevate at all.

## Making accounts

An administrator, through elevate:

```
$ elevate user add --admin --full-name "Alice Liddell" alice
Making an administrator's account for alice.
New password:
The same again:
Made alice, number 1000, home /users/alice
$ elevate user add bob
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

## Disabling, removing and renaming accounts

Disabling an account ends everything it runs at once, its sessions on
every console, what it left running and the commands it runs through
elevate, and nobody signs in to it until it is enabled again, with the
password it had:

```
$ elevate user disable bob
bob is disabled: 3 of its processes ended. user enable bob enables it again.
$ elevate user enable bob
bob is enabled again.
```

A folder the account handed to another program opens nothing more
either, since the system itself takes the account's access away, not a
command that hunts down its programs one by one. Someone who knows the
password of a disabled account is told it is disabled when they sign
in; anyone else is told the password is wrong.

Removing an account disables it the same way and takes it out of the
account files. `user` asks what becomes of its home:

- **keep** leaves it where it is, for an administrator to see to;
- **archive** packs it into one compressed file in `/users/.removed`,
  which only administrators open, and deletes it;
- **delete** deletes it, after asking again.

In a script, `--keep-home`, `--archive-home` or `--delete-home` says
which. A removed account's number is never given to another account, so
the files it left anywhere stay out of everyone else's reach.

```
$ elevate user remove --archive-home bob
bob is removed: its home is archived in /users/.removed/bob-2026-10-31.tar.zst,
for administrators only.
```

`elevate user rename bob robert` renames an account, with its group and
its home, once it runs nothing: after bob signs out and what he left
running ends, or once it is disabled. `user set` changes an account's
full name, which anyone changes for themselves, or whether it is an
administrator's: `elevate user set --admin bob`. Someone who is no
longer an administrator keeps nothing of it, not an elevated shell they
left open.

You do not disable, remove or rename the account you are signed in
with: another administrator does, so that your own session does not end
under you. Disabling or removing the last administrator who can sign
in, or making them no longer one, asks first, since nobody could
elevate afterwards; the startup-trouble menu (below) enables a disabled
administrator again.

## Who signed in, and when

As you sign in, Dukovo says when you last did, and whether anyone typed
a wrong password for your account since:

```
Last sign-in: Sat Oct 31 12:40 2026 on pts/0
Since then, 1 wrong password was typed for bob, the last on Sat Oct 31 12:44 2026 on pts/1.
```

`last` lists your sign-ins, newest first, with how long each lasted,
and when the computer started; `lastb` lists the wrong passwords typed
for your account, and `lastlog` your last sign-in. They are Linux's
commands, with their options:

```
$ last
bob      pts/1                         Sat Oct 31 12:46   still logged in
bob      pts/1                         Sat Oct 31 12:41 - 12:45  (00:04)
reboot   system boot  0.13.0           Sat Oct 31 12:39   still running

sign-in records begin Sat Oct 31 12:39:01 2026
```

Your sign-ins are yours: on Linux anyone reads everyone's, but here each
person sees only their own, and an administrator sees everyone's, with
`elevate last`, `elevate lastb` and `elevate lastlog`. The wrong
passwords typed for names that are no account's are for administrators
alone.

## Changing a password

`passwd` changes yours, given the one you have; an administrator,
`elevate passwd bob`, changes bob's without his. A wrong password is answered only after a
wait of two seconds, and a longer one after each wrong one in a row, up
to a minute, so a password cannot be guessed quickly.

## A lost administrator's password

If you lose the password of the only administrator, choose **Dukovo
Server: startup trouble** in the boot menu, which shows for three
seconds when the computer starts. The sign-in then first shows the
startup-trouble menu:

```
  1  Reset an administrator's password
  2  Sign in
  3  Restart
  4  Power off
```

Choose 1, name the administrator, and type the new password twice; then
choose 2 and sign in with it. A disabled administrator is enabled too. Nobody resets a password unseen: the audit
log keeps each reset, and every administrator is told at their next
sign-in, before their shell starts. The menu gives nothing that the boot
menu did not give already, since anyone at it can start root's shell; a
server is reached through its own console or its cloud provider's.
Once disks are encrypted, in release 1.4, a reset will need the
recovery key.

## Where accounts are kept

| File | What it holds |
| --- | --- |
| `/system/settings/passwd` | The users, in Linux's format, without passwords |
| `/system/settings/group` | The groups, in Linux's format |
| `/system/state/authd/passwords` | The passwords' Argon2id hashes, which only the authd service reads |
| `/system/state/authd/notices` | What each person is told at their next sign-in, such as a password reset |
| `/system/state/authd/retired` | The numbers of removed accounts, which no account gets again |
| `/system/state/sign-ins` | Who signed in, when, and the wrong passwords typed: a file for each account, which that person reads, and one of the computer's starts and stops |
| `/users/.removed` | The archived homes of removed accounts |

`/etc/passwd` and `/etc/group` lead to the first two, so programs from
Linux find them where they look. Only authd changes these files, and it
keeps any line it does not read, so a comment an administrator adds by
hand stays. A hash is in the format Linux's libxcrypt reads, so a
password hash can move between Dukovo and Linux.

Making, disabling, enabling, removing, renaming and changing an
account, changing a password and each wrong password are security
events in the audit log: `log --audit` shows them (see
[Logs, monitoring and troubleshooting](logs.md)).
