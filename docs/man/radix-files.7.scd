radix-files(7) "Dukovo" "Dukovo Manual"

; SPDX-FileCopyrightText: 2026 The Dukovo contributors
; SPDX-License-Identifier: Apache-2.0 OR MIT

# NAME

radix-files - files, directories, filesystems and the directory hierarchy
in the Radix kernel

# DESCRIPTION

A program reaches files only through *directory capabilities*. There is no
global root and no current directory: every path is resolved beneath the
directory capability it is given with, and a program can reach exactly
what lies beneath the directories it holds. Spark, the first program,
holds the root of the system's tree and hands out capabilities to the
parts each program needs. The interfaces are *radix-directory*(2) and
*radix-file*(2); the ring (*radix-ring*(2)) has read, write and sync
operations for files too.

This page describes files as of Phase 9 of the roadmap. The design, with
the arguments that a path never leaves its directory and that a freed
cluster is never written by its old owner, is in _docs/design/storage.md_.

# PATHS

A path is UTF-8 of at most 4,096 bytes (*PATH_MAX*), names separated by
*/*, each at most 255 bytes (*NAME_MAX*), with no NUL. *.* is the
directory itself and *..* its parent, but never above the directory the
path starts from: a path that starts with */*, or whose *..* climbs above
its directory, fails with *outside* (at the root of the whole tree, *..*
is the root, as */..* is */*). This is how WASI's preopened directories
and Capsicum's *openat* work. A walk crosses into whatever is mounted on
a directory it passes through, and *..* crosses back.

A *symbolic link* holds a path, its target, which the walk follows in
its place. A relative target goes on from the link's directory; one that
starts with */* goes on from the directory the walk started from, as if
that were the root, so a link never leads out of what a program holds
(Linux's *RESOLVE_IN_ROOT*). *..* steps back along the names the walk
took, never along the text of a target. A walk follows at most 40 links
and then fails with *too_many_links*. A link that is a path's last name
is followed too, unless the call is told not to (*no_follow*), or a */*
ends the path; removing or renaming a link works on the link itself, and
creating a file through one creates its target. *make_link*, *read_link*
and *real_path* (the path a walk reaches, every link followed) are in
*radix-directory*(2).

Absolute paths are for people and for POSIX programs: Spark strips the
leading */* from the paths on its command line and resolves them beneath
the root it holds, and the C library does the same with the directory
each POSIX program is given as its root (*radix-posix*(7)).

# RIGHTS

A directory capability's rights say what it can do:

*read*
	look names up, list the directory, open files for reading and
	directories, and *stat*.

*write*
	create, remove and rename, open files for writing, and set times.

*control*
	mount and unmount filesystems on directories beneath it.

*open* makes a capability with the rights it asks for, which must be
among the directory's own: a file can have *read* and *write*, a
directory *read*, *write* and *control*. Both get *copy* and *transfer*,
so they can be passed on (see *radix-capabilities*(7)). A file opened for
writing on a read-only filesystem fails with *read_only*.

# TIMES

A file keeps when it was made, when its contents last changed and when
it was last read. Reading never changes the last (as with Linux's
*noatime*): it is when the file was made, or what *set_times* set.
*set_times* on a file needs its *write* right, and on a path the
directory's; tmpfs keeps times to the nanosecond, FAT in two-second
steps and the last read to the day. *stat* also says the mount a file is
on, a number unique since boot.

# FILES AND THE PAGE CACHE

A file's data is kept in memory in its *page cache*: a read copies from
the cached pages (fetching them from the disk first), and a write copies
into them and marks them dirty. The disk sees a change when its
filesystem is synced:

- every five seconds, by the kernel's writeback thread;
- when a program calls *sync* on a file or a directory of the filesystem;
- when the filesystem is unmounted.

So a power cut loses at most the last five seconds of changes, as on
Linux. *sync* returns once the disk says it has the data. The disk reads
and writes cached pages directly, so data is copied once between a
program and the disk.

When memory runs low, the writeback thread drops clean cached pages, and
the out-of-memory policy (*radix-scheduling*(7)) asks it to before it
stops anything. Cached pages are counted as *page_cache* in
_/proc/meminfo_.

A file removed while it is open lives on, unnamed, until its last
capability goes, then its space is freed.

# FILESYSTEMS

*mount* takes a filesystem kind by name:

*tmpfs*
	Files in memory only. A tmpfs a program mounts may grow to half of
	memory and hold one file or directory for every two pages of it, as
	on Linux; nothing in it survives a reboot. It keeps symbolic links.

*fat*
	FAT12, FAT16 or FAT32, read-write, with long names, on the block
	device whose file capability the call names (a disk or a partition,
	from _/dev_). Names compare without regard to case, as FAT requires,
	and may not hold control characters or any of *" \* / : < > ? \\ |*.
	A file is at most 4 GiB less one byte. FAT has no symbolic links
	(*make_link* fails with EPERM). exFAT comes in Phase 10.

*proc*
	The processes and what the kernel holds, as text (*radix-proc*(5)).

*sys*
	The kernel's devices and settings, one value per file, in Linux's
	layout (*radix-sysfs*(5)).

*dev*
	The devices: _null_, _zero_, _random_, _urandom_, _kmsg_ (the kernel
	log, *radix-kernel-log*(2)), a block device per disk and partition,
	the terminals: _ptmx_, _tty_ and _pts/_*N* (*radix-posix*(7)), and
	from Phase 9 the screen and the keyboard: _fb0_, the framebuffer
	the firmware set up (*radix-framebuffer*(2)), and _kbd_, the PS/2
	keyboard's key events (*radix-input*(2)), which *consoled*(8) uses.

*bootfs*
	The boot archive (*radix-processes*(7)), read-only, with the
	symbolic links the archive holds.

A second mount on a directory stacks on the first. *unmount* syncs the
filesystem first, and fails with *in_use* while anything is mounted on it
or anything holds a capability to a place in it. A rename never moves a
file between filesystems (*cross_mount*), and a mount point cannot be
removed.

# THE DIRECTORY HIERARCHY

The kernel makes only an empty root. Spark (*spark*(8)) builds the tree
on it at every boot:

[[ *Path*
:< *What it is*
|  _/system_
:< the boot archive, read-only: programs and the system's own files
|  _/system/etc_, _/system/var_
:< tmpfs for now; settings, which start from the defaults in
   _/system/lib/defaults/etc_ (*spark*(8)), and the system's changing data,
   with _/system/var/tmp_ for temporary files that are to outlive a restart
|  _/applications_
:< applications (bundles come in Phase 13)
|  _/users_
:< the users' homes
|  _/data_
:< data shared between users
|  _/tmp_, _/run_
:< tmpfs: temporary files, and the state of running services
|  _/dev_
:< the devices
|  _/proc_
:< the processes (*radix-proc*(5))
|  _/sys_
:< the kernel's devices and settings (*radix-sysfs*(5))
|  _/volumes_
:< disks, USB sticks and SD cards, one directory each
|  _/bin_, _/sbin_, _/usr_, _/etc_, _/var_
:< symbolic links to _system/bin_, _system/bin_, _system_, _system/etc_
   and _system/var_, for Unix programs and scripts

Until the root filesystem is on a disk (Btrfs, Phase 10), _/applications_,
_/users_ and _/data_ are empty directories of the root's tmpfs.

# VOLUMES

At boot the kernel finds each disk, reads its partition table (MBR or
GPT) and names the disk and its partitions as Linux does: _vda_, _vdb_ for
virtio disks, _vda1_ for a partition. Each appears in _/dev_ and
_/sys/block_. Spark mounts the FAT filesystem of each partition, or of
each disk with no partition table, at _/volumes/_*name*, read-write, or
read-only when the device is. A device with no FAT filesystem is left
alone. Disks are found once, at boot. Removable media, mounted by label
at _/volumes/_*label* with safe eject, come with *volumed* in Phase 11.

# DIAGNOSTICS

Errors of files and directories are in the *file* domain (2) of the
status word (*radix-errors*(2)): *not_found*, *exists*, *not_directory*,
*is_directory*, *not_empty*, *read_only*, *no_space*, *name_too_long*,
*bad_name*, *outside*, *cross_mount*, *in_use*, *io*, *corrupt*,
*too_big*, *unknown_filesystem*, *not_block_device*, *no_device*,
*too_many_links* (ELOOP) and *not_link* (EINVAL), each with the POSIX
errno it maps to.

The kernel logs each disk, partition and mount:

```
Disk: vda is a virtio block device of 32 MiB at 0000:00:03.0, 512 bytes a block, write cache, MSI-X vector 0x50
Disk: vda has no partition table; it is used whole
Files: mounted fat from vda on /volumes/vda, labelled RADIX16
```

*Files: writing back to a disk failed:* _error_*; changes stay in memory*
	A sync by the writeback thread failed; the dirty pages stay in
	memory and are tried again at the next sync.

# LIMITS

- Only virtio block devices have a driver: AHCI and NVMe come in Phase
  10, USB storage in Phase 11.
- FAT has no journal: a power cut in the middle of a write can leave lost
  clusters, which *fsck.fat* finds and frees, as on every system.
- There is no name cache, no read-ahead and no hot plug yet.

# SEE ALSO

*radix-directory*(2), *radix-file*(2), *radix-ring*(2), *radix-errors*(2),
*radix-proc*(5), *radix-sysfs*(5), *radix-capabilities*(7),
*radix-processes*(7), *spark*(8)
