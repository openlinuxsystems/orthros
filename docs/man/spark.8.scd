spark(8) "Dukovo" "Dukovo Manual"

; SPDX-FileCopyrightText: 2026 The Dukovo contributors
; SPDX-License-Identifier: Apache-2.0 OR MIT

# NAME

spark - the first program, which starts every other

# SYNOPSIS

Kernel command line: [*init=*_path_] [_flags_ ...] [*--* [*--no-volumes*] [*--compress* _disk_ ...] [*--watchdog* _seconds_] [*--test-hang*] [*--services* | *--service-dir* _directory_] [*--posix*] [_program_ [_argument_ ...]]]

# DESCRIPTION

Spark is the first program Dukovo runs. At the end of boot the Radix
kernel finds it in the boot archive, starts it in a process of its own,
and starts nothing else: every other program is started by Spark, or by
a program Spark started. The kernel keeps Spark's process for good, and
Spark must not end. If it does, the kernel stops with a panic that names
its exit code, since nothing could start a program again.

This page describes Spark v2, as of Phase 9 of the roadmap. It builds
the directory hierarchy and mounts the filesystems on it, mounts the FAT
filesystem of each disk under _/volumes_, and then supervises: it starts
the system's services (see *SERVICES*), each in a resource domain of its
own, starts them again when they end, answers requests on its control
socket (see *CONTROL SOCKET*), and stops them all in order when the
machine is to be turned off or restarted (see *SHUTTING DOWN*).

A boot test instead names one program on the kernel command line. Spark
starts that program, logs over the kernel log, waits for it to exit,
reaps it and checks that all of the program's memory came back, or runs
it as a POSIX program (see *POSIX PROGRAMS*); it starts the services too
only when *--services* comes first. *--service-dir* _directory_ starts
the services that _directory_ describes instead of
_/system/settings/spark/services_, with or without a program: the *spark-oom*
boot test points it at _/system/tests/services_. *--no-volumes*, before
the rest, leaves every disk's filesystems unmounted, for the disk check
(*ob image --disk-check*), which writes over a whole disk. *--compress*
_disk_, before the rest and as often as needed, mounts the Btrfs of the
device _disk_ (such as *vdb* or *nvme0n1p2*) to compress the data written
to it with zstd, as Linux's *compress=zstd:1* mount option does; other
disks are written as they are. *--watchdog* _seconds_, before the rest,
sets the hardware watchdog's timeout (60 seconds unless it is given; 0
leaves the watchdog off), and *--test-hang* has Spark stop responding
once it has started the watchdog, unless the watchdog restarted the
machine at the end of the last boot: the *watchdog* boot test's (see
*WATCHDOG*).

# STARTING

The kernel starts Spark when the boot loader loaded a boot archive (a
module with the word *bootfs*, see *dukovo-boot*(7)) and no self-test
was asked for. It looks for the program at *init=*_path_ in the archive,
_/system/commands/spark_ by default, so another program can be tried as the
first one without rebuilding anything. Its arguments are its path and
then the words after a lone *--* on the kernel command line, at most 32;
the kernel's own flags end at the *--*.

Spark gets what every program gets (see *radix-startup*(2)) and more:

- the boot archive, mapped read-only at a random address, whose place
  and size the start-up page gives;
- the root of the system's file tree, an empty tmpfs, as a directory
  capability with every right in *SLOT_ROOT* (7);
- a memory object of 4 MiB in slot 8, the first of the extra slots, from
  which it makes the memory of the program a boot test names;
- the *services* resource domain in slot 9, below the root domain, in
  which it makes a domain for each service (*radix-resource-domain*(2));
- the power capability in slot 10, which turns the machine off,
  restarts it or halts it (*radix-power*(2)), and which only Spark holds;
- an image placed at a random address, as every position-independent
  program is (see *radix-processes*(7)).

The kernel logs:

```
Init: the boot archive is 86929 KiB with 706 entries; starting /system/commands/spark
Init: /system/commands/spark is process 1
```

# WHAT SPARK v2 DOES

. It checks the boot archive whole.
. It makes the root's nine directories, _/system_, _/applications_,
  _/users_, _/shared_, _/tmp_, _/devices_, _/processes_, _/kernel_ and
  _/volumes_ (see *radix-files*(7) for what each is for).
