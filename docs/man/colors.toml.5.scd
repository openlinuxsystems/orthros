colors.toml(5) "Dukovo" "Dukovo Manual"

; SPDX-FileCopyrightText: 2026 The Dukovo contributors
; SPDX-License-Identifier: Apache-2.0 OR MIT

# NAME

colors.toml - the colours of nukesh, ls, grep, diff and ip

# SYNOPSIS

_/system/settings/nukesh/colors.toml_++
_~/.config/nukesh/colors.toml_

# DESCRIPTION

*nukesh*(1) reads these files when it starts as an interactive shell
(ADR 0258). The first is the system's, which says every key and its
default; the second is a person's own, in which only the keys it names
change, key by key. A file that is not there changes nothing. The
changes show in the next shell started.

Only the terminal's 16 standard colours are offered, so that the
colours follow the terminal's theme, and a colour is never all a thing
says: an unknown command is underlined as well as red, and the prompt's
badges are words.

Colours are off when *NO_COLOR* is set to something (no-color.org), when
the terminal is dumb or the output is not a terminal, and everywhere
with *enabled = false*. *ls*, *grep* and *diff* get *--color=auto*, so
their output into a pipe or a file is plain.

The file is TOML. A mistake never stops the shell: the shell says what
it is when it starts, and the key keeps its default.
*nukesh --colors --check* checks the files; *nukesh --colors* shows
every key in its colour.

# STYLES

A style is a few words in quotes: one of the colours *black*, *red*,
*green*, *yellow*, *blue*, *magenta*, *cyan* and *white*, or the same
with *bright-* before it (*grey* is *bright-black*); then *on* and
another colour for the background; and any of *bold*, *dim*, *italic*,
*underline* and *reverse*. *"none"* is the terminal's own text. For
example *"bold blue"*, *"red underline"*, *"bold bright-white on red"*.

# KEYS

*enabled =* *true* | *false*
	Colours at all. Every section but *[suggest]* takes *enabled* as
	well, for that section alone.

*theme =* *"default"* | *"high-contrast"* | *"light"* | *"colorblind"*
	Where every style starts from (ADR 0265): the defaults below;
	bright and bold, with nothing faded; for a terminal with a light
	background, with no yellow or white text; or blue and yellow in
	place of green and red, for red-green colour blindness. The keys
	a file sets change the theme's, one by one, and a person's file
	may choose another theme than the system's. *nukesh --colors*
	shows the theme's styles.

*[prompt]* *user*, *at*, *host*, *path*, *symbol*, *error*, *admin*, *took*, *jobs*, *branch*, *git-state*, *git-sync*, *git-conflict*, *git-staged*, *git-changed*, *git-untracked*, *git-stash*, *git-clean*
	While *PS1* is left at its default, the prompt is
	_name_*@*_host_ _folder_ *$*, with a *✗* _status_ badge after a
	command that failed and an *admin* badge, and *#*, while the shell
	is root's. *error* and *admin* are the badges' styles. At the
	right of the line (ADR 0262): *took* _time_ after a command that
	took *took-after* seconds or more, how many jobs there are, and
	in a folder of a git repository, its branch and state (ADR 0288):
	*git-state* for an operation under way (*REBASING 2/5*) and a tag or
	commit when HEAD is detached, *git-sync* for how far ahead and
	behind the upstream, *git-conflict*, *git-staged*, *git-changed* and
	*git-untracked* for the files' counts, *git-stash* for the stashes,
	and *git-clean* for the mark of a work tree with nothing changed.

*[prompt]* *took-after =* _seconds_
	How long a command takes before the right prompt says how long.
	The default is 3.

*[line]* *command*, *unknown*, *keyword*, *option*, *string*, *variable*, *operator*, *comment*, *hint*
	The command line as it is typed: a command that runs something,
	one that does not (before Enter), reserved words, options, quoted
	text, variables and substitutions, operators and redirections,
	comments, and the hint from the history.

*[suggest]* *history =* *true* | *false*, *did-you-mean =* *true* | *false*, *options =* *true* | *false*
	*history*: the newest command of the command record (*nukesh*(1),
	COMMAND RECORD) that starts with what was typed, one run in this
	folder first, shows faded after the cursor; → or End takes it.
	*options*: while an option is typed, the rest of its name and what
	it does show faded after it, from the program's completion file;
	→ takes the rest of the name only. Both show only with colours.
	*did-you-mean*: after "command not found" at the prompt, the
	closest command's name, within two letters changed. None ever runs
	a command.

*[ls]* *directory*, *program*, *link*, *broken-link*, *archive*, *pipe*, *socket*, *device*
	*LS_COLORS*, and an alias *ls --color=auto*.

*[grep]* *match*, *file*, *line*, *separator*
	*GREP_COLORS*, and an alias *grep --color=auto*.

*[diff]* *added*, *removed*, *header*, *hunk*, *added-word*, *removed-word*, *moved-added*, *moved-removed*
	An alias *diff --color=auto --palette=*_..._, and git's colours:
	each interactive shell writes _~/.local/state/nukesh/gitconfig_,
	which the system's gitconfig includes, so a user's _~/.gitconfig_
	still wins. It gives git's diffs these colours, *git status* and
	*git branch* the prompt's git colours, and *git diff*, *show* and
	*log* the pager *nukesh --diff-pager*, which marks the words of a
	changed line that changed in *added-word* and *removed-word*
	(*nukesh*(1)). Lines git saw moved are in *moved-added* and
	*moved-removed*. With *enabled = false* here, the file is empty.

*[ip]* *name*, *up*, *down*, *address*, *address6*, *mac*
	*IP_COLORS*, and an alias *ip -color=auto* (*ip*(8)).

An alias or variable that is already there when the shell starts is
kept; the startup files may change or remove them (*unalias ls*).

# EXAMPLES

The colour-blind theme, with added lines in diff cyan:

```
theme = "colorblind"

[diff]
added = "cyan"
```

Commands in bright green and no colours in *ls*, in
_~/.config/nukesh/colors.toml_:

```
[line]
command = "bright-green"

[ls]
enabled = false
```

# FILES

_/system/settings/nukesh/colors.toml_
	The system's colours. A fresh copy is
	_/system/resources/default-settings/nukesh/colors.toml_.

# SEE ALSO

*nukesh*(1), *settings.toml*(5), *ip*(8), *welcome.toml*(5)
