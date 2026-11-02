mkfs-fat(8) "Dukovo" "Dukovo Manual"

; SPDX-FileCopyrightText: 2026 The Dukovo contributors
; SPDX-License-Identifier: Apache-2.0 OR MIT

# NAME

mkfs-fat, mkfs.fat, mkfs.vfat - make a FAT filesystem

# SYNOPSIS

*mkfs-fat* [*-F* 12|16|32] [*-n* _label_] [*-i* _volume-id_] [*-s* _sectors_]
[*-f* _tables_] [*-r* _entries_] [*-h* _hidden_] [*-S* _sector-size_] [*-I*]
[*-C*] [*-v*] [*--invariant*] _device_ [_kib_]

# DESCRIPTION

mkfs-fat makes a FAT12, FAT16 or FAT32 filesystem on _device_, a disk or
partition in _/devices_ or a file, with the options of dosfstools'
*mkfs.fat*, by whose names it runs too (ADR 0302). It follows
Microsoft's FAT specification: unless *-F* says, a volume of up to 4.1 MiB
is FAT12, one under 512 MiB FAT16, and a larger one FAT32, with the
cluster sizes the specification gives for each size, which Windows'
*format* gives too. The data area starts on a whole cluster, and a
FAT32 volume has its FSInfo sector and a backup of its boot sector in
sectors 1 and 6.

A volume on a partition says where the partition starts (its hidden
sectors), as Windows wants. Its boot sector prints, if booted, that it
is not a boot disk. A volume ID is random, unless *-i* or *--invariant*
gives it; the time of the label's entry is now, or *SOURCE_DATE_EPOCH*.

mkfs-fat refuses a device in use (*rdisk*(8) says what uses one), and a
whole disk with partitions, whose table the filesystem would replace,
unless *-I*. Once it has written, the links of _/devices/disk_ follow
the new label and UUID (ADR 0303). It prints what it made:

```
mkfs-fat: made FAT32, 130812 clusters of 4 KiB, serial 1A2B-3C4D, label EFI on /devices/vda1
```

# OPTIONS

*-F* _bits_
	Make FAT12, FAT16 or FAT32, if the volume's size fits it.

*-n* _label_
	The volume's label: up to 11 characters, in upper case.

*-i* _volume-id_
	The volume ID, in hexadecimal (*1234abcd* or *1234-ABCD*).

*-s* _sectors_
	Sectors in a cluster: a power of two.

*-f* _tables_
	How many allocation tables: 1 or 2 (the default).

*-r* _entries_
	The entries of a FAT12 or FAT16 root directory (512 by default).

*-h* _hidden_
	The hidden sectors the boot sector says: where the partition starts
	by default, 0 on a whole disk.

*-S* _sector-size_
	The sector size of a file; a disk has its own.

*-I*
	Make the filesystem on a whole disk with partitions.

*-C*
	Make the file _device_, _kib_ KiB long, first.

*-v*
	Say where the tables and the data area are.

*--invariant*
	Make the same volume each time: the volume ID 1234-ABCD, and the
	time *SOURCE_DATE_EPOCH* or 1980's first second.

_kib_
	Use only the first _kib_ KiB of the device.

# DIFFERENCES FROM LINUX

- *-c* and *-l* (bad blocks) are not there, nor the Atari variant
  (*-A*), *--mbr*, *-a* or *-b*.
- Labels are ASCII in upper case: *--codepage* is not there.

# EXIT STATUS

0 when the filesystem was made; 1 when it was not, and for a mistake in
the command line.

# SEE ALSO

*fsck-fat*(8), *mkfs-exfat*(8), *mkfs*(8), *rdisk*(8), *blkid*(8)
