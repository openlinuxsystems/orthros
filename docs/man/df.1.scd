df(1) "Dukovo" "Dukovo Manual"

; SPDX-FileCopyrightText: 2026 The Dukovo contributors
; SPDX-License-Identifier: Apache-2.0 OR MIT

# NAME

df - the space of each mounted filesystem

# SYNOPSIS

*df* [_option_ ...] [_file_ ...]

# DESCRIPTION

df shows, for each mounted filesystem, as GNU df does, what it was
mounted from, its size, the space used and available, the share used
and where it is mounted, from _/processes/mounts_ (*radix-proc*(5)) and
*statvfs*(3). Sizes are in blocks of 1 KiB unless an option says
otherwise. Given files, it shows the filesystems they are on.

Without *-a* it leaves out the filesystems that hold no files of their
own: _/processes_, _/kernel_ and _/devices_.

# OPTIONS

*-a*, *--all*
	Every filesystem.

*-h*, *--human-readable*
	Sizes with a unit, in powers of 1024 (*1.5G*), rounded up, so that
	none reads less than it is.

*-H*, *--si*
	Sizes with a unit, in powers of 1000.

*-k*; *-m*
	Blocks of 1 KiB; of 1 MiB.

*-B*, *--block-size* _size_
	Blocks of _size_: a number, a unit, or both. *K* and *KiB* are
	1024 bytes and *KB* or *kB* 1000, and likewise *M*, *G*, *T*, *P* and
	*E*, in either case. A unit given without a number follows every
	size too (*-BM* shows *258020M*). The header names the block size in
	the powers that divide it more evenly (*4K-blocks*, *3kB-blocks*).

*-i*, *--inodes*
	Files (inodes) in place of blocks.

*-T*, *--print-type*
	A Type column: the filesystem's kind.

*-t*, *--type* _kind_
	Only filesystems of this kind; may be given more than once.

*-x*, *--exclude-type* _kind_
	Leave out filesystems of this kind.

*--total*
	A last line, *total*, adding them up.

*-P*, *--portability*
	POSIX's format: 1024-blocks and Capacity, one line each.

*-l*, *--local*, *--sync* and *--no-sync* are taken and change nothing.

# EXAMPLES

	df -hT
	df -h /home

# SEE ALSO

*free*(1), *systools*(1), *radix-proc*(5), *radix-files*(7)
