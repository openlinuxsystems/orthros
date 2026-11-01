welcome.toml(5) "Dukovo" "Dukovo Manual"

; SPDX-FileCopyrightText: 2026 The Dukovo contributors
; SPDX-License-Identifier: Apache-2.0 OR MIT

# NAME

welcome.toml - the welcome banner above the sign-in prompt

# SYNOPSIS

_/system/settings/console/welcome.toml_

# DESCRIPTION

*consoled*(8) shows a banner above the sign-in prompt on each console
(ADR 0261): Duke and the name drawn in plain ASCII, so that every
console font has each character, then the tagline and the system's
name and version, read as the system runs. Where the banner shows,
_/system/settings/issue_'s text does not. A screen too small for Duke
gets the name alone, or only the two lines of words. The banner is not
shown on consoles that start root's shell with no sign-in.

consoled reads the file when it starts; *service consoled restart*
reads it again. A key that is wrong is logged and keeps its default.

# KEYS

*show =* *"every-boot"* | *"first-boot"* | *"never"*
	When the banner shows: each time the sign-in prompt comes, only
	during the system's first boot (_/system/state/consoled/welcome-shown_
	records that it was), or never. The default is *"every-boot"*.

*art =* *"duke"* | *"small"* | *"none"* | _path_
	Duke with the name beside him, the name alone, no art, or the text
	of a file of your own. The default is *"duke"*.

*tagline =* _text_ | *false*
	The words beside Duke. The default is *"A bright future. A human
	future."*

*version =* _text_ | *false*
	The system's name and version, with issue's escapes: *\\s* the
	system's name, *\\r* its version, *\\n* the computer's name. The
	default is *"\\\\s Server \\\\r"*.

*colors =* *true* | *false*
	Duke in yellow, the name in bold white and the version dimmed, or
	the console's own colour.

# FILES

_/system/settings/console/welcome.toml_
	The settings. A fresh copy is
	_/system/resources/default-settings/console/welcome.toml_.

# SEE ALSO

*consoled*(8), *colors.toml*(5)
