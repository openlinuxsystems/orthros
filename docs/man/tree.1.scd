tree(1) "Dukovo" "Dukovo Manual"

; SPDX-FileCopyrightText: 2026 The Dukovo contributors
; SPDX-License-Identifier: Apache-2.0 OR MIT

# NAME

tree - draw what directories hold as a tree

# SYNOPSIS

*tree* [_option_ ...] [_directory_ ...]

# DESCRIPTION

tree draws each _directory_ (the working directory by default) and
everything under it as a tree, as Steve Baker's tree does, then counts
the directories and files it drew:

	.
	├── docs
	│   └── man
	└── README.md

	2 directories, 1 file

Entries are sorted by name, byte by byte, and those whose names start
with a dot are left out. The lines are drawn in UTF-8. A symbolic link
shows where it points, after *->*; one to a directory is counted as a
directory, but only followed with *-l*, and a link back to a directory
already drawn is marked *[recursive, not followed]*. A directory that
cannot be read is marked *[error opening dir]*.

It was written for Dukovo and has none of tree's code.

# LISTING

*-a*
	Every entry, also those whose names start with a dot.

*-d*
	Only directories.

*-l*
	Follow symbolic links to directories.

*-f*
	Each entry's whole path, not only its name.

*-x*
	Stay on the file system of each _directory_.

*-L* _level_
	Go at most _level_ directories deep.

*-P* _pattern_
	Only the files that match _pattern_, wildcards (*\**, *?*, *[...]*)
	separated by *|*; directories are still drawn.

*-I* _pattern_
	Leave out the entries that match _pattern_. It may be given more
	than once.

*--ignore-case*
	*-P* and *-I* ignore case.

*--matchdirs*
	*-P* matches directories' names too; what a matching one holds is
	drawn whole.

*--prune*
	Leave out directories with nothing drawn under them.

*--filelimit* _n_
	Do not open directories of more than _n_ entries.

*--noreport*
	No count at the end.

*-o* _file_
	Write to _file_.

# FILES

*-p*
	Each entry's type and permissions, as *ls -l* shows them.

*-u*, *-g*
	Each entry's owner, group.

*-s*
	Each entry's size in bytes.

*-h*, *--si*
	Sizes with units, of 1024 or (*--si*) 1000.

*--du*
	A directory's size is that of all it holds, and the count ends
	with the total.

*-D*
	The time each entry was last modified (with *-c*, its status
	changed): the day and time, or the day and year if it is more than
	six months old. *--timefmt* _format_ sets its format, as
	*strftime*(3)'s, and implies *-D*.

*-F*
	A */* after directories, *\** after programs, *@* after links, *|*
	after pipes, *=* after sockets.

*--inodes*, *--device*
	Each entry's file number, device.

*-q*, *-N*
	Control characters in names as *?*, or as they are; by default
	they are octal escapes, *\\001*.

*-Q*
	Names in double quotes.

*-i*
	No tree lines or indents: a list.

*--charset* _charset_
	Draw the lines in ASCII (*|--*, *`--*) unless _charset_ is UTF-8.
	Without it they are UTF-8 when the locale's characters are: when
	the first of *LC_ALL*, *LC_CTYPE* and *LANG* that is set names
	UTF-8, as Dukovo's *C.UTF-8* does.

*--metafirst*
	What *-p*, *-s* and the others show first on each line, before the
	indents.

# SORTING

*-v*
	By version: numbers in names compared as numbers.

*-t*
	By the time each was last modified, oldest first.

*-c*
	By the time each one's status last changed, oldest first; *-D*
	shows that time.

*-U*
	Unsorted, in the directory's order.

*--sort* _how_
	By *name*, *version*, *size* (largest first), *mtime*, *ctime* or
	*none*.

*-r*
	In reverse.

*--dirsfirst*, *--filesfirst*
	Directories before files, or after.

# OUTPUT

*-J*
	The tree as JSON.

*-X*
	The tree as XML.

*-n*, *-C*, *-A*, *-S*, *-R* and *--info* are taken and change nothing:
tree does not colour.

# EXIT STATUS

0, or 2 if a _directory_ could not be found.

# SEE ALSO

*column*(1), *systools*(1)
