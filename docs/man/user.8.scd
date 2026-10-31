user(8) "Dukovo" "Dukovo Manual"

; SPDX-FileCopyrightText: 2026 The Dukovo contributors
; SPDX-License-Identifier: Apache-2.0 OR MIT

# NAME

user - make and list accounts

# SYNOPSIS

*user add* [*--admin*] [*--full-name* _name_] _name_

*user list*

# DESCRIPTION

*user* makes and lists Dukovo's accounts, through *authd*(8), which
alone changes the account files (ADR 0218).

*user add* makes the account _name_: its user and group, its password
and its home, _/users/_*name*. It asks for the password twice, unseen,
at the terminal; with no terminal, as in a script, the password is the
first line of the standard input, asked once. No rules are forced on it:
an empty password, or one shorter than 8 characters, gets a warning,
and at a terminal the question whether to keep it. Only root may make
an account, and from Phase 13b.6 an administrator inside *elevate*.

A name is 1 to 32 lowercase letters, digits, *\_*, *-* and *.*, starting
with a letter or *\_*, and one no user or group has. *authd*(8) says what
an account gets.

*user list* lists the people's accounts, those numbered from 1000, with
their numbers, whether each is an administrator's, their homes and full
names.

# OPTIONS

*--admin*
	Make an administrator's account: one in the *admins* group, which
	may elevate (Phase 13b.6).

*--full-name* _name_
	The account's full name, which the passwd file keeps after its
	number, as Linux's GECOS field; it may hold no colon or line break.

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
$ user list
NAME             NUMBER  KIND           HOME                 FULL NAME
alice              1000  administrator  /users/alice         Alice Liddell
bob                1001  user           /users/bob
```

# EXIT STATUS

0 when the account was made, or listed; 1 when it was not, saying why.

# SEE ALSO

*passwd*(1), *authd*(8), *id*(1), *systools*(1)
