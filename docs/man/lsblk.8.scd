lsblk(8) "Dukovo" "Dukovo Manual"

; SPDX-FileCopyrightText: 2026 The Dukovo contributors
; SPDX-License-Identifier: Apache-2.0 OR MIT

# NAME

lsblk - the disks and their partitions

# SYNOPSIS

*lsblk* [*-bdflnp*] [*-o* [*+*]_column_,...] [_device_...]

# DESCRIPTION

lsblk lists each disk the kernel has (_/kernel/block_,
*radix-sysfs*(5)) with its partitions beneath it, as util-linux's lsblk
does: by default its name, size, whether it is read-only, its type and
where it is mounted (_/processes/mounts_, *radix-proc*(5)), or *[SWAP]*
for a swap area in use. A _device_ named, as _sda_ or _/devices/sda_,
shows a disk with its partitions, or a partition alone. Radix gives
disks no device numbers, so there is no *MAJ:MIN* column, and knows of
no removable disks yet, so there is no *RM*.

```
$ lsblk
NAME    SIZE RO TYPE MOUNTPOINTS
vda     256M  0 disk
├─vda1   64M  0 part /system/boot
└─vda2  191M  0 part /
```

The filesystem columns come from what *blkid*(8) finds on each device,
and the space from *statvfs*(3) on where it is mounted.

# OPTIONS

*-f*, *--fs*
	Show the filesystems: *NAME*, *FSTYPE*, *FSVER*, *LABEL*, *UUID*,
	*FSAVAIL*, *FSUSE%* and *MOUNTPOINTS*.

*-o*, *--output* _column_,...
	Show these columns, or, after a *+*, these as well: *NAME*, *KNAME*,
	*PATH*, *PKNAME* (a partition's disk), *SIZE*, *RO*, *TYPE*,
	*MOUNTPOINT*, *MOUNTPOINTS*, *FSTYPE*, *FSVER*, *LABEL*, *UUID*,
	*FSAVAIL*, *FSSIZE*, *FSUSED*, *FSUSE%*, *PTUUID*, *PTTYPE*,
	*PARTUUID*, *PARTLABEL*, *PARTN*, *START*, *MODEL*, *SERIAL*, *REV*,
	*ROTA*, *LOG-SEC*, *PHY-SEC*, *MIN-IO*, *OPT-IO*, *DISC-GRAN* and
	*DISC-MAX*.

*-b*, *--bytes*
	Sizes in bytes.

*-d*, *--nodeps*
	Disks only, without their partitions.

*-l*, *--list*
	A list, not a tree.

*-n*, *--noheadings*
	No line of headings.

*-p*, *--paths*
	Names as paths in _/devices_.

*-a*, *--all*
	Taken, as every disk is shown already.

# EXIT STATUS

0; 1 if no device was shown or for a mistake in the command line; 32 if
a _device_ named is no disk or partition, as util-linux's lsblk.

# SEE ALSO

*blkid*(8), *findmnt*(8), *df*(1), *radix-sysfs*(5), *systools*(1)
