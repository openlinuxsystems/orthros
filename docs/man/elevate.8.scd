elevate(8) "Dukovo" "Dukovo Manual"

; SPDX-FileCopyrightText: 2026 The Dukovo contributors
; SPDX-License-Identifier: Apache-2.0 OR MIT

# NAME

elevate - run a command as an administrator

# SYNOPSIS

*elevate* [*-nS*] [*-p* _prompt_] [*-u root*] [*-k*] _command_ [_argument_...]

*elevate* [*-nS*] [*-p* _prompt_] *-s*|*-i* [_command_ [_argument_...]]

*elevate* *-v*|*-k*|*-K*|*-l*|*-V*|*-h*

# DESCRIPTION

*elevate* runs one command, or root's shell, with the admin set: as
root, past the owners and modes of every file, and with the platform
capability that *ectool*(8) uses (ADR 0219). It is how an administrator,
a member of the *admins* group, does what only root may: make accounts
with *user*(8), read the hashes' file or another user's home, run
*ectool*. *sudo*(8) is the same command, for the habits and scripts
that use that name.

There is no setuid program. elevate asks *authd*(8), which checks that
the person asking is an administrator, then asks for their password
unless their terminal pane typed it in the last 15 minutes, and then
starts the command itself, as its own child. Asked by someone who is
not an administrator, authd refuses before any password, and says so
in the audit log. A command that is not found is refused before any
password too.

In a pane on one of the consoles, the password is asked by
*consoled*(8), on the bottom line, where no program draws it or reads
what is typed, with the command it is for: *[elevate] Password for*
_name_*, to run* _command_*:*. Ctrl+Alt+Delete clears what was typed
there, and Esc closes it (ADR 0251). On any other terminal, as over
SSH, elevate asks on the terminal, unseen, as *[elevate]
password for* _name_*:* (*[sudo]* for sudo). A wrong one is asked
again, *Sorry, try again.*, up to three times in all, each answered
only after authd's wait for wrong passwords. Once typed, the pane, which is the terminal
of the session that asked, stays unlocked for 15 minutes from its last
elevated command, as sudo's per-terminal tickets do on Ubuntu: neither
another pane, another session on the same terminal, nor a program with
no terminal rides it. *elevate -k* locks it again.

# THE COMMAND

The command is looked for only in _/system/commands_, then
_/applications/commands_, whatever *PATH* says, as sudo's *secure_path*
does; a name with a */* is taken as given. It starts in the directory
elevate ran in, with an environment of its own, as sudo's *env_reset*
gives: *HOME*, *SHELL*, *USER* and *LOGNAME* from root's account,
*PATH=/system/commands:/applications/commands*, *TERM*, the language
settings *LANG*, *LANGUAGE* and *LC_\** whose values are at most 64
letters, digits and *.\_-@:*, and who asked, in sudo's own variables:
*SUDO_USER*, *SUDO_UID*, *SUDO_GID* and *SUDO_COMMAND*. Nothing else of the caller's environment passes, so
that nothing of theirs, such as a command of their own named like a
system one, runs as an administrator by accident. To give a command a
variable, run it through *env*(1): *elevate env NAME=value command*.

When the standard input, output or error is a terminal, the command
gets a new pseudo-terminal of its own in its place, and elevate copies
between it and its own, as sudo's *use_pty* does: the command and what
it starts are that terminal's foreground, so Ctrl+C and Ctrl+Z reach
them and not the shell that ran elevate. When the command stops, as
with Ctrl+Z, elevate stops too, and *fg* has both go on. elevate passes
the signals *SIGINT*, *SIGQUIT*, *SIGTERM*, *SIGHUP* and *SIGTSTP* it
gets on to the command; if elevate ends first, authd hangs the command
up with *SIGHUP*. A standard input, output or error that is not a
terminal is the command's own, so *elevate cat* _file_ *> copy* and
pipes work as with sudo.

authd starts each elevated command, and audits it, its start and its
end (*logd*(8)):

```
authd: alice elevated on pts/0 (password typed): process 81 runs /system/commands/user add bob, for process 77
authd: alice's elevated process 81 on pts/0 ended with status 0x0
authd: bob, not an administrator, asked to elevate on pts/1: dmesg, for process 93
```

As root, which an elevated command and root's shell are, elevate runs
the command itself, with the same path and environment, and *-v* and
*-k* do nothing.

# OPTIONS

*-s*, *--shell*
	Run root's shell, from _/system/settings/passwd_, in the current
	directory; with a command, have it run that, each character the
	shell would read as more than itself escaped with *\\*.

*-i*, *--login*
	Run root's shell as a login shell, in root's home; with a command,
	as with *-s*.

*-k*, *-K*, *--reset-timestamp*, *--remove-timestamp*
	Lock the pane again. With a command, lock it first, so that the
	password is asked.

*-v*, *--validate*
	Unlock the pane for 15 minutes, or 15 more, asking for the password
	if it is locked, and run nothing.

*-n*, *--non-interactive*
	Fail, saying a password is needed, rather than ask for one.

*-S*, *--stdin*
	Read the password as a line of the standard input, with no prompt
	on the terminal or the console, for scripts.

*-p*, *--prompt* _prompt_
	Ask with _prompt_, where *%p* and *%u* are the user's name, *%h* the
	host's, and *%%* a *%*, when elevate asks; consoled's prompt on a
	console keeps its own words.

*-u*, *--user* _user_
	Only *root*, *0* or *#0*: elevate runs commands as root alone. A
	service's own account comes with the service (Phase 13d).

*-H*, *--set-home*
	Nothing: *HOME* is always root's.

*-E*, *--preserve-env*
	Refused, saying how to pass a variable with *env*(1).

*-l*, *--list*
	Say whether the user may elevate.

*-V*, *--version*
	Print the version.

*-h*, *--help*
	Print the usage.

# EXIT STATUS

The command's exit status, or 128 and the number of the signal that
ended it; 1 when elevate could not run it, saying why: not an
administrator, three wrong passwords, a password needed with *-n*, or
a command not found. In the elevated process, 126 when the program
could not be run and 127 when it is not there.

# SECURITY

The admin set amounts to the whole machine: an administrator who can
write _/system/settings_ can have a service run anything. That is
intended, as the owner owns the machine; what elevate buys is that
nothing runs with that power by accident, and that each use is in the
audit log. A program running in an unlocked pane can elevate in its
15 minutes, as with sudo; *elevate -k* ends them. On the consoles the
password goes from the keyboard to consoled and authd only: a program
that draws a prompt like it cannot keep it up through Ctrl+Alt+Delete,
which brings the real one back, or consoled's menu. Elsewhere elevate
draws its own prompt, as sudo does, and a program of the user's could
draw one like it.

An elevated command is authd's child, in authd's resource domain, and
ends if authd does.

# SEE ALSO

*sudo*(8), *authd*(8), *user*(8), *passwd*(1), *login*(1), *env*(1),
*logd*(8), *systools*(1)
