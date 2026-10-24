chown(1) "Dukovo" "Dukovo Manual"

; SPDX-FileCopyrightText: 2026 The Dukovo contributors
; SPDX-License-Identifier: Apache-2.0 OR MIT

# NAME

chown, chgrp - change a file's owner and group

# SYNOPSIS

*chown* [_option_ ...] [_owner_][*:*[_group_]] _file_ ...

*chgrp* [_option_ ...] _group_ _file_ ...

*chown* | *chgrp* [_option_ ...] *--reference*=_rfile_ _file_ ...

# DESCRIPTION

chown changes the owner of each file, and its group if one follows a
*:*, as GNU chown does; *owner:* is the owner and the owner's login
group, and *:group* only the group. *owner.group* is taken too, with a
warning, when no user has that whole name. chgrp changes only the
group. Users and groups are named by name or number.

A symbolic link is changed through, its target getting the owner,
unless *-h* is given or *-R* walks without *-H* or *-L*: then the link
itself is changed. With *-v* each file gets a line in GNU's words, such
as *changed ownership of 'f' from root to nobody* or *ownership of 'f'
retained as root:root*.

Until users come, in Phase 13 of the roadmap, Radix's files have no
owners to change: the C library's *chown*(2) checks that the file is
there and changes nothing (*radix-posix*(7)). Every user and group but
root's is unknown, so naming another fails as it would on Linux for a
user who does not exist. These tools still read their arguments as
Linux's do, so that scripts that use them run.

# OPTIONS

*-R*, *--recursive*
	Change the directories' contents too, each directory after what is
	in it.

*-H*
	With *-R*, follow a symbolic link to a directory named on the
	command line, and change through links found inside.

*-L*
	With *-R*, follow every symbolic link to a directory, and change
	through every link.

*-P*
	With *-R*, follow no link and change links themselves; the default.
	*--dereference* with it is refused.

*-h*, *--no-dereference*
	Change a symbolic link, not what it points to.

*--dereference*
	Change what a symbolic link points to, as by default.

*-v*, *--verbose*; *-c*, *--changes*; *-f*, *--silent*, *--quiet*
	As for *chmod*(1).

*--reference*=_rfile_
	Give each file _rfile_'s owner and group (chgrp: its group).

*--preserve-root* and *--no-preserve-root* are taken and change
nothing.

# SEE ALSO

*chmod*(1), *systools*(1), *radix-posix*(7)
