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
_/processes/meminfo_.

A file removed while it is open lives on, unnamed, until its last
capability goes, then its space is freed.

# NAMES AND SPECIAL FILES

A file can have more than one name, its *hard links*, which *link* in
*radix-directory*(2) makes, on the same filesystem and not for a
directory. Removing a name leaves the file to its others; the file goes
when its last name and its last capability have. *stat* gives how many
names it has, *links*: on Btrfs at most 65,535, and one more fails with
*too_many_names* (EMLINK). A directory says 1, as Linux's Btrfs does.

*make_node* makes a *named pipe*, a *device node* or a *socket's name*.
Opening one opens what it stands for, on a read-only mount too:

- A named pipe opens an end of the one pipe its opens share while any
  is open. An open only to read waits for a writer, and one only to
  write waits for a reader; with *non_blocking*, a read open succeeds
  at once and a write open with no reader fails with *no_device*
  (ENXIO). An open to read and write waits for nothing.
- A device node opens the device in _/devices_ with Linux's number:
  _null_ 1:3, _zero_ 1:5, _random_ 1:8, _urandom_ 1:9, _kmsg_ 1:11,
  _tty_ 5:0, _ptmx_ 5:2 and _fb0_ 29:0. Any other fails with
  *no_device*.
- A socket's name is bound with *bind_socket* and connected through
  with *connect_socket* (*radix-directory*(2)), and fails with
  *no_device* when opened. On Btrfs it stays on the disk,
  but what is bound to it lasts only while the filesystem is mounted,
  as on Linux.

*stat*'s *changed* is when the file's names, times or contents last
changed, kept by tmpfs and Btrfs; FAT and exFAT give *modified*.

# FILESYSTEMS

*mount* takes a filesystem kind by name:

*tmpfs*
	Files in memory only. A tmpfs a program mounts may grow to half of
	memory and hold one file or directory for every two pages of it, as
	on Linux; nothing in it survives a reboot. It keeps symbolic links,
	hard links and the special files below.

*fat*
	FAT12, FAT16 or FAT32, read-write, with long names, on the block
	device whose file capability the call names (a disk or a partition,
	from _/dev_). Names compare without regard to case, as FAT requires,
	and may not hold control characters or any of *" \* / : < > ? \\ |*.
	A file is at most 4 GiB less one byte: as on Linux, a write that
	would pass that writes what fits, and one at the limit fails with
	EFBIG. FAT has no symbolic links
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

*bind*
	A directory of a filesystem already mounted, whose capability the
	call names, mounted again at the path: the same files, one
	filesystem. It is read-only if the call asks or the directory's own
	mount is. Spark mounts the subvolumes of Dukovo's root this way
	(*spark*(8)). A directory of a read-only Btrfs subvolume refuses
	every change with EROFS, as on Linux, however it is mounted.

On Btrfs, a directory's *make_subvolume* makes a new, empty subvolume;
*snapshot* makes a snapshot of the subvolume that holds a directory in
another directory of the same filesystem, read-only if asked; and
*remove_subvolume* removes a subvolume with everything in it, in one
transaction, freeing what no snapshot shares (*subvolume*(8)). *remove*
takes an empty subvolume too, as Linux's *rmdir*(2) does. A subvolume
that is the default, holds another, or has something in it open fails
to go (*in_use*, *not_empty*).

A Btrfs file's *control* takes Linux's *FICLONE* and *FICLONERANGE*
(*ioctl_ficlone*(2)): the file opened for writing shares the blocks of
another, opened for reading on the same filesystem, whole or a range of
whole blocks, until either changes them, so the copy takes no room.
*cp*(1) clones this way unless given *--reflink=never*. Offsets and
lengths not in whole blocks, ranges of one file that overlap, and a file
with checksums cloned to or from one without fail with EINVAL; another
filesystem fails with EXDEV. Other filesystems fail with EOPNOTSUPP.

A Btrfs directory's *scrub* reads every block of its filesystem that
the last commit holds, every copy, checks each against its checksum,
and writes a good copy over a bad one unless asked only to read, as
Linux's *btrfs scrub* does (*scrub*(8)). It needs the directory's read
right, and its write right to mend.

A second mount on a directory stacks on the first. *unmount* syncs the
filesystem first, and fails with *in_use* while anything is mounted on it
or anything holds a capability to a place in it. A rename never moves a
file between filesystems (*cross_mount*), and a mount point cannot be
removed.

# THE DIRECTORY HIERARCHY

