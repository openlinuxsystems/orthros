fstrim(8) "Dukovo" "Dukovo Manual"

; SPDX-FileCopyrightText: 2026 The Dukovo contributors
; SPDX-License-Identifier: Apache-2.0 OR MIT

# NAME

fstrim - discard what a mounted filesystem does not use

# SYNOPSIS

*fstrim* [*-v*] [*-m* _minimum_] _mountpoint_

*fstrim* *-a* [*-vq*] [*-m* _minimum_]

# DESCRIPTION

fstrim has the filesystem mounted at _mountpoint_, or the one a path in
it is on, discard on its disks the bytes it does not use, as
util-linux's fstrim does with FITRIM, through *radix-directory*(2)'s
*trim* (ADR 0158). A solid state disk told which of its blocks hold
nothing erases them ahead of time, and writes faster and wears less.

On Btrfs, it discards the free space of each block group on every copy
of it, then each disk's bytes no chunk takes, a batch at a time while
changes wait. A disk's first MiB and the copies of its superblock are
never discarded. Space freed by changes not yet committed is trimmed by
the next trim. Other filesystems do not trim.

```
$ fstrim -v /
/: 6.2 GiB (6657343488 bytes) trimmed
```

Trimming needs the control right on the directory (*radix-directory*(2)),
which the C library's *open* does not ask for: fstrim opens the
directory again from the program's root with it. Root's programs have
it; others are denied.

# OPTIONS

*-v*, *--verbose*
	Say how many bytes were discarded, each copy counted.

*-m*, *--minimum* _size_
	Discard only free ranges of at least _size_ bytes, or with a suffix
	*K*, *M*, *G* or *T* (*KiB*...; *KB*... for powers of 1000). Larger
	ranges make the trim quicker and leave the small ones.

*-a*, *--all*, *-A*, *--fstab*
	Trim every filesystem mounted writable from a disk, once each, and
	with *-v* say which disk each is on. Those that do not trim are told
	of on standard error, unless *-q*.

*-q*, *--quiet-unsupported*
	With *-a*, say nothing of a filesystem that does not trim.

# EXIT STATUS

0 if the trim was done; 1 if it failed, or for a mistake in the command
line. With *-a*, 32 if every trim failed and 64 if some did, as
util-linux's.

# SEE ALSO

*iostat*(1), *findmnt*(8), *scrub*(8), *systools*(1)
