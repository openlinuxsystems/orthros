user(8) "Dukovo" "Dukovo Manual"

; SPDX-FileCopyrightText: 2026 The Dukovo contributors
; SPDX-License-Identifier: Apache-2.0 OR MIT

# NAME

user - make, list, disable, remove, rename and change accounts

# SYNOPSIS

*user add* [*--admin*] [*--full-name* _name_] _name_

*user list*

*user disable* [*--yes*] _name_

*user enable* _name_

*user remove* [*--keep-home* | *--archive-home* | *--delete-home*] [*--yes*] _name_

*user rename* _name_ _new-name_

*user set* [*--full-name* _name_] [*--admin* | *--no-admin*] [*--yes*] _name_

# DESCRIPTION

*user* makes, lists and changes Dukovo's accounts, through *authd*(8),
which alone changes the account files (ADR 0218, ADR 0220). Only root
changes an account, which an administrator is inside *elevate*(8),
except that anyone may change their own full name.

*user add* makes the account _name_: its user and group, its password
and its home, _/users/_*name*. It asks for the password twice, unseen,
at the terminal; with no terminal, as in a script, the password is the
first line of the standard input, asked once. No rules are forced on it:
an empty password, or one shorter than 8 characters, gets a warning,
and at a terminal the question whether to keep it. Only root may make
an account, which an administrator is inside *elevate*(8): *elevate
user add bob*.

A name is 1 to 32 lowercase letters, digits, *\_*, *-* and *.*, starting
with a letter or *\_*, and one no user or group has. *authd*(8) says what
an account gets.

*user list* lists the people's accounts, those numbered from 1000, with
their numbers, whether each is an administrator's, whether it is
enabled or disabled, which only root reads (*-* to others), their homes
and full names.

*user disable* disables the account _name_ at once: nothing signs in to
it, and everything it runs ends, its sessions, the programs it left
running and the commands it runs through *elevate*(8), however many it
starts meanwhile. A folder it handed to another program opens nothing
more. Its password is kept: *user enable* enables it again, as it was.
Someone who knows the password of a disabled account is told it is
disabled when they sign in; anyone else is told the password is wrong.

*user remove* removes the account _name_: it disables it and ends what
it runs, then takes it out of the account files, with its own group.
Its number is never given to an account again, so the files it leaves
are no one else's. At a terminal it asks what becomes of the account's
home, _/users/_*name*: whether to keep it, archive it or delete it;
a script says so with an option. A home elsewhere is kept.

*user rename* gives the account _name_ the name _new-name_, and its own
group too, and moves its home if it is _/users/_*name*. It waits until
nothing runs for the account: until its person has signed out and what
they left running has ended, or the account is disabled.

*user set* changes the account's full name, or whether it is an
administrator's. Someone who is no longer an administrator keeps
nothing of the admin set: their elevated commands end, and elevate
asks them nothing more.

An administrator does not disable, remove or rename the account they
are signed in with: another administrator does. Disabling or removing
the last administrator who can sign in, or making them no longer one,
asks first, saying that no one could then elevate; in a script it
needs *--yes*. The startup-trouble menu (*login*(1)) resets a disabled
administrator's password and enables the account.

# OPTIONS

*--admin*
	Make an administrator's account: one in the *admins* group, which
	may run commands as root with *elevate*(8).

*--full-name* _name_
	The account's full name, which the passwd file keeps after its
	number, as Linux's GECOS field; it may hold no colon or line break.

*--no-admin*
	With *user set*: the account is no longer an administrator's.

*--keep-home*
	With *user remove*: keep the home where it is. Its files stay
	those of the removed account's number, which no account gets
	again, so only administrators reach them; a new account of the
	same name is told so, and how to give it the files.

*--archive-home*
	With *user remove*: pack the home with *tar*(1) and zstd into a
	file in _/users/.removed_, root's alone, named for the account and
	the day, as _bob-2026-10-31.tar.zst_, then delete it.

*--delete-home*
	With *user remove*: delete the home and every file in it. At a
	terminal it asks first.

*-y*, *--yes*
	Go on without asking: when the account is the last administrator
	who can sign in, and before deleting a home.

# EXAMPLES

Make an administrator's account for Alice:

```
# user add --admin --full-name "Alice Liddell" alice
Making an administrator's account for alice.
New password:
The same again:
Made alice, number 1000, home /users/alice
```

List the accounts:

```
$ elevate user list
NAME             NUMBER  KIND           STATE     HOME                 FULL NAME
alice              1000  administrator  enabled   /users/alice         Alice Liddell
bob                1001  user           disabled  /users/bob
```

Disable Bob's account, which ends his sessions at once, then enable it
again:

```
$ elevate user disable bob
bob is disabled: 3 of its processes ended. user enable bob enables it again.
$ elevate user enable bob
bob is enabled again.
```

Remove Bob's account, archiving his home:

```
$ elevate user remove --archive-home bob
bob is removed: its home is archived in /users/.removed/bob-2026-10-31.tar.zst,
for administrators only.
```

Change your own full name:

```
$ user set --full-name "Alice P. Liddell" alice
alice is changed: full name "Alice P. Liddell".
```

# FILES

_/system/state/authd/retired_
	The numbers of removed accounts, which no account gets again: a
	line _number_:_name_:_date_ each, root's alone.

_/users/.removed_
	The archives of removed accounts' homes, root's alone.

# EXIT STATUS

0 when the account was made, listed or changed; 1 when it was not,
saying why.

# SEE ALSO

*passwd*(1), *login*(1), *authd*(8), *elevate*(8), *id*(1), *tar*(1), *systools*(1)