The kernel makes only an empty root. Spark (*spark*(8)) builds the tree
on it at every boot. Each folder is named for what it holds:

[[ *Path*
:< *What it is*
|  _/system_
:< the system, read-only: programs and the system's own files; the
   _@system.a_ subvolume of Dukovo's root on a disk, or else the boot
   archive
|  _/system/commands_
:< the programs, where the shell's *PATH* leads
|  _/system/libraries_
:< the libraries, the dynamic linker _ld.so_ and the locales
|  _/system/resources_
:< everything else the programs read: manual pages, terminal
   descriptions, completions, the PCI ID database, the licenses, and the
   default settings
|  _/system/settings_
:< the system's settings, which start from the defaults in
   _/system/resources/default-settings_ (*spark*(8)): _@settings_ on
   Dukovo's root, or else a tmpfs
|  _/system/state_
:< the system's changing data, _@state_ on Dukovo's root or else a
   tmpfs, with _/system/state/tmp_ for temporary files that are to
   outlive a restart
|  _/system/live_
:< a tmpfs: the state of running services, such as Spark's control
   socket
|  _/applications_
:< applications (bundles come in Phase 13)
|  _/users_
:< the users' homes
|  _/shared_
:< files that users and services share
|  _/tmp_
:< a tmpfs: temporary files
|  _/devices_
:< the devices
|  _/processes_
:< the processes (*radix-proc*(5))
|  _/kernel_
:< the kernel's view of the hardware and its settings (*radix-sysfs*(5))
|  _/volumes_
:< disks, USB sticks and SD cards, one directory each

Unix's names for these are symbolic links in the root, for Unix
programs, scripts and people: _/bin_ and _/sbin_ lead to
_system/commands_, _/usr_ to _system_, _/etc_ to _system/settings_,
_/var_ to _system/state_, _/run_ to _system/live_, _/dev_ to _devices_,
_/proc_ to _processes_ and _/sys_ to _kernel_; _/system/state/run_
leads to _../live_, so that _/var/run_ is _/run_. The root's _.hidden_ file lists them, one name a line, and
*ls*(1) and the shell's completion leave the names it lists out, unless
asked for every name (*ls -a*), as the file manager will.

The boot archive has the same for _/system_: _bin_ leads to _commands_,
_lib_ to _libraries_ and _share_ to _resources_, so _/usr/bin/env_,
_/usr/lib/ld.so_ and _/usr/share/man_ are there too, and
_/system/.hidden_ lists them.

Until the root filesystem is on a disk (Btrfs, Phase 10), _/applications_,
_/users_ and _/shared_ are empty directories of the root's tmpfs.

# VOLUMES

At boot the kernel finds each disk, reads its partition table (MBR or
GPT) and names the disk and its partitions as Linux does: _vda_, _vdb_ for
virtio disks, _sda_, _sdb_ for SATA disks, _nvme0n1_, _nvme0n2_ for the
namespaces of the first NVMe controller and _nvme1n1_ for the second's,
_vda1_ for a partition, and _nvme0n1p1_ after a name that ends in a digit.
When the machine is powered off or restarted, every disk's write cache is
emptied and each NVMe controller is shut down. Each
appears in _/devices_ and _/kernel/block_. As on Linux, an entry that starts past
the disk's end is skipped, an MBR one that reaches past it is cut to end
there, and an extended partition shows only its first two sectors. Spark
mounts the FAT, exFAT, Btrfs or ext4 filesystem of each partition, or of
each disk with no partition table, at _/volumes/_*name*, read-write, or
read-only when the device is, for exFAT when it has two tables (TexFAT)
or its medium reported a failure, for Btrfs when it uses a feature
Radix does not write yet (named holes, without *no-holes*, or quotas),
and for ext4 (and ext2 and ext3) when it does: no extents, quotas,
inline data, encryption, case folding, MMP, fast commits or a journal on
another device. A mount replays the journal Linux left. A device with
none of them is left alone. Disks are found once, at boot. Removable media, mounted by label at _/volumes/_*label*
with safe eject, come with *volumed* in Phase 11.

A disk or partition in _/devices_ is read and written raw, at any offset and
length, with nothing cached between, as Linux's are when opened with
*O_DIRECT*; *fsync*(2) empties the disk's write cache, and *ioctl*(2)
takes Linux's requests for its size, sector size and discards
(*radix-block*(2)). The whole sectors of a transfer that starts on a
sector's boundary, into or out of memory on a sector's boundary (an
aligned *posix_memalign*(3) or *mmap*(2) buffer), go straight between
the disk and the program's memory; the rest is copied. A write that
covers part of a sector reads the rest of the sector first. A sector is
the disk's own: 512 bytes, or 4096 on a disk of 4 KiB sectors, which
*BLKSSZGET* gives, and which a discard must cover whole. Writing to
a disk that holds a mounted filesystem damages the filesystem, as on
every system.

# DIAGNOSTICS

Errors of files and directories are in the *file* domain (2) of the
status word (*radix-errors*(2)): *not_found*, *exists*, *not_directory*,
*is_directory*, *not_empty*, *read_only*, *no_space*, *name_too_long*,
*bad_name*, *outside*, *cross_mount*, *in_use*, *io*, *corrupt*,
*too_big*, *unknown_filesystem*, *not_block_device*, *no_device*,
*too_many_links* (ELOOP), *not_link* (EINVAL) and *too_many_names*
(EMLINK), each with the POSIX
errno it maps to.

The kernel logs each disk, partition and mount:

```
Disk: vda is a virtio block device of 32 MiB at 0000:00:03.0, 512 bytes a block, write cache, discards, MSI-X vector 0x50
Disk: vda has no partition table; it is used whole
Disk: AHCI 1.0 controller at 0000:00:1f.2: 6 ports, 32 command slots, NCQ, MSI vector 0x54
Disk: sda is a SATA disk of 64 MiB on AHCI port 0: QEMU HARDDISK, 512 bytes a block, NCQ depth 32, write cache, TRIM
Disk: nvme0 is an NVMe 1.4 controller at 0000:00:04.0: QEMU NVMe Ctrl, firmware 8.2.2, 4 I/O queues of 63 commands, an MSI-X vector for each of 4 CPUs, shadow doorbells, scatter gather lists
Disk: nvme0n1 is an NVMe namespace of 64 MiB: QEMU NVMe Ctrl, 512 bytes a block, write cache, deallocates
Files: mounted fat from vda on /volumes/vda, labelled RADIX16
```

*Disk:* _name_*: a read of* _n_ *sectors from sector* _s_ *failed after* _t_ *tries:* _why_
	A request failed for good, and the program that made it got *io*
	(EIO). It names the request (*a read*, *a write*, *a discard* or
	*a flush*), how many times it went to the disk when more than once,
	and what the disk said: for NVMe, its status (*unrecovered read
	error*, *write fault*, ...), otherwise *an I/O error*. At most 10
	such lines are logged for each disk in 5 seconds.

*Disk:* _name_*: an error stopped its port (status* _bits_*); it runs again (*_n_* errors since boot)*
	A SATA disk reported an error, or the link to it failed, and the
	port was restarted. The commands in flight go again, five times at
	most, and the one the disk names as failed ends in *io* (EIO). The
	status bits are the port's interrupt status (AHCI's PxIS).

*Disk:* _name_*: a request went unanswered for 30 s; the link was reset, and what was in flight goes again*
	A SATA disk stopped answering, and its link was reset.

*Disk:* _name_*: a request went unanswered for 30 s; the controller was reset, and what was in flight goes again*
	An NVMe controller stopped answering, or said it failed, and was
	disabled and enabled again, its queues made again. When it does
	not come back, the line ends instead in *its disks are gone, and
	their requests fail*.

	When the disk does not come back, or another one is there after
	the reset, the line ends instead in *the disk is gone, and its
	requests fail*: every request to it fails from then on, until the
	next boot.

*Files: writing back to a disk failed:* _error_*; changes stay in memory*
	A sync by the writeback thread failed; the dirty pages stay in
	memory and are tried again at the next sync.

# LIMITS

- virtio block devices, SATA disks on AHCI controllers and NVMe
  namespaces have drivers; USB storage comes in Phase 11. Port
  multipliers, optical drives, disks with logical sectors larger than
  4096 bytes and NVMe namespaces with metadata are not supported.
- FAT has no journal: a power cut in the middle of a write can leave lost
  clusters, which *fsck.fat* finds and frees, as on every system.
- There is no name cache and no hot plug yet, and FAT files are read a
  page per request (Btrfs reads ahead).

# SEE ALSO

*radix-directory*(2), *radix-file*(2), *radix-block*(2), *radix-ring*(2),
*radix-errors*(2),
*radix-proc*(5), *radix-sysfs*(5), *radix-capabilities*(7),
*radix-processes*(7), *scrub*(8), *spark*(8), *subvolume*(8)
