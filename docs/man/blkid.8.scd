blkid(8) "Dukovo" "Dukovo Manual"

; SPDX-FileCopyrightText: 2026 The Dukovo contributors
; SPDX-License-Identifier: Apache-2.0 OR MIT

# NAME

blkid - what each disk and partition holds

# SYNOPSIS

*blkid* [*-o* _format_] [*-s* _tag_]... [*-t* _tag_=_value_] [_device_...]

*blkid* *-L* _label_ | *-U* _uuid_

# DESCRIPTION

blkid reads the first sectors of each disk and partition the kernel has
(_/kernel/block_, *radix-sysfs*(5)), or of each _device_ named, as
_sda1_ or _/devices/sda1_, and prints what it finds as util-linux's
blkid does, with libblkid's tags in its order:

- *LABEL*, *UUID* and *TYPE* of a filesystem: *vfat* (FAT12, FAT16 and
  FAT32, its UUID the volume serial number), *exfat*, *ext2*, *ext3*,
  *ext4*, *btrfs* (with *UUID_SUB*, the UUID of this disk of it) and
  *swap*, with the size of its sectors or blocks as *BLOCK_SIZE*;
- *PTUUID* and *PTTYPE* (*gpt* or *dos*) of a disk's partition table;
- *PARTUUID* and *PARTLABEL* of a partition, from its disk's table; an
  MBR partition's PARTUUID is the disk's signature and its number.

A device that holds none of these is left out.

```
$ blkid
/devices/vda: PTUUID="0a6b0d4e-3c1f-4f5e-9b5d-21c0e0f2a7d1" PTTYPE="gpt"
/devices/vda1: UUID="7C3A-19E2" BLOCK_SIZE="512" TYPE="vfat" PARTLABEL="EFI" PARTUUID="..."
/devices/vdb: LABEL="data" UUID="..." UUID_SUB="..." BLOCK_SIZE="4096" TYPE="btrfs"
```

# OPTIONS

*-o*, *--output* _format_
	*full*, the default: a line per device, as above. *value*: each value
	on a line of its own. *export*: *DEVNAME=*_path_ and _TAG_=_value_
	lines for a shell, with a blank line after each device. *device*: the
	device's path alone.

*-s*, *--match-tag* _tag_
	Show only _tag_; given again, show each tag given.

*-t*, *--match-token* _tag_=_value_
	Show only the devices whose _tag_ has _value_, as *-t TYPE=btrfs*.

*-L*, *--label* _label_; *-U*, *--uuid* _uuid_
	Print the path of the device whose filesystem has that label or UUID.

*-c*, *--cache-file* _file_; *-p*, *--probe*; *-g*, *--garbage-collect*
	Taken for scripts written for Linux, and ignored: blkid keeps no
	cache, and always reads the devices.

# EXIT STATUS

0 if a device was found with a tag shown; 2 if none was, as util-linux's
blkid; 1 for a mistake in the command line.

# SEE ALSO

*lsblk*(8), *findmnt*(8), *radix-sysfs*(5), *systools*(1)
