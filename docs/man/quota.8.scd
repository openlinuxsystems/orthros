quota(8) "Dukovo" "Dukovo Manual"

; SPDX-FileCopyrightText: 2026 The Dukovo contributors
; SPDX-License-Identifier: Apache-2.0 OR MIT

# NAME

quota - turn a Btrfs filesystem's simple quotas on and off, show and limit them

# SYNOPSIS

*quota enable* [*-s*|*--simple*] _path_

*quota disable* _path_

*quota show* [*-r*] [*-e*] [*--raw*] _path_

*quota limit* [*-e*] _size_|*none* _qgroupid_ _path_

# DESCRIPTION

quota keeps the simple quotas of the Btrfs filesystem that holds
_path_, as Linux's *btrfs quota enable --simple*, *btrfs quota
disable*, *btrfs qgroup show* and *btrfs qgroup limit* do (ADR 0170).

With simple quotas on, each subvolume has a quota group, _0/_*id*, that
counts the bytes of every extent the subvolume wrote, data and tree
blocks, for as long as the extent lives, whoever shares it since: a
snapshot's group counts only the blocks the snapshot itself changed,
and what a removed subvolume's snapshots still use stays counted to
the removed subvolume. Only what is written once they are on counts.
Its referenced and exclusive bytes are the same.

*enable*
	Turn simple quotas on: each subvolume gets its group, at zero.
	Doing it again does nothing. A filesystem with Linux's full quota
	groups on is refused; Dukovo marks their counts inconsistent when
	it writes, and Linux's *btrfs quota rescan* counts them again.

*disable*
	Turn quotas off: the quota groups go, with their limits.

*show*
	List the quota groups, with what each counts.

*limit*
	Set a group's limit on its referenced bytes, or with *-e* on its
	exclusive bytes; *none* takes the limit away. _size_ is in bytes,
	or with a unit of *k*, *m*, *g*, *t*, *p* or *e*, in powers of
	1024. A write that would take the group past it fails with
	*EDQUOT*, as a write short of room fails with *ENOSPC*; removing
	files never does. _qgroupid_ is the group's, _0/_*id*, or the
	subvolume's ID.

Turning quotas on and off and setting a limit need the control right on
_path_'s directory (*radix-directory*(2)), which quota takes from the
program's root, as *replace*(8) does; *show* needs the read right.

# OPTIONS

*-s*, *--simple*
	Simple quotas, the only ones quota keeps.

*-r*
	With *show*, add the limit on referenced bytes.

*-e*
	With *show*, add the limit on exclusive bytes; with *limit*, set
	it.

*--raw*
	With *show*, print bytes rather than sizes with units.

# OUTPUT

*show* prints a table, as *btrfs qgroup show* does:

```
Qgroupid Referenced Exclusive
-------- ---------- ---------
0/5        16.00KiB  16.00KiB
0/256       1.00MiB   1.00MiB
```

# DIFFERENCES FROM LINUX

- Linux's full quota groups, which count what each subvolume shares
  with others, are not kept: *enable* turns on simple quotas alone, as
  with Linux's *--simple*, and there is no *rescan*.
- Groups above level 0 are counted when the filesystem has them, but
  quota makes none: *btrfs qgroup create* and *assign* are not there.
- *limit* needs the group's ID; Linux also takes a subvolume's path.
- *show* has no *Path* column, and none of *btrfs qgroup show*'s
  sorting and column options.

# EXIT STATUS

0 when it was done; 1 when it could not be, as on a filesystem that is
not Btrfs or a Btrfs whose quotas are off, or for a mistake in the
command line.

# SEE ALSO

*balance*(8), *device*(8), *subvolume*(8), *radix-directory*(2)