. It mounts tmpfs on _/tmp_, and the *dev*, *proc* and *sys* trees on
  _/devices_, _/processes_ and _/kernel_ (*radix-proc*(5),
  *radix-sysfs*(5)).
. It looks for disks in _/devices_ and mounts the FAT, exFAT, Btrfs or
  ext4 filesystem of each partition, or of each disk with no partition
  table, at _/volumes/_*name*, read-write, or read-only when the device
  is or when the kernel refuses to write it (*read_only* from the disk,
  or *invalid_argument* for what Radix does not write yet: an exFAT with
  two tables, or a Btrfs or ext4 feature). A Btrfs named with
  *--compress* compresses what is written to it. FAT and exFAT are tried
  first, in that order, as *mkfs.btrfs* and *mkfs.ext4* clear the
  sectors they start in while *mkfs.fat* and *mkfs.exfat* may leave an
  older Btrfs or ext4 superblock. A Btrfs of several disks is mounted
  once, from the first of them found; the kernel refuses the others with
  *in_use*, and they are left alone (ADR 0153). A
  device that holds none of them is left alone, and so is every device
  with *--no-volumes*. So is an EFI system partition, whose
  _/kernel/block/_*name*_/parttype_ is *0xef* in an MBR or
  *c12a7328-f81f-11d2-ba4b-00a0c93ec93b* in a GPT: what boots the
  machine is not one of the user's volumes, as on Windows and macOS and
  with Linux's udisks (ADR 0161).
. The first Btrfs mounted read-write whose top holds the subvolumes
  _@system.a_, _@settings_, _@state_, _@applications_, _@users_ and
  _@shared_ is Dukovo's root, as *./ob image* makes it on its Dukovo root
  partition. Spark mounts it again to compress if it was not, as the
  root always does, then mounts _@system.a_ read-only on _/system_,
  _@settings_ on _/system/settings_, _@state_ on _/system/state_,
  _@applications_ on _/applications_, _@users_ on _/users_ and _@shared_
  on _/shared_, each as a directory of the mount at _/volumes/_*name*
  (the *bind* kind of *radix-files*(7)), and tmpfs on _/system/live_.
  With no such disk, or with *--no-volumes*, it mounts the boot archive
  read-only on _/system_ and tmpfs on _/system/settings_, _/system/state_
  and _/system/live_, which the next boot starts without. Then it makes
  _/system/state/tmp_, for temporary files that are to outlive a
  restart. If any of this fails, it starts nothing.
. It makes the symbolic links with Unix's names, for Unix programs,
  scripts and people: _/bin_ and _/sbin_ to _system/commands_, _/usr_
  to _system_, _/etc_ to _system/settings_, _/var_ to _system/state_,
  _/run_ to _system/live_, _/dev_ to _devices_, _/proc_ to _processes_,
  _/sys_ to _kernel_, and _/system/state/run_ to _../live_. It writes the names
  of those in the root to _/.hidden_, and _run_ to _/system/state/.hidden_,
  so that listings leave them out (*radix-files*(7)). The boot archive
  holds _/system_'s own links, _bin_, _lib_ and _share_. A directory or
  link that Dukovo's root kept from the boot before is left as it is.
. It copies each default setting in
  _/system/resources/default-settings_ that _/system/settings_ lacks
  into it, with the directories it is in: the
  defaults the system ships, such as nano's _nanorc_ and the service
  files, which an administrator edits in _/system/settings_. A file
  already there is never replaced, so an update can add settings without
  undoing edits. On Dukovo's root the first boot copies them all; with
  _/system/settings_ in memory, every boot does.
. It listens at _/system/live/spark/control_.
. With no program on the command line, as on a normal boot, it starts
  the services that _/system/settings/spark/services_ enables, the lowest
  *order* first.
