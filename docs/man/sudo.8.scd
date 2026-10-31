sudo(8) "Dukovo" "Dukovo Manual"

; SPDX-FileCopyrightText: 2026 The Dukovo contributors
; SPDX-License-Identifier: Apache-2.0 OR MIT

# NAME

sudo - run a command as an administrator, through elevate

# SYNOPSIS

*sudo* [*-nS*] [*-p* _prompt_] [*-u root*] [*-k*] _command_ [_argument_...]

*sudo* [*-nS*] [*-p* _prompt_] *-s*|*-i* [_command_ [_argument_...]]

*sudo* *-v*|*-k*|*-K*|*-l*|*-V*|*-h*

# DESCRIPTION

*sudo* is *elevate*(8) by the name Linux's habits and scripts use: the
same program, with the same options, which are sudo's own. It runs
the command as root with the admin set, after the administrator's
password unless their terminal pane typed it in the last 15 minutes,
asked as *[sudo] password for* _name_*:*. There is no _sudoers_ file:
every administrator, a member of the *admins* group, may run any
command, and nobody else may run one (ADR 0219).

Options sudo has that elevate does not take, such as *-e*, *-b* or
*-g*, are refused as unknown. *-E* is refused, since an elevated
command keeps none of the caller's environment but the terminal's type
and the language settings; *-u* takes only root.

# EXIT STATUS

As *elevate*(8).

# SEE ALSO

*elevate*(8), *authd*(8), *user*(8), *systools*(1)
