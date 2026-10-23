which(1) "Dukovo" "Dukovo Manual"

; SPDX-FileCopyrightText: 2026 The Dukovo contributors
; SPDX-License-Identifier: Apache-2.0 OR MIT

# NAME

which - where a command is

# SYNOPSIS

*which* [*-as*] _name_ ...

# DESCRIPTION

which prints, for each _name_, the first executable file of that name in
the directories of *PATH*, as Debian's which does; a name with a */* is
looked for as it is. Nukesh's *type* also knows its built-in commands
and aliases.

# OPTIONS

*-a*, *--all*
	Every match, not only the first.

*-s*
	Print nothing: only the exit status says.

# EXIT STATUS

0 if every name was found, 1 if one was not or none was given, 2 for
an option it does not know, as debianutils' which has it.

# SEE ALSO

*nukesh*(1), *systools*(1)
