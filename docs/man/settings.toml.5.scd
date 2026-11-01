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
	In a git repository, the right of the prompt shows the branch, or
	the tag or commit when HEAD is detached, and an operation under way
	(*REBASING 2/5*, *MERGING*); with *git-dirty*, also how far ahead
	(*↑2*) and behind (*↓1*) its upstream the branch is, and the files
	staged (*+3*), changed (*~2*), untracked (*?1*) and in conflict
	(*!1 conflict*), and the stashes (*≡1*), from *git status* run in the
	background: the prompt waits for it 150 ms at most and otherwise
	shows what it found the last time. On the console, whose font lacks
	some of these signs, they are *git*, *^*, *v* and *stash* (ADR 0288).
	The default is *true*.

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

*[git]* *abbreviations =* *true* | *false*, *guardrails =* *true* | *false*, *reminders =* *true* | *false*, *status-wait =* _milliseconds_
	*abbreviations*: oh-my-zsh's git abbreviations, so that *gst* grows
	into *git status*, *gco* into *git checkout* and *gp* into *git
	push* (*abbr* lists them); yours of the same name win. *guardrails*:
	before *git commit* runs, a note when HEAD is detached, as the
	commit would be on no branch, and when it goes straight to the main
	branch of a repository with a remote. *reminders*: on entering a
	repository, once a session, a note of stashes a month old (ADR
	0288). *status-wait*: how long the prompt waits for *git status*
	to count a repository's files; past it, the prompt shows the
	last count it found there. The defaults are *false*, *true*,
	*true* and *500*.

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
