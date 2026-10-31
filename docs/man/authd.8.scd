authd(8) "Dukovo" "Dukovo Manual"

; SPDX-FileCopyrightText: 2026 The Dukovo contributors
; SPDX-License-Identifier: Apache-2.0 OR MIT

# NAME

authd - keep the accounts and check passwords

# SYNOPSIS

*authd*

# DESCRIPTION

authd keeps Dukovo's accounts and their passwords (ADR 0218). Spark runs
it as the *authd* service. It alone changes the account files, and it
alone reads the passwords' hashes: other programs ask it over a socket,
as *user*(8) and *passwd*(1) do, and sign-in will.

An account is a name its owner chooses and a password, which may be
empty: nothing else, and never an age, a birth date or an identity
(ADR 0126). Each lives in three files:

- _/system/settings/passwd_ and _/system/settings/group_, in Linux's
  formats, which every program reads (*getpwnam*(3), *ls -l*, *id*(1))
  and which hold no password;
- _/system/state/authd/passwords_, a line _name_*:*_hash_*:* for each
  account, as Linux's _/etc/shadow_ begins, root's and of mode 0600 in a
  folder of mode 0700, which no other service's view holds
  (*service.toml*(5)).

A hash is Argon2id (RFC 9106), at the RFC's second recommendation: 64
MiB of memory, 3 passes and 4 lanes, with a salt of 16 random bytes,
written as a PHC string, as libxcrypt and Python's argon2-cffi write it:

```
alice:$argon2id$v=19$m=65536,t=3,p=4$c29tZXNhbHQ...$GpZ3sK/oH9p7...:
```

A hash says its cost, so one moved from Linux, or made at another cost,
is checked all the same, up to 1 GiB, 16 passes and 16 lanes. An empty
hash is an empty password; an account with no line, or a line authd
cannot read, is locked: no password opens it.
authd writes each file anew beside the old one, syncs it and renames it
over the old one, the hashes first and the passwd file last, so that an
account is there once its user is. Lines it does not read, an
administrator's comments among them, are kept as they are.

No rules are forced on a password (ADR 0218): *user* and *passwd* warn
about an empty one, or one shorter than 8 characters, and ask whether to
keep it; the person who answers decides.

# THE SOCKET

authd answers at _/system/live/authd/socket_, which anyone may connect
to. A request is its fields, each ending in a NUL byte, after which the
client shuts down its end for writing; authd answers with one line,
*ok* or *ok* _text_ when it did what was asked, or *no* _why_, then
closes the connection:

*check* _name_ _password_
	Whether _password_ is _name_'s. Root asks this of any account, as
	sign-in will; another program only of the account it runs for, as a
	screen lock does, and gets *no* for any other name, there or not.

*passwd* _name_ _current_ _new_
	Sets _name_'s password to _new_. Root changes anyone's, with an
	empty _current_; a user changes only their own, given the one it
	has.

*add* _name_ _full-name_ *admin*|*user* _password_
	Makes an account (below). Only root may.

authd knows who asks by the connection: the kernel gives it the process
at the other end, and the user and group it acted for when it connected
(*SO_PEERCRED*, *radix-posix*(7)). Until *elevate* (Phase 13b.6), an
administrator is root.

# WRONG PASSWORDS

A wrong password is answered only after a wait: 2 seconds for each of
the first three in a row, then twice as long each time, up to a minute,
for that account asked by that user. A try made while one waits waits
its turn, so tries at once do not go faster. A right password ends the
run; one forgotten after 15 minutes with no try ends it too. A name
there is not is checked against a hash all the same, and answered the
same way after the same wait, so that neither the answer nor the time
says whether the account is there.

authd answers 64 connections at a time and 4 from one process; one that
sends no whole request in 10 seconds is closed.

# ACCOUNTS

An account's name is 1 to 32 lowercase letters, digits, *\_*, *-* and
*.*, starting with a letter or *\_*, as Debian's adduser takes them. It
gets the lowest number from 1000 that no user or group has, a group of
its own of that name and number, the shell *nukesh*(1) and the home
_/users/_*name*, of mode 0700, with _documents_, _downloads_, _desktop_,
_development_, _music_, _pictures_, _videos_ and _applications_, of mode
0755, and _app-data_, of mode 0700, all the user's. An administrator's
account is also in the *admins* group, number 10, which authd makes if
it is not there.

When it starts, authd makes the home of each account that has none,
such as one *ob image --user* made in an image before its first boot,
whose files are all root's. It makes the folders while the home is
still root's, and gives the home to its user last, so that a user
already running cannot put a link where a folder is to go.

Each account made, each password changed and each wrong password is a
security event in the audit log (*logd*(8)), with the process and user
that asked. A name a request gave is shown as at most 64 characters,
with *?* for each control character, so that it cannot start a line of
its own:

```
authd: made the account alice (1000), an administrator's, for process 41 (user 0)
authd: a wrong password for alice, from process 52 (user 0); 1 in a row
```

# FILES

_/system/settings/passwd_
	The users.

_/system/settings/group_
	The groups.

_/system/state/authd/passwords_
	The passwords' hashes.

_/system/live/authd/socket_
	The socket authd answers at.

_/users/_*name*
	Each account's home.

# EXIT STATUS

As a service, authd does not exit but when it is stopped, with 0; it
exits with 1 if it cannot start.

# SEE ALSO

*user*(8), *passwd*(1), *id*(1), *service*(8), *spark*(8), *logd*(8),
*radix-posix*(7), *systools*(1)
