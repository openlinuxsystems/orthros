spark(8) "Dukovo" "Dukovo Manual"

; SPDX-FileCopyrightText: 2026 The Dukovo contributors
; SPDX-License-Identifier: Apache-2.0 OR MIT

# NAME

spark - the first program, which starts every other

# SYNOPSIS

Kernel command line: [*init=*_path_] [_flags_ ...] [*--* [*--posix*] [_program_ [_argument_ ...]]]

# DESCRIPTION

Spark is the first program Dukovo runs. At the end of boot the Radix
kernel finds it in the boot archive, starts it in a process of its own,
and starts nothing else: every other program is started by Spark, or by
a program Spark started. The kernel keeps Spark's process for good, and
Spark must not end. If it does, the kernel stops with a panic that names
its exit code, since nothing could start a program again.

This page describes Spark v1, as of Phase 8 of the roadmap. It builds
the directory hierarchy and mounts the filesystems on it, mounts the FAT
filesystem of each disk under _/volumes_, then starts the one program its
arguments name, logs over the kernel log, waits for the program to exit,
reaps it and checks that all of the program's memory came back, or runs
it as a POSIX program (see *POSIX PROGRAMS*). Then it waits. Phase 9 makes it the supervisor of the system's services, with
restarts and resource domains.

# STARTING

The kernel starts Spark when the boot loader loaded a boot archive (a
module with the word *bootfs*, see *dukovo-boot*(7)) and no self-test
was asked for. It looks for the program at *init=*_path_ in the archive,
_/system/bin/spark_ by default, so another program can be tried as the
first one without rebuilding anything. Its arguments are its path and
then the words after a lone *--* on the kernel command line, at most 32;
the kernel's own flags end at the *--*.

Spark gets what every program gets (see *radix-startup*(2)) and four
things more:

- the boot archive, mapped read-only at a random address, whose place
  and size the start-up page gives;
- the root of the system's file tree, an empty tmpfs, as a directory
  capability with every right in *SLOT_ROOT* (7);
- a memory object of 4 MiB in slot 8, the first of the extra slots, from
  which it makes the memory of the programs it starts;
- an image placed at a random address, as every position-independent
  program is (see *radix-processes*(7)).

The kernel logs:

```
Init: the boot archive is 206 KiB with 9 entries; starting /system/bin/spark
Init: /system/bin/spark is process 1
```

# WHAT SPARK v1 DOES

. It checks the boot archive whole.
. It makes the root's ten directories, _/system_, _/applications_,
  _/users_, _/data_, _/tmp_, _/run_, _/dev_, _/proc_, _/sys_ and
  _/volumes_ (see *radix-files*(7) for what each is for).
. It mounts, in this order, the boot archive read-only on _/system_;
  tmpfs on _/system/etc_, _/system/var_, _/tmp_ and _/run_; and the
  *dev*, *proc* and *sys* trees on _/dev_, _/proc_ and _/sys_
  (*radix-proc*(5), *radix-sysfs*(5)); then it makes _/system/var/tmp_,
  for temporary files that are to outlive a restart. If any of this
  fails, it starts nothing.
. It makes the root's symbolic links for Unix programs: _/bin_ and
  _/sbin_ to _system/bin_, _/usr_ to _system_, _/etc_ to _system/etc_
  and _/var_ to _system/var_.
. It copies each default setting in _/system/lib/defaults/etc_ that
  _/system/etc_ lacks into it, with the directories it is in: the
  defaults the system ships, such as nano's _nanorc_, which an
  administrator edits in _/system/etc_. A file already there is never
  replaced, so an update can add settings without undoing edits; until
  _/system/etc_ is kept on a disk, each boot starts it with all of
  them.
. It looks for disks in _/dev_ and mounts the FAT filesystem of each
  partition, or of each disk with no partition table, at
  _/volumes/_*name*: read-write, or read-only when the device is. A
  device that holds no FAT filesystem is left alone.
. It reads the program its first argument names from _/system_,
  _/system/tests/hello_ by default, and passes it the arguments after
  that one.
