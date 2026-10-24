coreutils(1) "Dukovo" "Dukovo Manual"

; SPDX-FileCopyrightText: 2026 The Dukovo contributors
; SPDX-License-Identifier: Apache-2.0 OR MIT

# NAME

coreutils - the core utilities, in one program

# SYNOPSIS

_utility_ [_argument_ ...]

*coreutils* _utility_ [_argument_ ...]

*coreutils* *--list* | *--version* | *--help*

# DESCRIPTION

coreutils is the uutils project's core utilities, written in Rust,
built for Radix as one program: *ls*(1), *cp*(1), *cat*(1), *sort*(1),
*date*(1), *test*(1) and the rest of the utilities POSIX names and GNU's
core utilities have, with GNU's options. Each is a link to
_/system/bin/coreutils_ by its name, and coreutils runs the utility its
name says; *coreutils* _utility_ runs one by name. Each utility has its
own manual page.

# OPTIONS

*--list*
	List the utilities, one per line.

*-V*, *--version*
	Print coreutils' version.

*-h*, *--help*
	Print how to run it, and the utilities.

# EXIT STATUS

The utility's, or 1 when there is no utility by that name.

# FILES

_/system/bin/coreutils_, and a link by each utility's name.

# SEE ALSO

*systools*(1), for the administration tools, and *radix-posix*(7).

uutils coreutils, by the uutils contributors, under the MIT licence:
https://github.com/uutils/coreutils
