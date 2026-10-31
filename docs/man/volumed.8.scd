volumed(8) "Dukovo" "Dukovo Manual"

; SPDX-FileCopyrightText: 2026 The Dukovo contributors
; SPDX-License-Identifier: Apache-2.0 OR MIT

# NAME

volumed - mount the volumes plugged in after boot by their labels

# SYNOPSIS

*volumed*

*volumed --list*

# DESCRIPTION

volumed mounts each disk or partition plugged in after boot at
_/volumes/_*label*, its filesystem's label, as udisks does on Linux for a
desktop's removable media (ADR 0163). Spark runs it as the *volumed*
service. A volume whose filesystem has no label is mounted at
_/volumes/_*name*, its name in _/devices_, as Spark mounts the disks
there at boot; one whose label another volume has is mounted at
_/volumes/_*label*-*name*. A label's slashes become underscores.

It mounts FAT, exFAT, NTFS, Btrfs and ext4 as Spark does at boot
(*spark*(8)): FAT, exFAT and NTFS are tried first, and a filesystem the
disk or Radix cannot write (such as an NTFS that Windows hibernated
with) is mounted read-only. A disk with partitions has each of them
mounted, and an EFI system partition is left alone, as Windows, macOS
and udisks leave it.

When a disk is pulled out, volumed unmounts what is mounted at
_/volumes_ from it and its partitions, including what Spark mounted
there at boot, and removes the directories. Unmount a volume with
*eject*(1) before pulling its disk out, so that what it holds is
written first.

volumed takes the disks there are when it starts as done: Spark has
mounted them. It looks at _/kernel/block_ (*radix-sysfs*(5)) every
second for disks and partitions that come or go, and writes a line of
each change, which Spark puts in the kernel log:

```
volumed: watching for volumes; 5 disks and partitions now
volumed: sdb is gone; unmounted /volumes/sdb
volumed: mounted the FAT filesystem of sdb at /volumes/DUKOVOUSB
```

Its service file gives it _/devices_ and _/volumes_ and nothing else of
the tree (*service.toml*(5)). Spark hands it _/volumes_ as descriptor 3
as well, as *files.mount* says and *MOUNT_PLACES* names, since nothing
is mounted through a service's view; volumed mounts and unmounts through
it, and through the root when *MOUNT_PLACES* names nothing, as for
*eject*(1).

Until Dukovo has users, every program reaches _/volumes_ as it reaches
the rest of the root; once it has them, the user logged in at the
console is given each volume volumed mounts.

# OPTIONS

*--list*
	Print each volume mounted at _/volumes_, with its device and
	filesystem, and exit.

# EXIT STATUS

*volumed --list* exits 0, or 1 if the mounts cannot be read. As a
service, volumed does not exit.

# SEE ALSO

*eject*(1), *remount*(8), *spark*(8), *findmnt*(8), *blkid*(8),
*radix-files*(7), *systools*(1)
