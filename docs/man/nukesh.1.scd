nukesh(1) "Dukovo" "Dukovo Manual"

; SPDX-FileCopyrightText: 2026 The Dukovo contributors
; SPDX-License-Identifier: Apache-2.0 OR MIT

# NAME

nukesh, sh, rnukesh - Dukovo's shell

# SYNOPSIS

*nukesh* [_long options_] [*-abCefhimnuvxBEHPT*] [*-o* _option_] [*-O* _shopt_] [_script_ [_argument_ ...]]

*nukesh* [_options_] *-c* _command_ [_name_ [_argument_ ...]]

*nukesh* [_options_] *-s* [_argument_ ...]

*sh* ...

*rnukesh* ...

# DESCRIPTION

Nukesh is the shell of Dukovo: the one people type at, and _/bin/sh_,
which runs the system's scripts and the commands of *system*(3),
*popen*(3) and *wordexp*(3). It is written in Rust for Dukovo.

It is the POSIX shell (XCU, Shell Command Language), with the parts of
bash that scripts use: arrays and associative arrays, *[[ ]]*, *(( ))*,
*${x//a/b}* and the other parameter expansions, brace expansion,
process substitution, here-strings, *local*, *declare*, *mapfile*,
*printf -v*, *set -o pipefail*, extended patterns, *shopt*, the *ERR*,
*DEBUG* and *RETURN* traps, *BASH_REMATCH*, *PIPESTATUS*, *FUNCNAME*,
programmable completion and the rest. A bash script runs unchanged, and
gives what bash gives: the test suite of Nukesh holds each of its
scripts' output from bash 5.2 (and from dash for the POSIX ones), and
checks it on the build host and at every boot test.

Started as *sh* (its link _/bin/sh_), it is in POSIX mode (see *POSIX
MODE*); started as *rnukesh*, it is restricted (see *RESTRICTED MODE*).

# OPTIONS

The one-letter options are those of *set*, with these too:

*-c* _command_
	Run _command_. The next argument is _$0_, and those after it the
	positional parameters.

*-s*
	Read commands from standard input; the arguments are the positional
	parameters.

*-i*
	Be interactive, even if standard input and standard error are not a
	terminal. A shell is interactive anyway when they are, and it was
	given no script and no *-c*.

*-l*, *--login*
	Be a login shell, as when _argv[0]_ starts with *-*.

*-r*, *--restricted*
	Be restricted.

*-O* _shopt_, *+O* _shopt_
	Turn a *shopt* option on or off.

*-n*
	Read the commands and check them, but run none: *nukesh -n script*
	reports each syntax error with its file and line, and exits with 2 if
	there was one.

*-x*
	Write each command to standard error before running it, after
	*PS4*, which is *+ ${BASH_SOURCE:-$0}:${LINENO}: * outside POSIX mode,
	so that each line names its file and line.

*--posix*
	POSIX mode.

*--norc*, *--noprofile*, *--rcfile* _file_ (also *--init-file*)
	Change which start-up files are read (see *STARTING*).

*--noediting*
	No line editor; read lines as they come.

*--verbose*
	As *-v*.

*--version*, *--help*
	Say the version, or how to use it, and exit.

# STARTING

A login shell reads _/etc/profile_, then the first of
_~/.nukesh_profile_, _~/.nukesh_login_ and _~/.profile_ that exists (only
_~/.profile_ in POSIX mode). When it ends it reads _~/.nukesh_logout_.

An interactive shell that is not a login shell reads _/etc/nukeshrc_ and
_~/.nukeshrc_, or the file *--rcfile* names; *--norc* reads none. In POSIX
mode it reads the file *$ENV* names instead.

A shell that runs a script or *-c* reads the file *$NUKESH_ENV* names,
outside POSIX mode. None of these is read when the shell's real and
effective user differ.

Functions come in the environment as bash puts them there,
*BASH_FUNC_*_name_*%%=() { ... }*, and *export -f* puts them there that
way, so that functions pass between bash and Nukesh. A value is taken as
a function only if it is one definition and nothing more. POSIX mode
takes none.

# SCRIPTS

A script is read a line at a time and each command runs before the
lines after it are read, so a command that reads the same input, such
as *read* in a script on a pipe or *cat* in *sh <<EOF*, gets what comes
after it. A file is read in blocks and its offset put back to the end of
the command before each command runs.

A syntax error in a script ends it with status 2. An error in an
expansion, an assignment to a read-only variable, or a special
builtin's error, ends a shell that is not interactive with status 1, as
in bash (dash gives 2).

A file that is a directory, or whose first line has a NUL byte, is not
run: status 126. One that is not found: 127.

# INTERACTIVE USE

## The line editor

When standard input and standard error are a terminal and *TERM* is set
and not *dumb*, lines are typed in a line editor (reedline): Emacs keys
by default, or vi keys after *set -o vi*. Tab completes, showing a menu
of the choices; the up and down arrows go through the history, and
Ctrl-R searches it; a command that is not finished, such as an open
quote, an *if* without its *fi* or a line ending in a backslash, goes on
to the next line with *PS2*. Pasted text is taken as typed, without
running each line as it comes.

With *TERM=dumb*, or no terminal, lines are read as they come, with the
terminal's own editing.

Colors follow *NO_COLOR*: when it is set, the shell writes no color.

## Prompts

*PS1* is the prompt, *PS2* the one for more of a command, *PS0* is
written after a command is read and before it runs, and *PROMPT_COMMAND*
(a string or an array) runs before each prompt. Prompts take bash's
escapes (*\\u*, *\\h*, *\\w*, *\\W*, *\\$*, *\\t*, *\\j*, *\\[*...*\\]* and the
others) and then parameter expansion.

## History

Commands are kept in the history and saved in *$HISTFILE*,
_~/.nukesh_history_ by default, made readable only by its user. Each
entry keeps its time, and a command of several lines stays one entry.
Several shells can share the file: each adds only its own commands, under
a lock, and the file is cut to *HISTFILESIZE* entries by writing a new
one in its place. *HISTSIZE*, *HISTCONTROL* (*ignorespace*, *ignoredups*,
*ignoreboth*, *erasedups*), *HISTIGNORE* and *HISTTIMEFORMAT* work as in
bash, as do *history*, *fc*, and history expansion (*!!*, *!$*, *!*_n_,
*^old^new*), which is off in POSIX mode.

## Completion

Tab completes command names, file names, variables after *$*, user
names after *~* and the like, and what *complete* says for a command's
arguments: words, functions (with *COMP_WORDS*, *COMP_CWORD*,
*COMP_LINE*, *COMP_POINT* and *COMPREPLY*), commands, patterns and
filters, as bash's *complete*, *compgen* and *compopt* do.

## Job control

An interactive shell has job control (*set -m*): each pipeline is a job
in a process group of its own, Ctrl-Z stops the one in the foreground,
and *jobs*, *fg*, *bg*, *kill %*_n_, *wait*, *disown* and *suspend* work
as in bash. The shell reports each background job that ends or stops
before the next prompt. Ctrl-C drops the line being typed, with status
130.

Ctrl-D at an empty line ends the shell, unless *IGNOREEOF* says how many
times it must come first, or *set -o ignoreeof* is on (ten times). A
shell with stopped jobs warns once before it ends. *TMOUT* ends a shell
left waiting at its prompt for that many seconds.

When the shell gets *SIGHUP*, it sends it on to its jobs (and *SIGCONT*
to those stopped), except those *disown -h* marked, then ends;
*shopt -s huponexit* does the same when a login shell exits.

# POSIX MODE

As *sh* or with *--posix* or *set -o posix*, the shell does what POSIX
says where bash does otherwise. *echo* takes escapes (*xpg_echo*) and
no options, *alias* lists aliases without *alias*, *shift* past the
end is an error that ends the shell, *PS4* is *+ *, process substitution
and *name[i]=* assignments are not taken, history expansion is off, and
_~/.profile_ and *$ENV* are the start-up files.

# RESTRICTED MODE

As *rnukesh*, with *-r* or *set -r* (which cannot be undone), the shell
refuses to: change directory; set *PATH*, *SHELL*, *ENV*, *BASH_ENV* or
*HISTFILE*; run a command whose name has a */*; redirect output to a file;
*source* a file by a path with a */*; replace itself with *exec*; give
*hash -p*; or leave restricted mode.
The start-up files are read before it is restricted.

# BUILTINS

Each builtin is described by *help* _name_. They are bash's: *.*, *:*,
*[*, *alias*, *bg*, *break*, *builtin*, *caller*, *cd*, *command*,
*compgen*, *complete*, *compopt*, *continue*, *declare*, *dirs*, *disown*,
*echo*, *enable*, *eval*, *exec*, *exit*, *export*, *false*, *fc*, *fg*,
*getopts*, *hash*, *help*, *history*, *jobs*, *kill*, *let*, *local*,
*logout*, *mapfile*, *popd*, *printf*, *pushd*, *pwd*, *read*, *readarray*,
*readonly*, *return*, *set*, *shift*, *shopt*, *source*, *suspend*,
*test*, *times*, *trap*, *true*, *type*, *typeset*, *ulimit*, *umask*,
*unalias*, *unset* and *wait*.

# VARIABLES

Besides POSIX's (*HOME*, *IFS*, *PATH*, *PS1*, *PS2*, *PS4*, *ENV*,
*LINENO*, *OPTIND*, *OPTARG*, *PPID*, *PWD*, *OLDPWD* and the rest), the
shell keeps bash's: *RANDOM*, *SRANDOM*, *SECONDS*, *EPOCHSECONDS*,
*EPOCHREALTIME*, *BASHPID*, *BASH_SUBSHELL*, *BASH_SOURCE*, *BASH_LINENO*,
*FUNCNAME*, *BASH_REMATCH*, *PIPESTATUS*, *SHLVL*, *UID*, *EUID*,
*HOSTNAME*, *REPLY*, *COLUMNS* and *LINES* (with *checkwinsize*), and the
history and completion variables above. *NUKESH_VERSION* is its version.
*BASH_VERSION* is not set, so that scripts can tell the shells apart.

# EXIT STATUS

The status of the last command; 2 for a syntax error or a builtin used
wrongly; 126 for a file that cannot be run, 127 for one not found;
128 + _n_ for a command signal _n_ ended.

# FILES

_/system/bin/nukesh_, and its links _/system/bin/sh_ and
_/system/bin/rnukesh_ (_/bin_ is _/system/bin_).

_/etc/profile_, _/etc/nukeshrc_, _~/.nukesh_profile_, _~/.nukesh_login_,
_~/.profile_, _~/.nukeshrc_, _~/.nukesh_logout_: the start-up files.

_~/.nukesh_history_: the history.

# STANDARDS

POSIX.1-2024, XCU Shell Command Language, in POSIX mode. bash 5.2 for
the rest.

# SEE ALSO

*radix-posix*(7), *spark*(8)

The Bash Reference Manual, for what each bash feature does.
