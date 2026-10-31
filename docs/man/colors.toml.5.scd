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

*[prompt]* *user*, *at*, *host*, *path*, *symbol*, *error*, *admin*
	While *PS1* is left at its default, the prompt is
	_name_*@*_host_ _folder_ *›*, with a *✗* _status_ badge after a
	command that failed and an *admin* badge, and *#*, while the shell
	is root's. *error* and *admin* are the badges' styles.

*[line]* *command*, *unknown*, *keyword*, *option*, *string*, *variable*, *operator*, *comment*, *hint*
	The command line as it is typed: a command that runs something,
	one that does not (before Enter), reserved words, options, quoted
	text, variables and substitutions, operators and redirections,
	comments, and the hint from the history.

*[suggest]* *history =* *true* | *false*, *did-you-mean =* *true* | *false*
	*history*: a command from the history that starts with what was
	typed shows faded after the cursor; → or End takes it. It shows
	only with colours. *did-you-mean*: after "command not found" at
	the prompt, the closest command's name, within two letters changed.
	Neither ever runs a command.

*[ls]* *directory*, *program*, *link*, *broken-link*, *archive*, *pipe*, *socket*, *device*
	*LS_COLORS*, and an alias *ls --color=auto*.

*[grep]* *match*, *file*, *line*, *separator*
	*GREP_COLORS*, and an alias *grep --color=auto*.

*[diff]* *added*, *removed*, *header*, *hunk*
	An alias *diff --color=auto --palette=*_..._.

*[ip]* *name*, *up*, *down*, *address*, *address6*, *mac*
	*IP_COLORS*, and an alias *ip -color=auto* (*ip*(8)).

An alias or variable that is already there when the shell starts is
kept; the startup files may change or remove them (*unalias ls*).

# EXAMPLES

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

*nukesh*(1), *ip*(8), *welcome.toml*(5)
