findmnt(8) "Dukovo" "Dukovo Manual"

; SPDX-FileCopyrightText: 2026 The Dukovo contributors
; SPDX-License-Identifier: Apache-2.0 OR MIT

# NAME

findmnt - the mounts, or the one sought

# SYNOPSIS

*findmnt* [*-bDflkn*] [*-o* [*+*]_column_,...] [*-t* [*no*]_kind_,...] [*-S* _source_] [*-T* _path_] [_source_ | _target_]

# DESCRIPTION

findmnt lists the mounts in _/processes/mounts_ (*radix-proc*(5)) as
util-linux's findmnt does: by default as a tree, each mount beneath the
one its target is on, with its target, its source, its kind and its
options. A mount from a disk or partition has its path in _/devices_ as
its source, and a FAT filesystem is *vfat*, as libmount names them.

```
$ findmnt
TARGET          SOURCE        FSTYPE OPTIONS
/               /devices/vda2 btrfs  rw
├─/processes    proc          proc   rw
└─/system/boot  /devices/vda1 vfat   rw
```

A _source_ or _target_ named, or *-t*, *-S* or *-T*, shows only the
mounts that match, as a list.

# OPTIONS

*-o*, *--output* _column_,...
	Show these columns, or, after a *+*, these as well: *TARGET*,
	*SOURCE*, *FSTYPE*, *OPTIONS* (*rw* or *ro*), *LABEL*, *UUID*,
	*SIZE*, *USED*, *AVAIL* and *USE%*.

*-D*, *--df*
	Show the space of each, as *df*(1) does: *SOURCE*, *FSTYPE*, *SIZE*,
	*USED*, *AVAIL*, *USE%* and *TARGET*.

*-t*, *--types* _kind_,...
	Only mounts of these kinds; after *no*, only mounts of other kinds.

*-S*, *--source* _source_
	Only mounts from _source_, as _vda1_ or _/devices/vda1_.

*-T*, *--target* _path_
	The mount that _path_ is on.

*-f*, *--first-only*
	The first mount found alone.

*-b*, *--bytes*
	Sizes in bytes.

*-l*, *--list*
	A list, not a tree.

*-n*, *--noheadings*
	No line of headings.

*-k*, *--kernel*
	Taken: the kernel's table is the only one, as there is no fstab.

# EXIT STATUS

0 if a mount was shown; 1 if none was, or for a mistake in the command
line.

# SEE ALSO

*lsblk*(8), *blkid*(8), *df*(1), *radix-proc*(5), *systools*(1)
