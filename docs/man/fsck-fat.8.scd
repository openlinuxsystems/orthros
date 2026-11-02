fsck-fat(8) "Dukovo" "Dukovo Manual"

; SPDX-FileCopyrightText: 2026 The Dukovo contributors
; SPDX-License-Identifier: Apache-2.0 OR MIT

# NAME

fsck-fat, fsck.fat, fsck.vfat - check and mend a FAT filesystem

# SYNOPSIS

*fsck-fat* [*-n* | *-a*] [*-V*] [*-v*] _device_

# DESCRIPTION

fsck-fat checks the FAT12, FAT16 or FAT32 filesystem on _device_, a disk
or partition in _/devices_ or a file, as dosfstools' *fsck.fat* does, by
whose names it runs too (ADR 0302). It reads the boot sector, or its
backup when the first is damaged; compares the allocation tables; walks
every directory, with its long names, and follows every file's chain of
clusters; and finds:

- files whose chains are shorter or longer than their sizes, and chains
  that loop or reach past the volume's end;
- clusters two files share, and directories reached twice;
- clusters taken that no file has, lost in chains of their own;
- long names whose parts or checksums do not match their short names;
- *.* and *..* entries that do not lead where they should;
- a FAT32 volume's FSInfo free count and its backup boot sector;
- the flags Linux and Windows set while a volume is mounted, and leave
  set when it is not unmounted.

With *-n*, the default, fsck-fat changes nothing. With *-a*, it mends
what it finds: a chain is cut to its file's size, or the size to its
chain; a shared cluster goes to the first file and the others are cut;
lost clusters are freed; and the tables, FSInfo and flags are written
anew. fsck-fat prints each problem, with what mending did or would do,
then a summary:

```
/NOTES.TXT: its chain ends after 2 clusters, fewer than its size needs; mending cuts the file to 8192 bytes.
Allocation table 2 differs from table 1.
Nothing was changed; fsck-fat -a mends what can be.
/devices/vda1: 12 files, 130/16354 clusters
```

To mend a volume, unmount it first: fsck-fat refuses one in use.

# OPTIONS

*-n*
	Check, and change nothing: the default.

*-a*, *-p*, *-y*, *-r*
	Mend what is found. *-r* asks nothing: fsck-fat mends as *-a* does.

*-V*
	With *-a*, check the volume again once it is mended.

*-v*
	Say the volume's kind, clusters and label first.

*-w*
	Taken, as every change is written as it is made.

# DIFFERENCES FROM LINUX

- The exit status is *fsck*(8)'s, as e2fsck gives, where dosfstools'
  gives 1 for a volume with problems, mended or not.
- fsck-fat asks nothing: *-r* mends as *-a* does.
- Lost chains are freed; *-f* (save them as files), *-l*, *-d*, *-u* and
  *-t* are not there.

# EXIT STATUS

0
	Nothing is wrong.

1
	Something was wrong, and is mended.

4
	Something is wrong, and is not mended: with *-n*, or what could not
	be.

8
	The volume could not be checked: it cannot be read, or holds no FAT.

16
	A mistake in the command line.

# SEE ALSO

*mkfs-fat*(8), *mkfs*(8), *rdisk*(8)
