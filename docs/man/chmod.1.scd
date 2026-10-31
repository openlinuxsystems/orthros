chmod(1) "Dukovo" "Dukovo Manual"

; SPDX-FileCopyrightText: 2026 The Dukovo contributors
; SPDX-License-Identifier: Apache-2.0 OR MIT

# NAME

chmod - change a file's mode

# SYNOPSIS

*chmod* [_option_ ...] _mode_ _file_ ...

*chmod* [_option_ ...] *--reference*=_rfile_ _file_ ...

# DESCRIPTION

chmod changes the permission bits of each file, as GNU chmod does, to
_mode_: an octal number (*755*), or POSIX's symbolic clauses separated
by commas, each of who (*u*, *g*, *o*, *a*), an operator (*+*, *-*, *=*)
and permissions (*r*, *w*, *x*, *X*, *s*, *t*) or another class's
(*u*, *g*, *o*): *u+x*, *go-w*, *a=rX*, *g=u*. *X* is *x* for a
directory, or for a file that the clauses before it left executable by
someone. Without who, a clause changes what the umask lets it, and when
the umask keeps bits that _mode_ takes away (*-w* with umask 022),
chmod says so, as *f: new permissions are r--rw-rw-, not r--r--r--*,
and fails.

A symbolic link named on the command line is changed through: its
target gets the mode. One found while recursing is left alone, and
*-v* says *neither symbolic link 'd/l' nor referent has been changed*.

tmpfs, Btrfs and ext4 keep the mode (ADR 0213). FAT and exFAT keep
none: as on Linux, a mode of permission bits alone is taken there and
changes nothing, and one with set-user-ID, set-group-ID or sticky fails
with *Operation not permitted*.
Only a file's owner changes its mode, unless the capability it is
reached through has the bypass right, and a mode with the set-group-ID
bit loses it when the file's group is not one of the caller's, as on
Linux (*radix-posix*(7), ADR 0214).

# OPTIONS

*-R*, *--recursive*
	Change the directories' contents too, each directory before what
	is in it. A symbolic link to a directory named on the command line
	is followed into.

*-L*
	With *-R*, change through every symbolic link and follow those to
	directories.

*-P*
	With *-R*, follow no symbolic link into its directory.

*-v*, *--verbose*
	Say what was done to each file.

*-c*, *--changes*
	Say so only for the files whose mode changes.

*-f*, *--silent*, *--quiet*
	Do not complain about files that cannot be changed.

*--reference*=_rfile_
	Give each file _rfile_'s mode.

*--preserve-root* and *--no-preserve-root* are taken and change nothing.

# EXIT STATUS

0 if every file was changed; 1 if one could not be, or for a mistake in
the command line, such as a bad mode, as GNU chmod has it.

# SEE ALSO

*chown*(1), *chgrp*(1), *systools*(1), *radix-posix*(7)
