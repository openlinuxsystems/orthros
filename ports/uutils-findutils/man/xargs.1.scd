xargs(1) "Dukovo" "Dukovo Manual"

; SPDX-FileCopyrightText: 2026 The Dukovo contributors
; SPDX-License-Identifier: Apache-2.0 OR MIT

# NAME

xargs - run a command with arguments read from standard input

# SYNOPSIS

*xargs* [_options_] [_command_ [_initial-argument_ ...]]

# DESCRIPTION

*xargs* reads items from standard input and runs _command_ (*echo* if none
is given) with the _initial-arguments_ and then as many items as fit, as
many times as it takes to use them all. Items are separated by blanks and
newlines, which quotes (*'* and *"*) and backslashes protect, unless *-0*,
*-d* or *-I* says otherwise.

Dukovo's *xargs* is the one of uutils findutils, in Rust, which follows
GNU xargs. This page describes it as Dukovo ships it.

# OPTIONS

*-0*, *--null*
	Items are separated by NUL bytes and taken as they are, quotes and
	backslashes included, as *find -print0* writes them.

*-a* _file_, *--arg-file* _file_
	Read the items from _file_; the command's standard input is then the
	one *xargs* has.

*-d* _delimiter_, *--delimiter* _delimiter_
	Items are separated by _delimiter_, a character or a C escape such as
	*\\n*, and taken as they are.

*-E* _eof_, *-e*[_eof_], *--eof*[=_eof_]
	Stop at an item that is _eof_ (not with *-0* or *-d*).

*-I* _replace_
	Run _command_ once for each line of input, with every _replace_ in the
	initial arguments replaced by the line. Blanks at the start of a line
	are dropped.

*-i*[_replace_], *--replace*[=_replace_]
	*-I* with _replace_, or with *{}* if none is given.

*-L* _N_, *--max-lines* _N_
	At most _N_ lines of input for each run of _command_. A line that ends
	with a blank goes on into the next.

*-l*[_N_]
	*-L* with _N_, or 1 if none is given.

*-n* _N_, *--max-args* _N_
	At most _N_ items for each run of _command_.

*-s* _N_, *--max-chars* _N_
	At most _N_ bytes for each command line, its arguments and the end of
	each included. The default and the most allowed come from the system's
	limit on arguments (*ARG_MAX*, see *getconf*(1)).

*-x*, *--exit*
	Stop if a command line would be longer than *-s* allows, rather than
	running _command_ with fewer items.

*-r*, *--no-run-if-empty*
	Do not run _command_ at all if there are no items. (Without it,
	_command_ runs once with the initial arguments alone.)

*-t*, *--verbose*
	Print each command line on standard error before running it.

*-P* _N_, *--max-procs* _N_
	Accepted, but commands run one at a time.

*-h*, *--help*
	Show a summary of the options.

*-V*, *--version*
	Print the version.

# EXIT STATUS

*0*
	Every run of _command_ exited with 0.

*123*
	Some run of _command_ exited with a status from 1 to 125.

*124*
	_command_ exited with 255.

*125*
	_command_ was ended by a signal.

*126*
	_command_ could not be run.

*127*
	_command_ was not found.

*1*
	Another error.

# EXAMPLES

Count the lines of every C file below the working directory, names with
spaces and all:

	find . -name '\*.c' -print0 | xargs -0 wc -l

Copy each file a list names into _/shared/backup_:

	xargs -I {} cp {} /shared/backup < list.txt

# SEE ALSO

*find*(1), *getconf*(1)

# COPYRIGHT

xargs is part of uutils findutils, https://github.com/uutils/findutils,
by the uutils developers, under the MIT license. Dukovo builds it from
release 0.9.1 (ports/uutils-findutils). This page is Dukovo's.
