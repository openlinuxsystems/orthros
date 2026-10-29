device(8) "Dukovo" "Dukovo Manual"

; SPDX-FileCopyrightText: 2026 The Dukovo contributors
; SPDX-License-Identifier: Apache-2.0 OR MIT

# NAME

device - add disks to a Btrfs filesystem and remove them

# SYNOPSIS

*device add* [*-f*] [*-K*] _disk_... _path_

*device remove* _disk_|_devid_... _path_

# DESCRIPTION

device adds disks to the Btrfs filesystem that holds _path_, or takes
them out of it, as Linux's *btrfs device add* and *btrfs device remove*
do (ADR 0159). The filesystem stays in use meanwhile.

*add* wipes the superblocks a _disk_ held, gives it the next device ID
and a UUID of its own, and makes it one of the filesystem's: the commit
writes it a superblock, and the next mount finds it with the others. It
holds nothing at first. New chunks go on the disks with the most room,
so they go to it from then on; *balance*(8) moves what the others hold
now, and with *convert* spreads copies onto it.

*remove* moves every chunk with a stripe on the disk to new chunks on
the other disks, as *balance*(8) moves them, then takes the disk out:
its device item goes, and its superblocks, if it is there, are wiped. A
disk is named by its device ID, _devid_, which a missing disk can only
be named by, or by its device. The disks left must be enough for every
profile the filesystem uses: two for RAID1, three for RAID1C3, four
for RAID1C4.

Adding or removing a disk needs the control right on _path_'s directory
(*radix-directory*(2)), which device takes from the program's root, as
*replace*(8) does.

# OPTIONS

*-f*, *--force*
	Add a _disk_ even if it holds a Btrfs filesystem.

*-K*, *--nodiscard*
	Do not discard the new disk first. device never does, so this is
	what it does without it too.

# DIFFERENCES FROM LINUX

- *usage*, *scan*, *ready* and *stats* are not there.
- *remove missing* is not there: name a missing disk by its device ID.
- A disk removed has its chunks moved as *balance*(8) moves them, which
  copies what snapshots share once for each of them.
- At most 16 disks can be the filesystem's between two mounts, those
  removed counted.
- Only a Btrfs filesystem on _disk_ needs *-f*.

# EXIT STATUS

0 when every disk was added or removed; 1 when one could not be, or for
a mistake in the command line.

# SEE ALSO

*balance*(8), *radix-directory*(2), *replace*(8), *scrub*(8), *systools*(1)
