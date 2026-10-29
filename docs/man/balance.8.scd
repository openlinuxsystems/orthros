balance(8) "Dukovo" "Dukovo Manual"

; SPDX-FileCopyrightText: 2026 The Dukovo contributors
; SPDX-License-Identifier: Apache-2.0 OR MIT

# NAME

balance - move a Btrfs filesystem's chunks, converting them to another profile

# SYNOPSIS

*balance start* [*-d*[_filters_]] [*-m*[_filters_]] [*-s*[_filters_]] [*-f*] [*--full-balance*] _path_

# DESCRIPTION

balance moves chunks of the Btrfs filesystem that holds _path_ to new
chunks, as Linux's *btrfs balance start* does (ADR 0159): those of data
with *-d*, of metadata with *-m*, and of the chunk tree with *-s*; all
of them with none of those. The new chunks go on the disks with the
most room, so a balance spreads what the disks hold over a disk
*device*(8) added; with *convert*, they take another profile, which
converts the filesystem.

Each chunk is moved alone: what it holds is copied to other chunks, a
batch at a time with a commit after each, and the chunk goes once it is
empty. The filesystem stays in use meanwhile. Data is copied a sector
at a time from a copy whose checksum matches, so a balance mends what a
disk lost as a scrub would; a sector no copy holds stops it, with the
chunks moved so far moved.

*-m* without *-s* moves the chunk tree's chunks too, as metadata goes.

When it is done, balance prints how many chunks it moved out of how
many there were.

Balancing needs the control right on _path_'s directory
(*radix-directory*(2)), which balance takes from the program's root, as
*replace*(8) does.

# FILTERS

Filters follow *-d*, *-m* or *-s* with no space, separated by commas,
as in *-dconvert=raid1,soft*.

*convert=*_profile_
	Make the new chunks _profile_: *single*, *dup*, *raid0*, *raid1*,
	*raid10*, *raid1c3* or *raid1c4*. A profile that needs more disks
	than are there is refused.

*soft*
	With *convert*, leave the chunks already of that profile, so a
	convert that stopped can go on.

# OPTIONS

*-f*, *--force*
	Allow *-s*.

*--full-balance*
	Move every chunk with no warning.

*-v*
	Say more. balance says the same without it.

# DIFFERENCES FROM LINUX

- *pause*, *cancel*, *resume* and *status* are not there, and balance
  never runs in the background: it returns when it is done.
- Only the *convert* and *soft* filters are there.
- Linux moves a block that snapshots share once and keeps it shared;
  balance gives each snapshot its own copy of the blocks it copies, so
  a filesystem with many snapshots takes more room after it.
- A full balance warns and goes on at once, where Linux waits ten
  seconds first.
- RAID5 and RAID6 are refused, as Radix does not write them.

# EXIT STATUS

0 when every chunk named was moved; 1 when one could not be, or for a
mistake in the command line.

# SEE ALSO

*device*(8), *radix-directory*(2), *replace*(8), *scrub*(8), *systools*(1)
