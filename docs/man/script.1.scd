script(1) "Dukovo" "Dukovo Manual"

; SPDX-FileCopyrightText: 2026 The Dukovo contributors
; SPDX-License-Identifier: Apache-2.0 OR MIT

# NAME

script - save a terminal session to a file

# SYNOPSIS

*script* [*-aefq*] [*-c* _command_] [_file_]

# DESCRIPTION

script runs a shell on a new pseudo-terminal (*radix-posix*(7)), as
util-linux's script does, and saves everything it writes to _file_
(_typescript_ by default) while passing it on to the terminal. The
session ends when the shell does (*exit*, or Ctrl-D).

The shell is _$SHELL_, or _/bin/sh_. The new terminal is the size of
the one script runs on, and follows it when it changes size. script's
own terminal is put in raw mode for the session, so that every key goes
to the shell's terminal, which handles it, and is put back after.

The file starts with a line that says when the session started, and
with what command, terminal type and size, and ends with one that says
when it ended and with what status:

	Script started on 2026-10-23 14:02:11+0000 [TERM="xterm" COLUMNS="80" LINES="24"]
	...
	Script done on 2026-10-23 14:05:40+0000 [COMMAND_EXIT_CODE="0"]

When script's input is not a terminal, what it reads is the session's
input, and its end is an end-of-file character typed to the shell.

# OPTIONS

*-c*, *--command* _command_
	Run _command_ with *$SHELL -c*, not an interactive shell.

*-a*, *--append*
	Add to _file_ rather than replacing it.

*-e*, *--return*
	Exit with the shell's or command's status.

*-f*, *--flush*
	Write to _file_ as soon as anything is written, so that another
	terminal can follow it.

*-q*, *--quiet*
	Do not say when the session starts and ends.

*-E*, *--echo* _when_ is taken and changes nothing.

# EXIT STATUS

0, or with *-e* the shell's or command's: its exit status, or 128 and
the signal's number if a signal ended it.

# EXAMPLES

Save a build's output with its colours:

	script -q -c 'ob build' build.log

# SEE ALSO

*nukesh*(1), *systools*(1), *radix-posix*(7)
