scrub(8) "Dukovo" "Dukovo Manual"

; SPDX-FileCopyrightText: 2026 The Dukovo contributors
; SPDX-License-Identifier: Apache-2.0 OR MIT

# NAME

scrub - read and check every block of a Btrfs filesystem

# SYNOPSIS

*scrub start* [*-B*] [*-r*] [*-q*] _path_

# DESCRIPTION

scrub reads every block of the Btrfs filesystem that holds _path_, as
Linux's *btrfs scrub start -B* does, with the same summary. Every copy
of every tree block and data sector the last commit holds is read from
the disk, past every cache, and checked against its checksum; a tree
block is checked as a read checks it, and the superblock's copies too.
A bad copy with a good one beside it, as the two copies of DUP
metadata are, is mended: the good copy is written over it (ADR 0150).

The filesystem stays in use while it runs; what is written after the
scrub has passed its place is left to the next one. The counts are a
sector at a time, as Linux counts them: a bad copy of a 16 KiB tree
block is four errors.

The summary names the filesystem, when the scrub started, how long it
took, how much it read and how fast, and what it found: _no errors
found_, or each kind of error with its count, then how many sectors
were corrected, how many no copy could mend (uncorrectable), and how
many were not checked (unverified).

# OPTIONS

*-B*
	Wait for the scrub to finish, and print its summary. scrub always
	waits, so this is what it does without it too.

*-r*
	Only read: count what is bad, and write nothing. A read-only
	mount, such as */system*, can be scrubbed only so.

*-q*
	Print nothing but errors.

# DIFFERENCES FROM LINUX

- *status*, *cancel* and *resume* are not there yet, and scrub never
  runs in the background: it returns when the scrub is done.
- A filesystem with more than one device is scrubbed as one, with one
  summary; Linux prints one for each device with *-d*.
- *-c* and *-n* (the scrub's I/O priority) and the speed limits are
  not there yet.

# EXIT STATUS

0 when the scrub found no errors, or mended every one; 1 when it could
not be done, or for a mistake in the command line; 3 when it found
errors that no copy mends, as Linux's *btrfs scrub* exits.

# SEE ALSO

*radix-directory*(2), *radix-files*(7), *subvolume*(8), *systools*(1)
