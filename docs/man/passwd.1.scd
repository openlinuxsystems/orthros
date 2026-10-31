passwd(1) "Dukovo" "Dukovo Manual"

; SPDX-FileCopyrightText: 2026 The Dukovo contributors
; SPDX-License-Identifier: Apache-2.0 OR MIT

# NAME

passwd - change a password

# SYNOPSIS

*passwd* [_name_]

# DESCRIPTION

*passwd* changes your password, or as root, the password of the account
_name_, through *authd*(8) (ADR 0218).

It asks for the password you have, then the new one twice, unseen, at
the terminal. Root is not asked for the one the account has. With no
terminal, as in a script, each is a line of the standard input, the new
one asked once. No rules are forced on a password: an empty one, or one
shorter than 8 characters, gets a warning, and at a terminal the
question whether to keep it.

A wrong current password is answered only after a wait, which grows
with each wrong one in a row (*authd*(8)). Only you and root, and from
Phase 13b.6 an administrator inside *elevate*, change your password.

# EXAMPLES

```
$ passwd
Changing the password of alice.
Current password:
New password:
The same again:
The password of alice is changed.
```

# EXIT STATUS

0 when the password was changed; 1 when it was not, saying why.

# SEE ALSO

*user*(8), *authd*(8), *systools*(1)
