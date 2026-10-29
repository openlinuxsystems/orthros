replace(8) "Dukovo" "Dukovo Manual"

; SPDX-FileCopyrightText: 2026 The Dukovo contributors
; SPDX-License-Identifier: Apache-2.0 OR MIT

# NAME

replace - put a new disk in the place of one of a Btrfs filesystem's

# SYNOPSIS

*replace start* [*-B*] [*-f*] [*-r*] _srcdev_|_devid_ _targetdev_ _path_

# DESCRIPTION

replace puts the disk _targetdev_ in the place of one of the disks of
the Btrfs filesystem that holds _path_, and rebuilds onto it, as
Linux's *btrfs replace start -B* does. The disk replaced is named by
its device ID, _devid_, which a missing disk can only be named by, or
by its device, _srcdev_ (ADR 0155).

The new disk takes the old one's device ID and UUID. Every copy that
the old disk held is written to it from the copies on the other disks,
by a scrub (*scrub*(8)); what is written meanwhile goes to it as well.
Once it holds everything, it is given its superblocks, and the old
disk, if it is there, has its superblocks wiped, so that it is not
taken for one of the filesystem's again. The filesystem stays in use
while it runs.

Until then the new disk has no superblock: if the machine stops in
the middle, the next mount finds the old disk missing still, and the
replace is started again.

When it is done, replace prints when it started and finished, and how
many sectors no copy could mend.

Replacing a disk needs the control right on _path_'s directory
(*radix-directory*(2)), which replace takes from the program's root, as
*fstrim*(8) does: root's programs have it; others are denied.

# OPTIONS

*-B*
	Wait for the rebuild to finish. replace always waits, so this is
	what it does without it too.

*-f*
	Write over _targetdev_ even if it holds a Btrfs filesystem.

*-r*
	Read from the old disk only when no other copy is there. replace
	never reads from it, so this is what it does without it too.

# DIFFERENCES FROM LINUX

- *status* and *cancel* are not there, and replace never runs in the
  background: it returns when the rebuild is done.
- Linux copies from the old disk while it is there; replace rebuilds
  from the other disks' copies only, so it refuses a filesystem whose
  chunks have no copy on another disk, such as one of a single disk, or
  with RAID0 or DUP chunks on the disk.
- A replace stopped in the middle starts again from the beginning, as
  Linux's resumes.
- Only a Btrfs filesystem on _targetdev_ needs *-f*.

# EXIT STATUS

0 when the new disk holds everything; 1 when the replace could not be
done, when sectors were left that no copy could mend (the new disk then
gets no superblock), or for a mistake in the command line.

# SEE ALSO

*radix-directory*(2), *radix-files*(7), *scrub*(8), *systools*(1)
