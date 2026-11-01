nukesh(1) "Dukovo" "Dukovo Manual"

; SPDX-FileCopyrightText: 2026 The Dukovo contributors
; SPDX-License-Identifier: Apache-2.0 OR MIT

# NAME

nukesh, sh, bash, rnukesh - Dukovo's shell

# SYNOPSIS

*nukesh* [_long options_] [*-abCefhimnuvxBEHPT*] [*-o* _option_] [*-O* _shopt_] [_script_ [_argument_ ...]]

*nukesh* [_options_] *-c* _command_ [_name_ [_argument_ ...]]

*nukesh* [_options_] *-s* [_argument_ ...]

*sh* ...

*bash* ...

*rnukesh* ...

# DESCRIPTION

Nukesh is the shell of Dukovo: the one people type at, and _/bin/sh_,
which runs the system's scripts and the commands of *system*(3),
*popen*(3) and *wordexp*(3). It is written in Rust for Dukovo.

It is the POSIX shell (XCU, Shell Command Language), with the parts of
bash that scripts use: arrays and associative arrays, *[[ ]]*, *(( ))*,
*${x//a/b}* and the other parameter expansions, brace expansion,
process substitution, here-strings, coprocesses, *local*, *declare*, *mapfile*,
*printf -v*, *set -o pipefail*, extended patterns, *shopt*, the *ERR*,
*DEBUG* and *RETURN* traps, *BASH_REMATCH*, *PIPESTATUS*, *FUNCNAME*,
programmable completion and the rest. A bash script runs unchanged, and
gives what bash gives: the test suite of Nukesh holds each of its
scripts' output from bash 5.2 (and from dash for the POSIX ones), and
checks it on the build host and at every boot test.

Started as *sh* (its link _/bin/sh_), it is in POSIX mode (see *POSIX
MODE*); started as *rnukesh*, it is restricted (see *RESTRICTED MODE*).
Started as *bash* (_/bin/bash_), it is itself, so that scripts written
for bash, whose first line names it, run unchanged.

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

*--colors* [*--check*] [_file_...]
	Shows every key of *colors.toml*(5) in its colour, from the
	system's file and one's own or from the _files_ named, and says
	what is wrong in them; with *--check*, only checks them, and fails
	if anything is wrong. It must be the first argument.

*--private*
	Keep nothing of this session: no history file, command record,
	folder ranks, notices or resumed folder (PRIVATE SESSIONS).

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

A syntax error in a script ends it with status 2. Other errors are
bash's. An error in arithmetic, a bad substitution, an assignment to a
read-only variable, an indirect expansion that names no variable, or a
pattern that matches nothing under *failglob* abandons the command it is
in: the shell goes on with the next command it reads, with status 1, and
a subshell ends. In *((*, *let*, *for ((* and the comparisons of numbers
of *[[*, an error in arithmetic only fails the command, with status 1.
*${name:?}*, and an unset variable under *set -u*, end a shell that is
not interactive, with status 1. In POSIX mode, each of these errors, and
a special builtin's (an unknown option to *set*, say), ends a shell that
is not interactive with status 2, as in dash.

A file that is a directory, or whose first line has a NUL byte, is not
run: status 126. One that is not found: 127.

# INTERACTIVE USE

## The line editor

When standard input and standard error are a terminal and *TERM* is set
and not *dumb*, lines are typed in a line editor (reedline): Emacs keys
by default, or vi keys after *set -o vi*. Tab completes, showing a menu
of the choices; the up and down arrows go through this shell's history,
and Ctrl-R opens the picker (COMMAND RECORD); a command that is not finished, such as an open
quote, an *if* without its *fi* or a line ending in a backslash, goes on
to the next line with *PS2*. Pasted text is taken as typed, without
running each line as it comes: a paste of several lines waits in the
line for Enter, so it can be read, changed or dropped with Ctrl-C first.

Alt-S puts *elevate* before the line, or takes it off again. Ctrl-X
Ctrl-E (and Ctrl-O) opens the line in *VISUAL*, *EDITOR* or *nano*, and
what is saved comes back as the line. Alt-E explains the line (EXPLAIN).

Alt-← and Alt-→ on an empty line go back to the folder this shell was in
before, and forward again, as a file manager's arrows do; on a line with
text they move a word.

Tab forgives: when nothing starts with the word, it offers names that
start with it ignoring case, then those one typo away; and in a path,
*...* is *../..* and *....* is *../../..* (ADR 0283). A finished line
the shell cannot read shows the token it is near red and underlined
before Enter, and why, faded: *← `fi` cannot go there* (ADR 0284).

On an empty line, the hint is what comes next (ADR 0280): after a
command that failed, the command fixed (a mistyped name, *cd* to a
folder a typo away, an admin-only command after *elevate*, or
*bundle install* for a missing command a bundle has), then *Alt+E
explains what went wrong*; otherwise the command you usually run after
the last one. → takes it; nothing runs until Enter.

With *TERM=dumb*, or no terminal, lines are read as they come, with the
terminal's own editing.

## Explain

Alt-E says what the line does, under it, part by part: each program's
line from its manual page, each option's from its completion file, how
the parts are joined and where output goes. What cannot be taken back
comes first, in bold: deleting, overwriting, formatting, changing
permissions or owners, running as the administrator, running a script
from the internet, and sending data off the computer. On an empty line
it says why the last command failed. *explain* _command_ prints the
same, and *explain* alone explains each line of its input. Nothing is
run, passwords and tokens are masked, and when Nebix runs it is asked
too, on this computer only (ADR 0268).

## Private sessions

*nukesh --private* keeps nothing: no history file, command record,
folder ranks, notices or resumed folder, and the prompt says *private*
(ADR 0267).

## Folder files

A _.nukesh.toml_ in a folder or above it sets *[env]* variables and
*[abbr]* abbreviations while the shell is in that folder or under it,
and they are undone when it leaves. It applies only after *trust*,
which keeps a copy of it; a changed file must be trusted again, and
*trust -r* stops it (ADR 0286). It runs nothing.

```
[env]
DATABASE_URL = "postgres://localhost/site"
[abbr]
t = "make test"
```

## Terminals and screen readers

The prompt and each command's output are marked for terminals that
jump between commands (OSC 133), and *ls* names files as links (OSC 8)
(ADR 0282). With *[accessibility] screen-reader* in *settings.toml*(5)
the shell draws nothing a reader would trip on (ADR 0269). When the
terminal hangs up at the prompt, the shell hangs up its jobs, saves its
history and exits (ADR 0267).

## Colours

The line is coloured as it is typed: commands, reserved words, options,
quoted text, variables, operators and comments; a command that runs
nothing is red and underlined before Enter. A command from the history
that starts with what was typed shows faded after the cursor, and → or
End takes it; while an option is typed, the rest of its name and what it
does show faded after it, from its completion file. After "command not
found" at the prompt, the shell names
the closest command it knows: "Did you mean service?". While *PS1* is
left at its default, the prompt is _name_*@*_host_ _folder_ *$*, with a
*✗* _status_ badge after a command that failed and an *admin* badge,
and *#*, while the shell is root's; at the right of the line, how long
the last command took when it took three seconds or more, the jobs, and
the folder's git branch. An interactive shell also gives
*ls*, *grep*, *diff* and *ip* colours on a terminal, through aliases
and *LS_COLORS*, *GREP_COLORS* and *IP_COLORS*.

*colors.toml*(5) has every colour and switch: the system's file, then
one's own, _~/.config/nukesh/colors.toml_. Only the terminal's 16
standard colours are used, and themes for high contrast, light
backgrounds and colour blindness change them all at once. With
*NO_COLOR* set, a dumb terminal or *enabled = false*, the shell writes
no colour, and the hint is not shown.

*settings.toml*(5) has the rest: hidden files shown everywhere, the
notice that a long command ended while you were on another console,
and the switches for each feature below.

## Command record

Every command typed at an interactive shell is written at once to
_~/.local/state/nukesh/commands_, readable only by its user, with when
it ran, its exit status, how long it took and its folder (ADR 0262).
Every shell of a person's shares it: a command typed in one is found in
the others at their next prompt. A command after a space, and commands
while *set +o history*, are left out. The history (*HISTFILE*, *fc*,
*history*, the up arrow) is unchanged.

Ctrl-R opens the picker: each command of the record once, matched
loosely as you type (*gst* finds *git status*), the best match first,
then those run in this folder, then the newest; each shows how it ended
(*✗ 2*), how long it took, when and where. Enter takes it into the line;
Ctrl-R again goes down. The hint after the cursor comes from the record
too, one run in this folder first.

## Folders

*z* _word_ ... goes to the folder most used lately whose path has the
words in order, the last in its last part, as *zoxide* and *z.sh* do:
after *cd ~/projects/site* once, *z site* goes there from anywhere.
*z -l* _word_ ... lists the matches with their scores. Each folder an
interactive shell goes into is counted in _~/.local/state/nukesh/folders_;
those not used for long fade from it.

## Abbreviations

*abbr* _name_ _expansion_ ... makes _name_, typed as a command, grow into
its expansion when Space or Enter follows it, as in fish: after
*abbr gs git status*, *gs* and Space shows *git status *. What runs is
what was seen, and the history keeps it whole. *abbr* lists them and
*abbr -r* _name_ removes one; put them in _~/.nukeshrc_. Unlike an
alias, an abbreviation is only in the line editor: scripts never see
one.

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
filters, as bash's *complete*, *compgen* and *compopt* do. A file name
that a directory's _.hidden_ file lists, as the root's lists the links
with Unix's names (*radix-files*(7)), completes only once it is typed
whole, so _/de_ completes to _/devices/_.

A program that *complete* says nothing for completes from its completion
file, if it has one, which every program in _/system/commands_ does: a word
that starts with *-* completes to the program's options, shown with what
each does, and the word after an option completes as that option's
argument, a file, a directory, a user and the like; *--name=* and an
argument joined to its option, as in *7zz*(1)'s *-o*{_directory_},
complete too. The operands complete as the file says, and as files when
it does not.

## Completion files

A program's completion file is the file of its name in the first of the
directories of *NUKESH_COMPLETION_PATH* that has one or, without that
variable, in _$XDG_DATA_HOME/nukesh/completions_ (by default
_~/.local/share/nukesh/completions_), so that a user's own come first,
then in _/system/resources/nukesh/completions_. The system's are written from
each program's manual page when the system is built. A shell reads each
file once.

Each line holds fields separated by tabs; lines of any other kind, such
as comments after *#*, are left out:

*option* _forms_ _argument_ _description_
	An option: its forms, separated by spaces (*-i --ignore-case*);
	what its argument completes as, empty if it takes none, or after a
	*+* if the argument is joined to the option (*+directory*); and what
	it does, which the menu shows.

*operand* _n_ _what_
	What the _n_-th operand, from 1, completes as, or every operand not
	named, for 0.

An argument or an operand completes as one of *complete*'s actions
(*file*, *directory*, *command*, *user*, *group*, *service*, *signal*,
*hostname* and the rest of *-A*'s), as *value* or *none* for nothing
(anything can be typed), or, for an operand, as *words* and a list of
words separated by spaces.

## Job control

An interactive shell has job control (*set -m*): each pipeline is a job
in a process group of its own, Ctrl-Z stops the one in the foreground,
and *jobs*, *fg*, *bg*, *kill %*_n_, *wait*, *disown* and *suspend* work
as in bash. The shell reports each background job that ends or stops
before the next prompt. Ctrl-C drops the line being typed, with status
130. *set -m* in a script gives its jobs process groups of their own
too, so that stop signals reach them, and, when the script runs in the
foreground of the terminal on its standard error, its foreground jobs
have the terminal while they run, as in bash.

Ctrl-D at an empty line ends the shell, unless *IGNOREEOF* says how many
times it must come first, or *set -o ignoreeof* is on (ten times). A
shell with stopped jobs warns once before it ends. *TMOUT* ends a shell
left waiting at its prompt for that many seconds.

When the shell gets *SIGHUP*, it sends it on to its jobs (and *SIGCONT*
to those stopped), except those *disown -h* marked, then ends;
*shopt -s huponexit* does the same when a login shell exits.

# POSIX MODE

As *sh* or with *--posix* or *set -o posix*, the shell does what POSIX
says, as dash does, where bash does otherwise. *echo* takes escapes
(*xpg_echo*) and no options, *alias* lists aliases without *alias*,
*shift* past the end is an error that ends the shell, *PS4* is *+ *,
process substitution, *[[* and *name[i]=* assignments are not taken,
history expansion is off, and _~/.profile_ and *$ENV* are the start-up
files. Aliases are expanded in scripts too, *.\** matches _._ and _.._,
and a command's redirections are expanded before its assignments.
In a signal's trap, *exit* with no number leaves the status from before
the trap, as in the EXIT trap, not *$?* as bash does.
*${!...}* is *$!* with an operator, as in *${!#x}*, not an indirect
expansion. The errors that *SCRIPTS* lists end the shell with status 2.
bash's own variables, those whose names begin with *BASH*, are not set,
so that a script does not take the shell for bash. Started in POSIX
mode, the shell has brace expansion off, as dash has; *set -o posix* in
a running shell leaves it on and turns *inherit_errexit* on, as in bash.

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
*unalias*, *unset* and *wait*, and Nukesh's own *abbr*, *explain*,
*trust* and *z*. *shopt* takes each of bash 5.2's options;
those for what the shell does not do (the *compat* levels, *cdspell*,
*dirspell*, *direxpand*, *gnu_errfmt*, *hostcomplete*, *mailwarn*,
*no_empty_cmd_completion*) change nothing, and *restricted_shell* cannot
be changed. With *extdebug*, *declare -F* _name_ tells where the function
was defined.

# VARIABLES

Besides POSIX's (*HOME*, *IFS*, *PATH*, *PS1*, *PS2*, *PS4*, *ENV*,
*LINENO*, *OPTIND*, *OPTARG*, *PPID*, *PWD*, *OLDPWD* and the rest), the
shell keeps bash's: *RANDOM*, *SRANDOM*, *SECONDS*, *EPOCHSECONDS*,
*EPOCHREALTIME*, *BASHPID*, *BASH_SUBSHELL*, *BASH_SOURCE*, *BASH_LINENO*,
*FUNCNAME*, *BASH_REMATCH*, *PIPESTATUS*, *SHLVL*, *UID*, *EUID*,
*HOSTNAME*, *REPLY*, *COLUMNS* and *LINES* (with *checkwinsize*), and the
history and completion variables above. *NUKESH_VERSION* is its version.
*NUKESH_COMPLETION_PATH* names the directories of completion files.
*BASH_VERSION* and *BASH_VERSINFO* are bash 5.2's, as scripts check them
before they use a feature of bash (*declare -A*, say), or refuse to run
without bash; *NUKESH_VERSION* tells the shells apart. *BASH* is the
shell's own file, for scripts that run *"$BASH" -c*. In POSIX mode none
of the three is set.

# LIMITS

Commands, *$(...)*, *${...}* and the parentheses and *!* of *[[ ]]* nest
at most 1,000 deep; deeper text is a syntax error, *too deeply nested*,
where bash would run out of stack. Functions call each other at most
2,000 deep, unless *FUNCNEST* says otherwise. Arithmetic's parentheses
nest at most 1,024 deep. Extended patterns opened more than 64 deep, and
braces past the 256th in a word, are left as plain text.

Text is UTF-8 in every locale: *${#x}*, *${x:i:n}* and *?* count
characters, as bash's do in a UTF-8 locale; in the C locale bash counts
bytes.

Matching a pattern takes time that grows with the length of the text
times that of the pattern, never exponentially: *${x//+(ab)/x}* on
40 KB of text takes milliseconds, where bash takes minutes. Where bash's
matcher has known quirks, Nukesh does what its manual says:
a pattern ending in *\\\** matches a literal star in *${x/pattern/text}*
too, and an empty text matches *?(a)* and *!(a)* there as it does in
*[[ ]]*.

A coprocess started as *coproc cat* shows as that in *jobs*, where
bash's *coproc COPROC cat* would read back as a command named COPROC.
Each coprocess's descriptors close, and its variables go, once the shell
has seen it end; bash cleans up only the last one started.

# EXIT STATUS

The status of the last command; 2 for a syntax error or a builtin used
wrongly; 126 for a file that cannot be run, 127 for one not found;
128 + _n_ for a command signal _n_ ended.

# FILES

_/system/commands/nukesh_, and its links _/system/commands/sh_, _/system/commands/bash_
and _/system/commands/rnukesh_ (_/bin_ leads to _/system/commands_).

_/etc/profile_, _/etc/nukeshrc_, _~/.nukesh_profile_, _~/.nukesh_login_,
_~/.profile_, _~/.nukeshrc_, _~/.nukesh_logout_: the start-up files.

_~/.nukesh_history_: the history.

_~/.local/state/nukesh/commands_ and _~/.local/state/nukesh/folders_: the
command record and the folders *z* knows; _~/.local/state/nukesh/where_
and _~/.local/state/nukesh/trusted/_: each console's last folder, and the
folder files trusted.

_/system/settings/nukesh/colors.toml_, _/system/settings/nukesh/settings.toml_
and the same under _~/.config/nukesh_: colours and settings.

_/system/resources/nukesh/completions_ and
_~/.local/share/nukesh/completions_: completion files.

# STANDARDS

POSIX.1-2024, XCU Shell Command Language, in POSIX mode. bash 5.2 for
the rest.

# SEE ALSO

*man*(1), *makewhatis*(8), *radix-posix*(7), *spark*(8), *colors.toml*(5), *settings.toml*(5)

The Bash Reference Manual, for what each bash feature does.
