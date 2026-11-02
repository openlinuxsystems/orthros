mkfs-exfat(8) "Dukovo" "Dukovo Manual"

; SPDX-FileCopyrightText: 2026 The Dukovo contributors
; SPDX-License-Identifier: Apache-2.0 OR MIT

# NAME

mkfs-exfat, mkfs.exfat - make an exFAT filesystem

# SYNOPSIS

*mkfs-exfat* [*-L* _label_] [*-c* _cluster-size_] [*--volume-serial* _id_] [*--invariant*] [*-I*] [*-q*] _device_

# DESCRIPTION

mkfs-exfat makes an exFAT filesystem on _device_, a disk or partition in
_/devices_ or a file, with the options of exfatprogs' *mkfs.exfat*, by
whose name it runs too (ADR 0302). It follows Microsoft's exFAT
specification: the main and backup boot regions with their checksums,
the allocation table, and in the cluster heap the allocation bitmap, the
up-case table and the root directory, with the volume's label.

Clusters are 4 KiB on a volume of up to 256 MiB, 32 KiB up to 32 GiB,
and 128 KiB on a larger one, as Windows' *format* makes them. On a
volume of 64 MiB or more, the table and the heap start on a whole MiB.
A volume on a partition says where the partition starts, as Windows
wants. Its serial number is random, unless *--volume-serial* or
*--invariant* gives it.

mkfs-exfat refuses a device in use, and a whole disk with partitions,
unless *-I*. Once it has written, the links of _/devices/disk_ follow
the new label and UUID (ADR 0303). It prints what it made:

```
mkfs-exfat: made exFAT, 1015 clusters of 32 KiB, serial 5E6F-7A8B, label DATA on /devices/vda3
```

# OPTIONS

*-L* _label_, *--volume-label* _label_
	The volume's label: up to 11 characters.

*-c* _cluster-size_, *--cluster-size* _cluster-size_
	The cluster size, in bytes or with *K* or *M*: a power of two, from
	a sector to 32 MiB.

*--volume-serial* _id_
	The volume's serial number, in hexadecimal.

*--invariant*
	Make the same volume each time: the serial number 1234-ABCD.

*-I*
	Make the filesystem on a whole disk with partitions.

*-q*, *--quiet*
	Print nothing but errors.

*-v*, *--verbose*
	Taken, and print no more.

# DIFFERENCES FROM LINUX

- *-f* (zero the whole volume), *-b*, *-U* and *--pack-bitmap* are not
  there.

# EXIT STATUS

0 when the filesystem was made; 1 when it was not, and for a mistake in
the command line.

# SEE ALSO

*mkfs-fat*(8), *mkfs*(8), *rdisk*(8), *blkid*(8)
