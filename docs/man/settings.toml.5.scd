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
