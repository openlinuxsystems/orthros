login(1) "Dukovo" "Dukovo Manual"

; SPDX-FileCopyrightText: 2026 The Dukovo contributors
; SPDX-License-Identifier: Apache-2.0 OR MIT

# NAME

login - sign in at a console

# SYNOPSIS

*login*

*login --session*

# DESCRIPTION

*login* signs people in at Dukovo's consoles (ADR 0250). *consoled*(8)
runs it as root on each console's terminal, after the console's
_/system/settings/issue_ text. It asks for a name, then for a password,
unseen, and sends both, with the terminal, to *authd*(8), which checks
the password and starts the person's session on that terminal: their
shell, acting for their user and groups, in their home. login then
waits, holding nothing of the terminal, until the session ends, and
ends too; consoled clears the console and starts it again.

A wrong name or password is answered, after a wait that grows with
each wrong one in a row, with *The name or the password is wrong.*,
whichever it was, and the name is asked again. Every sign-in and
sign-out, and every wrong password, is a security event in the audit
log (*authd*(8)).

Each console signs in on its own: two people, or one person twice, can
be signed in on two consoles at once, and *who*(1) lists them.

# THE FIRST SIGN-IN

On a machine with no account yet, made without *ob image --user*,
login first makes the owner's account, an administrator's (*user*(8)):
a name, a full name, which may be empty, and a password, twice. No
rules are forced on the password: an empty one, or one shorter than 8
characters, gets a warning and the question whether to keep it. Then
it signs the owner in. Only the first account is made this way; the
owner makes the others with *user add*.

```
Dukovo Server 0.10.4 (tty1)

Welcome to Dukovo. This computer has no account yet: make yours, the owner's,
which administers it. Nothing but a name and a password is needed, and the
password may be empty.

Name: alice
Full name (may be empty): Alice Liddell
New password:
The same again:
Made alice, number 1000, home /users/alice

$
```

# THE SESSION

authd runs *login --session* as the session's first process, acting
for the person who signed in, with the terminal as its standard input,
output and error. It starts a new session (*setsid*(2)) with the
terminal as its controlling terminal, goes to *HOME*, or to _/_ with a
message if it cannot, and runs *SHELL*, *nukesh*(1) unless the account
names another, as a login shell: its name starts with *-*. The
environment authd gives it is *HOME*, *SHELL*, *USER* and *LOGNAME* from
the account, *PATH=/system/commands*, *TERM* from the console,
*LANG=C.UTF-8* and *EDITOR=nano*.

# OPTIONS

*--session*
	Run as a session's first process, as above. authd passes it.

# EXIT STATUS

0 when a session started and ended; 1 when the input ended first, or
authd could not be reached in 30 seconds, saying so.

# SEE ALSO

*authd*(8), *consoled*(8), *user*(8), *passwd*(1), *who*(1), *nukesh*(1),
*systools*(1)
