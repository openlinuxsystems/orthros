rdisk-layout(5) "Dukovo" "Dukovo Manual"

; SPDX-FileCopyrightText: 2026 The Dukovo contributors
; SPDX-License-Identifier: Apache-2.0 OR MIT

# NAME

rdisk-layout - a partition table in a file, for rdisk --script

# DESCRIPTION

A layout file says what partition table a disk gets, in JSON:
*rdisk --script* applies one, and *rdisk --dump* writes one of a disk's
table (*rdisk*(8)). The installer partitions disks with them, and
*./ob image* makes its images' tables from them (ADR 0300).

```
{
  "table": "gpt",
  "partitions": [
    { "type": "bios-boot", "size": "1MiB" },
    { "type": "efi", "size": "512MiB", "name": "EFI system" },
    { "type": "swap", "size": "8GiB" },
    { "type": "dukovo-root", "size": "rest", "name": "Dukovo root" }
  ]
}
```

The partitions are placed in order, each as rdisk's *n* places one: on
the first whole MiB of free space it fits in, unless *start* says where.
A name rdisk does not know is refused, so that a mistyped one is not
missed.

# THE TABLE

*table*
	*gpt*, or *mbr* (*dos*, as sfdisk calls it).

*uuid*
	The disk's GUID, for a GPT, or its 32-bit signature, for an MBR
	(*0x1234abcd*); a random one if not given.

*keep*
	*true* keeps the disk's table and its partitions, and adds the
	file's in its free space, as the installer's _Use free space_ does.
	The table must be of the kind *table* names. Without it, the file's
	table replaces the disk's.

*delete*
	With *keep*, the partitions to take out first, by number:
	*[3, 4]*.

*partitions*
	The partitions to add, in order.

# A PARTITION

*type*
	What the partition holds: an alias rdisk knows (*efi*, *bios-boot*,
	*dukovo-root*, *swap*, *linux*, *windows-data*; *rdisk --list-types*
	lists them), sgdisk's four-digit code (*ef00*), a type GUID, or an
	MBR type byte (*0x83*).

*size*
	Its size, with a unit: *512MiB*, *8GiB*, *2048s* (sectors); *K*, *M*
	and *G* are KiB, MiB and GiB, and *KB*, *MB* and *GB* thousands. A
	size is rounded up to whole sectors. *rest* takes the rest of the
	free space it starts in.

*start*
	Where it starts, from the disk's start, in whole sectors: *1MiB*,
	*2048s*.

*number*
	Its number in the table; the first free one if not given.

*name*
	Its name, in a GPT: up to 36 characters.

*uuid*
	Its unique GUID, in a GPT; a random one if not given.

*bootable*
	*true* marks it bootable, in an MBR.

*attributes*
	Its attributes, in a GPT: *required*, *no-block-io*,
	*legacy-bios-bootable*, *read-only*, *hidden*, *no-automount*, or
	*bit-*_N_ for bit _N_.

# EXAMPLES

A partition for data in the free space of the disk there is:

```
{ "table": "gpt", "keep": true,
  "partitions": [ { "type": "linux", "size": "rest", "name": "Data" } ] }
```

A stick for Windows and Linux alike:

```
{ "table": "mbr",
  "partitions": [ { "type": "fat32", "size": "rest", "bootable": true } ] }
```

# SEE ALSO

*rdisk*(8), *mkfs*(8)
