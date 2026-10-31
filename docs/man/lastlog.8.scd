lastlog(8) "Dukovo" "Dukovo Manual"

; SPDX-FileCopyrightText: 2026 The Dukovo contributors
; SPDX-License-Identifier: Apache-2.0 OR MIT

# NAME

lastlog - show each account's last sign-in

# SYNOPSIS

*lastlog* [*-u* _name_] [*-t* _days_] [*-b* _days_]

# DESCRIPTION

lastlog shows when each account last signed in, on what terminal and
from where, as shadow's lastlog does, from the sign-in records
*authd*(8) keeps (ADR 0254): root's account and the people's, in the
order of _/system/settings/passwd_. An account that never signed in
shows *\*\*Never signed in\*\**.

Root reads every account's. Anyone else reads their own alone, and
gets a hint on the standard error saying so; an administrator runs
*elevate lastlog*.

# OPTIONS

*-u* _name_, *--user* _name_
	Only the account _name_, or of that number. Another person's is
	refused to anyone but root.

*-t* _days_, *--time* _days_
	Only sign-ins of the last _days_ days.

*-b* _days_, *--before* _days_
	Only sign-ins older than _days_ days, and the accounts that never
	signed in.

# EXAMPLES

```
$ elevate lastlog
Username         Port     From             Latest
root                                       **Never signed in**
alice            pts/0                     Sat Oct 31 12:40:01 +0000 2026
bob              pts/1                     Sat Oct 31 12:46:12 +0000 2026
```

# FILES

_/system/state/sign-ins/_*number*
	Each account's sign-ins, which its person and root read.

# EXIT STATUS

0; 1 if the accounts cannot be read, for an account there is not or
another person's, and for a mistake in the command line.

# SEE ALSO

*last*(1), *authd*(8), *user*(8), *systools*(1)
