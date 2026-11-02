rdisk(8) "Dukovo" "Dukovo Manual"

; SPDX-FileCopyrightText: 2026 The Dukovo contributors
; SPDX-License-Identifier: Apache-2.0 OR MIT

# NAME

rdisk - partition a disk: GPT and MBR, at a prompt or from a layout file

# SYNOPSIS

*rdisk* [*--dry-run*] [*--sector-size* _bytes_] _disk_

*rdisk* *-l* [_disk_ ...]

*rdisk* *--script* _file_ [*--dry-run*] _disk_

*rdisk* *--dump* _disk_

*rdisk* *--reread* _disk_

*rdisk* *--list-types*

# DESCRIPTION

rdisk reads and writes a disk's partition table, a GPT or an MBR, as
fdisk and sgdisk do, with Dukovo's disk engine (ADR 0300), which
*./ob image* uses too. _disk_ is a disk in _/devices_, which may be
named alone (*rdisk vda*), or an image file.

With a _disk_ alone, rdisk asks for commands at a prompt, one letter
each, as fdisk does. The table changes in memory, and nothing is written
until *w*; *q*, or the end of the input, quits and writes nothing. A
disk with no table starts with a new, empty GPT.

	*m*  list the commands++
*p*  print the table++
*F*  list the free space++
*l*  list the partition types++
*n*  add a new partition++
*d*  delete a partition++
*t*  change a partition's type++
*c*  change a partition's name (GPT)++
*u*  change a partition's UUID (GPT)++
*a*  mark a partition bootable, or not (MBR)++
*g*  start a new, empty GPT++
*o*  start a new, empty MBR++
*v*  verify the table++
*w*  write the table, and quit++
*q*  quit without writing

*n* asks for the partition's number, its first sector and its last, as
fdisk does: a first sector as a number of sectors, or a size with its
unit (*1M*); a last sector as a number, or *+*_size_ for the
partition's size (*+512M*, *+2048* sectors), or *-*_size_ for the room
to leave after it. The defaults are the first free number, the first
whole MiB free, and the end of the free space it starts in. A new
partition is a Linux filesystem; *t* changes its type, by an alias,
sgdisk's code, a GUID or a byte (*--list-types*).

The input may come from a pipe:

```
printf 'g\\nn\\n\\n\\n+512M\\nt\\nefi\\nn\\n\\n\\n\\nw\\n' | rdisk vda
```

*--script* applies a layout file instead (*rdisk-layout*(5)): a new
table and its partitions, or partitions added to the table there is and
some taken out. The installer partitions disks this way. *--dump* writes
the disk's table as a layout file, which *--script* makes again.

# WRITING

Before it writes, rdisk refuses a disk in use: one a filesystem is
mounted from, whose swap area is on, that is a disk of a mounted Btrfs,
or whose BitLocker volume is unlocked (radix-block(2)'s *GET_IN_USE*).
It refuses, too, a table that would change or take away a partition in
use. A partition the new table keeps where it was, of the size it was,
may be in use: a partition is added in the free space of the disk the
system runs from.

A GPT's backup is written first, at the disk's end, then its primary
copy and its protective MBR; an MBR over an old GPT clears the GPT's
headers, and a table over a filesystem on the whole disk clears that
filesystem's marks, so that nothing takes the disk for what it was.
*--dry-run* says what it would write, sector by sector, and writes
nothing.

Once the table is written, the kernel reads it again (*REREAD_PARTITIONS*,
ADR 0301): the partitions it adds appear in _/devices_ and _/kernel/block_
straight away, and those it took away go. *volumed*(8) leaves the new
ones unmounted, as they were made, not plugged in, and the links of
_/devices/disk_ follow them (ADR 0303). If the kernel cannot take the new
table, rdisk says why; *rdisk --reread* asks again once nothing uses the
disk.

# OPTIONS

*-l*, *--list*
	Print the tables of the disks named, or of every disk.

*--script* _file_
	Apply the layout _file_, *-* for standard input (*rdisk-layout*(5)).

*--dump*
	Write the disk's table as a layout file, to standard output. A table
	with logical partitions (in an MBR's extended partition) cannot be.

*--reread*
	Ask the kernel to read the disk's table again.

*-n*, *--dry-run*
	Write nothing; say what *w* or the script would write.

*--sector-size* _bytes_
	The sector size of an image file: 512 (the default), 1024, 2048 or
	4096. A disk has its own.

*--list-types*
	List the partition types rdisk knows by name, with sgdisk's code and
	the MBR byte of each.

# PARTITION TYPES

[[ *Alias*
:[ *Type*
|  efi
:  EFI System (EF00, 0xef)
|  bios-boot
:  BIOS boot (EF02), GRUB's for a BIOS boot from a GPT
|  dukovo-root
:  Dukovo root (ADR 0137); on an MBR, Linux (0x83)
|  swap
:  Linux swap (8200, 0x82)
|  linux
:  Linux filesystem (8300, 0x83)
|  windows-data
:  Microsoft basic data (0700, 0x07); FAT, exFAT and NTFS on a GPT
|  fat32, fat16, fat12
:  basic data on a GPT; on an MBR, 0x0c, 0x0e and 0x01

*--list-types* lists the others: linux-root, linux-home, linux-raid,
linux-lvm, windows-reserved and windows-recovery.

# DIFFERENCES FROM LINUX

- New partitions start on a whole MiB, as fdisk's do, and rdisk makes
  no extended or logical partitions: an MBR holds four primary ones, and
  a GPT 128. Logical partitions there are stay as they are.
- A disk with no table starts with a GPT, where fdisk starts an MBR.
- The prompt's letters are fdisk's for what both do; fdisk's expert
  menu is not there.

# EXIT STATUS

0 when the table was written, or nothing was asked to be; 1 when it was
not, when the kernel could not take it, and for a mistake in the
command line.

# SEE ALSO

*rdisk-layout*(5), *mkfs*(8), *lsblk*(8), *blkid*(8), *volumed*(8),
*radix-block*(2)