. It starts the program in a process made for Spark, with 2 MiB of
  memory from its pool, the capabilities every program gets, the root
  directory with the read and write rights in *SLOT_ROOT*, and an exit
  notification for itself (radix-rt's *spawn*).
. It waits for the program to exit and logs the exit code.
. It reaps the program by deleting its capabilities, and checks that every
  page of the memory it gave came back to the pool.
. It waits for good.

Its lines in the kernel log start with *spark:*, after the kernel's
*spark[*_thread_*]:* prefix:

```
spark: Spark v1 is process 1, moved by 0x10ae4974a000
spark: boot archive of 206 KiB with 3 files at 0x20e61a756000
spark: built the hierarchy of 10 directories and mounted 8 filesystems
spark: started /system/tests/hello as process 2
spark: process 2 (/system/tests/hello) exited with code 0x0
spark: reaped process 2; all of its memory came back
spark: nothing more to do; waiting
```

# POSIX PROGRAMS

With *--posix* as its first argument, Spark runs the program after it as
a POSIX program (*radix-posix*(7)): it makes a process for it, gives it
the root directory, the random source and the kernel log, and a new
pseudo-terminal from _/dev/ptmx_ as descriptors 0, 1 and 2, then starts
it with *exec* as its own child, with the environment *PATH=/system/bin*,
*HOME=/*, *TERM=dumb*, *LANG=C.UTF-8* and *EDITOR=nano* and _/_ as its
working directory. It copies what the program writes to the terminal to the
kernel log, line by line, waits for the program with *wait*, and logs how
it ended:

```
spark: started /system/tests/posix/run as process 14, on a pseudo-terminal
posix: hello passed
...
posix: 20 of 20 tests passed
spark: process 14 (/system/tests/posix/run) exited with code 0x0
```

The lines the program writes come without Spark's *spark:* prefix.

A POSIX program and its children may *fork* and *exec* freely; Spark
waits only for the one it started. The *posix* boot test (*ob*(1))
boots with *-- --posix /system/tests/posix/run*.

# EXAMPLES

Boot the default image in QEMU, where Spark starts _/system/tests/hello_:

	./ob run

Give the test program arguments, with no other kernel flags:

	./ob run --cmdline "-- /system/tests/hello one two"

The *spark* boot test (see *ob*(1)) boots with the command line
*lockdep -- /system/tests/hello one two* under UEFI and checks each of
the lines above.

With disks, Spark logs a line for each volume it mounts. The *storage*
boot test gives QEMU four FAT disks (FAT12, FAT16, FAT32, and one with an
MBR partition), has _/system/tests/storage_ write to each, and checks
everything after a reboot:

```
spark: mounted the FAT filesystem of vda at /volumes/vda
spark: mounted the FAT filesystem of vdb at /volumes/vdb
spark: mounted the FAT filesystem of vdc at /volumes/vdc
spark: mounted the FAT filesystem of vdd1 at /volumes/vdd1
spark: started /system/tests/storage as process 2
```

Boot QEMU with a disk image of your own, formatted with *mkfs.fat*, and
find it at _/volumes/vda_:

	./ob run -- -drive file=disk.img,if=none,id=d0,format=raw -device virtio-blk-pci,drive=d0

# FILES

_/boot/bootfs_
	The boot archive in the boot image: a ustar file with
	_/system/bin/spark_ and the programs it starts.

_/system/bin/spark_
	Spark, in the boot archive.

_/system/tests/hello_
	The test program Spark starts by default. It prints its process
	number, its arguments and how far it was moved, fills its heap, and
	exits with 0.

_/system/tests/storage_
	The *storage* boot test's program: *storage write* writes files to
	every volume and checks _/tmp_, _/dev_, _/proc_ and _/sys_;
	*storage check* reads them back after a reboot.

_/system/tests/posix/run_
	The *posix* boot test's runner, which Spark runs with *--posix*: it
	runs the POSIX test programs beside it (*radix-posix*(7)).

_/volumes/_*name*
	Where each disk's FAT filesystem is mounted.

# DIAGNOSTICS

*Init: the boot loader loaded no `bootfs` module, so nothing starts*
	The boot image has no boot archive; the kernel stays at its prompt.

*Init:* _path_ *is not in the boot archive*
	*init=* names a file the archive does not have.

*Init: the boot archive is damaged:* _reason_
	The archive failed its check; nothing in it is used.

*Spark (process* _n_*) exited with code* _code_
	The panic when Spark ends.

*spark: could not start* _path_*:* _reason_
	Spark could not build the program's process; the reason names the
	step and the kernel's error.

*spark: cannot make /*_name_*:* _error_, *spark: cannot mount* _kind_ *on /*_path_*:* _error_, *spark: cannot link /*_name_ *to /*_target_*:* _error_
	Building the hierarchy failed; Spark starts nothing.

_name_ *holds no FAT filesystem; left alone*
	A disk or partition that is not FAT is not mounted.

*spark: cannot mount* _name_*:* _error_
	A FAT filesystem that could not be mounted: *corrupt* when its boot
	sector or tables are damaged.

*spark: cannot make a pseudo-terminal:* _reason_, *spark: could not start* _path_*:* _reason_
	Running a POSIX program failed before it started: _/dev/ptmx_ could
	not be opened, or the process, its descriptors or *exec* failed.

*spark: process* _n_ *(*_path_*) was ended by signal* _signal_
	The POSIX program was killed by a signal.

# SEE ALSO

*dukovo-boot*(7), *radix-files*(7), *radix-processes*(7), *radix-posix*(7),
*radix-proc*(5), *radix-sysfs*(5), *radix-directory*(2),
*radix-process*(2), *radix-startup*(2), *ob*(1)
