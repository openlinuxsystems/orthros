settings.toml(5) "Dukovo" "Dukovo Manual"

; SPDX-FileCopyrightText: 2026 The Dukovo contributors
; SPDX-License-Identifier: Apache-2.0 OR MIT

# NAME

settings.toml - the interactive shell's settings

# SYNOPSIS

_/system/settings/nukesh/settings.toml_

_~/.config/nukesh/settings.toml_

# DESCRIPTION

*nukesh*(1) reads these files when it starts as an interactive shell,
before its startup files (ADR 0264). The first is the system's, which
says every key and its default; the second is a person's own, in which
only the keys it names change. A file that is not there changes
nothing. The changes show in the next shell started. The colours are
in *colors.toml*(5).

The file is TOML. A mistake never stops the shell: the shell says what
it is when it starts, and the key keeps its default.

# KEYS

*[files]* *show-hidden =* *true* | *false*
	Show hidden files: names that start with a dot, and those a
	folder's _.hidden_ file lists, such as _/etc_ and _/dev_ in _/_
	(ADR 0113). *ls* is *ls -A*, *\** and the other patterns match
	names that start with a dot (*shopt -s dotglob*), and Tab completes
	both kinds. The startup files run after, so they may still change
	any of the three. The default is *false*.

*[notify]* *enabled =* *true* | *false*, *after =* _seconds_
	When a command typed at the prompt runs for *after* seconds or
	more, it ends with a notification (*notifyd*(8)) that names it,
	says how it ended and how long it took. *consoled*(8) does not show
	it while the shell's own console is on the screen, so it shows only
	when you have gone to another console (ADR 0263). Editors, pagers,
	*ssh* and other programs you work in send none. The defaults are
	*true* and 30.

*[prompt]* *git-dirty =* *true* | *false*
	After the folder's git branch at the right of the prompt, a *\**
	when the work tree has changes not committed (*git status*, given
	100 ms at most). The default is *true*.

*[nebix]* *offer =* *true* | *false*
	After a command fails, the empty line offers Alt-E, which says what
	went wrong (ADR 0268). The default is *true*.

*[suggest]* *next =* *true* | *false*
	On an empty line, the command you usually run next, faded, from the
	command record; Nebix ranks it when it runs (ADR 0280). → takes it;
	nothing runs until Enter. The default is *true*.

*[accessibility]* *screen-reader =* *true* | *false*
	No colours, faded hints or right prompt, and a prompt of words
	(*~/src, failed 1, admin #*) for screen readers (ADR 0269). The
	default is *false*.

*[terminal]* *marks =* *true* | *false*, *links =* *true* | *false*
	*marks* writes OSC 133 marks around each prompt, the line typed
	and each command's output, so terminals can jump between commands
	and select one's output; *links* gives *ls* *--hyperlink=auto*, so
	the file names it shows are links a terminal can open (ADR 0282).
	The defaults are *true*.

*[history]* *skip-secrets =* *true* | *false*
	Commands holding a password, a token or a URL's password are not
	kept in the history or the command record (ADR 0281). The default
	is *true*.

*[commands]* *offer-install =* *true* | *false*
	A command that is not installed but that a bundle in the store's
	catalogue has is named after "command not found", and the empty
	line's hint is its *bundle install* command (ADR 0285). The default
	is *true*.

*[folders]* *project-files =* *true* | *false*, *resume =* *true* | *false*
	*project-files*: a trusted _.nukesh.toml_ sets variables and
	abbreviations while you are in its folder (*nukesh*(1), FOLDER
	FILES). *resume*: a console's first shell, starting at home, goes
	to the folder the last shell on that console was in (ADR 0286).
	The defaults are *true*.

# EXAMPLES

Hidden files everywhere, and a notice after ten seconds, in
_~/.config/nukesh/settings.toml_:

```
[files]
show-hidden = true

[notify]
after = 10
```

# FILES

_/system/settings/nukesh/settings.toml_
	The system's settings. A fresh copy is
	_/system/resources/default-settings/nukesh/settings.toml_.

# SEE ALSO

*nukesh*(1), *colors.toml*(5), *notifyd*(8), *consoled*(8)