. With a program, it reads it from _/system_ and passes it the arguments
  after it. It starts the program in a process made for Spark, with 2
  MiB of memory from its pool, the capabilities every program gets, the
  root directory with the read, write and control rights in
  *SLOT_ROOT*, and an
  exit notification for itself (radix-rt's *spawn*); waits for it to
  exit and logs the exit code; then reaps it by deleting its
  capabilities, and checks that every page of the memory it gave came
  back to the pool.
. It supervises for good.

Its lines in the kernel log start with *spark:*, after the kernel's
*spark[*_thread_*]:* prefix:

```
spark: Spark v2 is process 1, moved by 0x10ae4974a000
spark: boot archive of 86929 KiB with 706 files at 0x20e61a756000
spark: built the hierarchy of 9 directories and mounted 8 filesystems
spark: added 5 default settings to /system/settings
spark: listening at /system/live/spark/control
spark: 2 services in /system/settings/spark/services, 2 enabled
spark: started consoled (/system/commands/consoled) as process 12
spark: started diskhealth (/system/commands/diskhealth) as process 13
```

On Dukovo's root, as on a SATA disk written from the image, the
hierarchy line comes after the root's:

```
spark: /system is Dukovo's root on sda5, compressed (zstd)
spark: built the hierarchy of 9 directories and mounted 11 filesystems
```

and in a boot test that names a program:

```
spark: started /system/tests/hello as process 12
spark: process 12 (/system/tests/hello) exited with code 0x0
spark: reaped process 12; all of its memory came back
spark: nothing more to do; waiting
```

# SERVICES

A service is a program Spark starts at boot and keeps running. Each is
described by a file _/system/settings/spark/services/_*name*_.service_, whose
_name_, of 1 to 23 bytes, is the service's and its domain's. The
system's own come from _/system/resources/default-settings/spark/services_; to
turn one off, set *enabled = no* in its file rather than removing it,
or the next boot brings it back.

A service file is lines of _key_ *=* _value_; blank lines and lines
that start with *#* are left out. The keys:

*program* = _path_
	The program's absolute path. The only key a file must have.

*arguments* = _word_ ...
	Its arguments, after its path. Double quotes keep blanks in a word.

*description* = _text_
	What it is, for *service*(8).

*order* = _number_
	When it starts: lower first, 50 by default. Shutting down stops the
	highest first, so the services that others need want a low one.

*restart* = *always* | *on-failure* | *never*
	When it is started again after it ended: whatever its end (the
	default), only after an exit code other than 0 or a signal, or not
	at all.

*weight* = _number_
	Its domain's CPU weight, 1 to 10000, 100 by default: a busy domain
	of weight 200 gets twice the CPU time of one of 100 when they
	compete (*radix-scheduling*(7)).

*memory* = _size_ | *none*
	Its domain's memory limit, as bytes or with *K*, *M* or *G*, such as
	*64M*; none by default.

*oom* = *service* | *app*
	Its domain's out-of-memory class: *service*, the default, is stopped
	only after every app domain, when memory runs out
	(*radix-resource-domain*(2)).

*environment* = _NAME_*=*_value_ ...
	More of its environment.

*enabled* = *yes* | *no*
	Whether Spark starts it at boot; *yes* by default. *service* _name_
	*start* starts a disabled one.

A file Spark cannot read, or with a key it does not know, is logged and
left out. For example, _consoled.service_:

```
description = Virtual consoles
program = /system/commands/consoled
order = 10
restart = on-failure
weight = 200
memory = none
```

Spark starts each service as a POSIX program (*radix-posix*(7)) in a
domain of its own below *services*, which it makes the first time and
keeps for the restarts, with _/devices/null_ as its standard input, a pipe as
its standard output and error, _/_ as its working directory, and the
environment *PATH=/system/commands*, *HOME=/*, *TERM=dumb*, *LANG=C.UTF-8*
and *EDITOR=nano* with the file's *environment* over it. It copies what
the service writes, line by line, to the kernel log, after the service's
name and a colon unless the line starts with them already.

When a service ends, Spark logs how, and starts it again as its
*restart* says: after 0.1 seconds, and twice as long after each end in a
row that came less than 10 seconds after its start, up to 30 seconds. A
service that the out-of-memory policy stopped is logged as such, and
started again the same way. Processes a service started stay in its
domain; when the service's own process ends and some of them still
run, Spark stops the domain, as systemd ends a unit's control group, and
starts the service again only once the domain is empty:

```
spark: web (process 40) exited with code 0x1
spark: web left 2 threads behind; stopping its domain
spark: starting web again in 0.1 s
```

```
spark: consoled (process 12) was ended by signal 15
spark: starting consoled again in 0.1 s
spark: started consoled (/system/commands/consoled) as process 31
```

# CONTROL SOCKET

Spark listens at the Unix socket _/system/live/spark/control_. A request is one
line; Spark answers it with text and closes the connection. An answer
that starts with *ok:* or a status table is a success, one that starts
with *error:* a refusal. *shutdown*(8), *reboot*(8), *poweroff*(8),
*halt*(8) and *service*(8) are its clients.

*poweroff* | *reboot* | *halt* [_seconds_] [*--force*]
	Shut down to that end, in _seconds_ (now by default). A later
	request replaces a pending one. With *--force*, Spark ends the
	machine at once, without stopping the services.

*cancel*
	Cancel a pending shutdown.

*status*
	A line per service, its fields by tabs: its name; *running*,
	*waiting* (to be started again), *stopping* (what it left behind
	is ending), *stopped* or *disabled*; its
	process number or *-*; how often it was started again; the wait
	status of its last end, or *-*; its *restart*; its description. A
	pending shutdown adds a line *#*, *shutdown*, its end and the
	seconds left.

*start* | *stop* | *restart* _name_
	Start a service, stop one and keep it stopped until it is started
	again, or stop one and start it again, whatever its *restart*
	says. Stopping sends it SIGTERM; one that has not ended 5 seconds
	later has its domain stopped, as systemd sends SIGKILL after
	TimeoutStopSec.

*reload* _name_
	Send a running service SIGHUP, which asks a service that reads
	its settings to read them again (consoled reads its keyboard
	layout's, *consoled*(8)), as systemd's ExecReload does.

# SHUTTING DOWN

When a shutdown is due, Spark stops the services in groups by *order*,
the highest first: it sends each in the group SIGTERM, waits up to 5
seconds for them to end, then stops the domains of those that did not
(*radix-resource-domain*(2)'s *stop*, which ends every process in one)
and waits a second more. Once no service runs, it stops every service's
domain, for whatever they left behind, and calls the power capability.
The kernel writes back every filesystem's changes and gives the screen
back to its log, then powers the machine off through ACPI, restarts it,
or halts its processors (*radix-power*(2)):

```
spark: asked to power off the machine
spark: shutting down to power off: stopping 2 services
spark: diskhealth (process 13) was ended by signal 15
spark: consoled (process 12) was ended by signal 15
spark: every service has stopped
Power: `spark` asked to power off the machine; syncing the filesystems
```

A machine the kernel cannot power off, with no ACPI, is halted instead.

Pressing the machine's power button, where the kernel hears it (an ACPI
power button that is a fixed feature; *radix-power*(2)'s
*notify_button*), starts a shutdown to power off at once, as *poweroff*
does; one set for later is moved to now, and a press while shutting down
changes nothing:

```
Power: the power button was pressed
spark: the power button was pressed; powering off the machine
spark: shutting down to power off: stopping 2 services
```

Where the kernel cannot hear it, the firmware keeps the button.

# WATCHDOG

Once it supervises, Spark starts the machine's hardware watchdog, where
the kernel drives one (QEMU's i6300esb, or the TCO timer of an Intel ICH9
or ICH10; *radix-power*(2)'s *start_watchdog*), with a timeout of 60
seconds, and pets it every third of that from its loop. If Spark stops
responding, or the kernel stops running it, the watchdog restarts the
machine, as systemd's *RuntimeWatchdogSec* does; the next boot says so:

```
Watchdog: the Intel 6300ESB watchdog restarted the machine: the boot before this one hung
Watchdog: `spark` started the Intel 6300ESB watchdog: the machine restarts unless petted every 60 s
spark: the watchdog restarted the machine: the last boot stopped responding
```

Powering off or restarting gives the watchdog its longest timeout, up to
ten minutes, while the kernel writes the filesystems back; halting stops
it. *watchdog*(8) shows its state.

# POSIX PROGRAMS

With *--posix* as its first argument, Spark runs the program after it as
a POSIX program (*radix-posix*(7)): it makes a process for it, gives it
the root directory, the random source and the kernel log, and a new
pseudo-terminal from _/devices/ptmx_ as descriptors 0, 1 and 2, then starts
it with *exec* as its own child, with the environment *PATH=/system/commands*,
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
boots with *-- --posix /system/tests/posix/run*. A shutdown sends the
program SIGTERM along with the first group of services.

# EXAMPLES

Boot the default image in QEMU, where Spark starts the services and
consoled shows a shell on the screen (*consoled*(8)):

	./ob run

Start the test program with arguments instead, with no other kernel
flags:

	./ob run --cmdline "-- /system/tests/hello one two"

Start it and the services too:

	./ob run --cmdline "-- --services /system/tests/hello"

See what Spark supervises, restart a service, and turn the machine off
in five minutes:

	service --status-all++
service consoled restart++
shutdown +5

The *consoles* boot test boots with no program, types on tty1 and tty2,
restarts consoled with *service*(8) and powers the machine off with
*poweroff*(8).

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
	_/system/commands/spark_ and the programs it starts.

_/system/commands/spark_
	Spark, in the boot archive.

_/system/settings/spark/services/_*name*_.service_
	The services (see *SERVICES*); the system's own come from
	_/system/resources/default-settings/spark/services_.

_/system/live/spark/control_
	The control socket (see *CONTROL SOCKET*).

_/system/tests/hello_
	A test program for the boot tests. It prints its process number,
	its arguments and how far it was moved, fills its heap, and exits
	with 0.

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

_name_ *holds no FAT, exFAT, Btrfs or ext4 filesystem; left alone*
	A disk or partition that holds none of them is not mounted.

*spark: cannot mount* _name_*:* _error_
	A FAT, exFAT, Btrfs or ext4 filesystem that could not be mounted:
	*corrupt* when its boot sector, tables, superblock, trees or journal
	are damaged, and
	*invalid_argument* for a Btrfs filesystem Radix cannot read yet (on
	more than one device, or with sectors other than 4 KiB) or an ext4
	one (with clusters of several blocks, *bigalloc*, blocks larger than
	4 KiB or inodes larger than 1 KiB), and
	*read_only* for a Btrfs filesystem with a log of fsyncs to replay on
	a disk that cannot be written.

*spark: cannot make a pseudo-terminal:* _reason_, *spark: could not start* _path_*:* _reason_
	Running a POSIX program failed before it started: _/devices/ptmx_ could
	not be opened, or the process, its descriptors or *exec* failed.

*spark: process* _n_ *(*_path_*) was ended by signal* _signal_
	The POSIX program was killed by a signal.

*spark: /*_path_*:* _reason_*; it is not started*
	A service file that could not be read: the line and what is wrong
	with it.

*spark: could not start* _name_*:* _reason_
	A service could not be started: its domain, its process or *exec*
	failed. Spark tries again as after a quick end.

*spark:* _name_ *did not end in 5 s; stopping its domain*
	A service asked to stop or restart ignored SIGTERM: Spark stops
	every process in its domain.

*spark:* _name_ *left* _n_ *threads behind; stopping its domain*
	The service's process ended, and processes it started still ran:
	Spark stops them, and starts the service again, if it does, once
	they are gone.

_name_ *(process* _n_*) was ended by signal 9 (stopped by the out-of-memory policy)*
	Memory ran out and the kernel stopped the service's domain
	(*radix-resource-domain*(2)).

*spark: cannot hear of children ending; services are not watched*
	The kernel could not make the notification Spark waits on; the
	services start but are not started again.

*spark: the kernel gave no power capability; nothing can be turned off*
	A shutdown found no power capability in slot 10. Spark waits for
	good instead.

*spark: could not* _end_ *the machine:* _error_*; halting it*
	The kernel could not power the machine off (no ACPI) or restart it.

# SEE ALSO

*service*(8), *shutdown*(8), *reboot*(8), *consoled*(8), *dukovo-boot*(7),
*radix-files*(7), *radix-processes*(7), *radix-posix*(7), *radix-proc*(5),
*radix-sysfs*(5), *radix-directory*(2), *radix-process*(2),
*radix-resource-domain*(2), *radix-power*(2), *radix-startup*(2), *ob*(1),
*watchdog*(8)
