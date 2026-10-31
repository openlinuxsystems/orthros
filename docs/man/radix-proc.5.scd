radix-proc(5) "Dukovo" "Dukovo Manual"

; SPDX-FileCopyrightText: 2026 The Dukovo contributors
; SPDX-License-Identifier: Apache-2.0 OR MIT

# NAME

radix-proc - the processes and what the kernel holds, as text in
/processes

# DESCRIPTION

A *proc* filesystem, which Spark mounts at _/processes_ (*spark*(8)),
with _/proc_, Linux's name for it, a link to it (*radix-files*(7)), shows
the processes the kernel holds and some of its state as plain text. It
is read-only. Every file is written when it is opened, so a program that
reads it in pieces sees one snapshot; a file opened again shows the state
of that moment. Sizes in *stat* are 0 until a file is opened, as on
Linux, so read a file to its end rather than by its size.

Most files are lines of *key*: _value_, one per line, in a fixed order.
Keys are lower case, with *\_* between words. The files with a line per
thing (a thread, a capability, a CPU) write it as fields of
*key*=_value_ separated by spaces, with a name or path, which may hold
spaces, last. New keys may be added in later versions, never in the
middle of the existing ones, so a reader should look keys up by name.
Times are in nanoseconds, and memory in KiB where the key does not say
otherwise. A newline or a backslash in a name or a path is written as
Linux writes it, a backslash and three octal digits (*\\012*, *\\134*),
so that it cannot end its line early.

Linux keeps its own _/proc_ formats, which programs read: Dukovo's own
tools read these instead (*ps*(1), *top*(1), *free*(1), *lsof*(8) and the
others of *systools*(1)), and the C library's *getloadavg*(3) reads
_/processes/loadavg_ for programs ported from Linux. The files of
_/processes/net_ are the exception: they are in Linux's formats, which
*ss*(8) and ported programs read.

This page describes _/processes_ as of Phase 13 of the roadmap.

Each process's directory belongs to the user and group it acts for,
with mode 0555. Its *status* is 0444, and its other files 0400, so
another user sees a process's number, name and state but not its
arguments, files or threads, unless the capability they look through
has the bypass right (*radix-names*(2), ADR 0214).

# FILES

_/processes/_*n*_/status_
	One directory per process the kernel holds, named by its number:
	those still running, and those that ended but whose last capability
	has not been deleted yet (see *radix-processes*(7)). Its keys:

	*number*
		The process's number, as *radix-process*(2)'s *info* gives it.

	*name*
		The name its builder gave it: the program's file name.

	*state*
		*created*, *running*, *ending* or *exited*.

	*code*
		Only once it has *exited*: its exit code, in hexadecimal.

	*threads*
		Its threads.

	*anonymous_memory*
		The pages of memory it holds that no file backs, followed by
		*pages*.

	*domain*
		The resource domain it is charged to.

	*kernel_memory*
		The kernel memory charged for it, in bytes, followed by
		*bytes*.

	*user*, *user_group*
		The user and group it acts for (*radix-names*(2)).

	*parent*, *group*, *session*
		For a POSIX process: the number of its parent, of its process
		group and of its session (*radix-posix*(7)).

	*terminal*
		*pts/*_n_ for the pseudo-terminal that is its session's
		controlling terminal, or *none*.

	*terminal_group*
		Only with a terminal: the number of its foreground process
		group, 0 for none, which *ps*(1) shows as TPGID and with a
		*+* in STAT for the processes in it.

	*run*
		What it is doing: *running* (a thread of it runs or is ready
		to), *disk* (a thread waits for a disk), *sleeping*, *stopped*
		(by a signal) or *zombie* (it has ended, and its parent has not
		waited for it).

	*started_ns*
		When it was made, in nanoseconds since the kernel started.

	*cpu_ns*, *user_ns*, *system_ns*
		The CPU time its threads have used, in all, in user mode and
		in the kernel, with the time of its threads that have ended.

	*policy*, *priority*, *nice*
		The scheduling of its first thread (*radix-scheduling*(7)):
		*other*, *fifo*, *round_robin*, *batch* or *idle*; the
		real-time priority (0 for the other policies); and the nice
		level.

	*io_class*, *io_level*
		Its I/O priority (*ionice*(1)): *none*, *realtime*,
		*best_effort* or *idle*, and the level in the class, 0 first.
		A process with none is served as best effort at level 4.

	*virtual_memory*, *resident_memory*
		The memory mapped in its address space, and the part of it in
		memory, in KiB.

	*minor_faults*, *major_faults*
		Page faults met without reading a disk, and those that read one.

	*read_bytes*, *written_bytes*
		The bytes its threads have read and written, from files, pipes,
		sockets and terminals alike: Linux's *rchar* and *wchar*, not
		only what went to a disk.

	*voluntary_switches*, *involuntary_switches*
		Times its threads gave up the CPU to wait, and times another
		thread was given it.

	*traced*
		*yes* while a tracer reads its system calls (*radix-trace*(2)),
		else *no*.

	*directory*
		Its working directory, last, as it named it: the C library
		tells the kernel at each *exec* and *chdir*, and a child
		has its parent's. Left out for a process that never said,
		such as Spark.

_/processes/_*n*_/cmdline_
	The arguments its program started with, each followed by a NUL, as
	on Linux. Empty for a process that was not started from a program
	file with arguments.

_/processes/_*n*_/threads_
	A line per thread, of the fields *number*, *run* (*running*,
	*sleeping*, *disk*, *new* or *dead*), *cpu* (the CPU it last ran
	on), *user_ns*, *system_ns*, *voluntary_switches*,
	*involuntary_switches*, *policy*, *priority* and *nice*, then
	*waits* (what it is blocked on, with _\__ for spaces) if it is
	blocked, and *name* last.

_/processes/_*n*_/files_
	A line per capability the process holds, by slot: *fd*=_n_ for a
	slot of the file descriptors (slot 64 + _n_, *radix-posix*(7)), else
	*slot*=_n_. For a file or directory the fields that follow are
	*kind* (*file*, *directory*, *block*, *char*, *pipe*, *socket* or
	*link*), *access* (*r*, *w*, *rw* or *-*), *id* (its node number),
	*size*, *fs* (the filesystem's kind), *mount* (the mount's number),
	*rights*, *offset* (unless a read or write holds it), *cut=yes* if
	the path was too long to keep whole, and *path*: the path it was
	opened by, made absolute, or a pipe's or socket's name. For any
	other capability they are *kind* (*thread*, *address_space*,
	*process*, *endpoint*, *random*, *kernel_log* and so on) and
	*rights*. Rights are names joined by *+*, as *radix-capabilities*(7)
	lists them, or *none*.

_/processes/mounts_
	One line per mount, in the order they were made, of four fields
	separated by spaces, and a fifth when the filesystem has a label:
	where it was mounted from (a device name such as _vda1_, or the
	filesystem's kind), where it is, its kind (*tmpfs*, *fat*, *exfat*,
	*btrfs*, *ext4*, *ntfs*, *proc*, *sys*, *dev* or *bootfs*), *rw* or
	*ro*, and
	*label=*_label_. The
	first four fields are those of Linux's _/proc/mounts_, and as there,
	a space, tab, newline or backslash in a field is written *\\040*,
	*\\011*, *\\012* or *\\134*.

_/processes/meminfo_
	Memory in KiB, each value followed by *KiB*: *total*, the memory the
	kernel manages; *free*; and what each owner holds: *kernel*,
	*page_tables*, *heap_slabs*, *large_heap_blocks*, *mapped_areas*,
	*per_cpu_caches*, *memory_objects* and *page_cache*. The page cache
	is the files' data in memory (*radix-files*(7)), which the kernel
	gives back when memory runs low. Then *swap_total* and *swap_free*,
	the room of the swap areas on and how much of it is free (*swapon*(8)),
	and *hardware_corrupted*, the memory a machine check said went bad,
	which is out of use for good or leaves use when freed (Linux's
	*HardwareCorrupted*; *dukovo-boot*(7)).

_/processes/stat_
	A line per online CPU, *cpu*_n_*:* then the fields *user_ns*,
	*system_ns* and *idle_ns* (the time it spent running programs, in
	the kernel, and idle) and *switches* (the threads it switched
	between). Then their sums, one key a line: *cpus*, *user_ns*,
	*system_ns*, *idle_ns* and *switches*; *running*, the threads ready
	to run; *disk_waiting*, the threads waiting for a disk;
	*disk_reads*, *disk_read_sectors*, *disk_writes* and
	*disk_written_sectors*, what the disks have done since boot, in
	512-byte sectors; *timer_writes*, *ipis*, *halts* and *polls*, the
	events that cost an exit to the host under a hypervisor (writes of
	the timer hardware, interrupts sent to other CPUs, idle periods that
	halted and idle periods a wake-up ended while the CPU polled);
	*swap_ins* and *swap_outs*, the pages read back from swap and
	written to it since boot; and *boot_time_ns*, when the kernel
	started, in nanoseconds since 1970.

_/processes/swaps_
	The swap areas on, as Linux's _/proc/swaps_ lists them: a line of
	headings, then a line per area with its device
	(_/devices/_*name*), *partition*, its size and what is in use, in
	KiB, and its priority.

_/processes/loadavg_
	*load_1*, *load_5* and *load_15*: the load averages over 1, 5 and
	15 minutes, with two decimals, as Linux computes them: the threads
	running, ready to run or waiting for a disk, sampled every five
	seconds.

_/processes/uptime_
	*uptime_ns*: nanoseconds since the kernel started.

_/processes/net/dev_
	What each interface received and sent, as Linux's _/proc/net/dev_
	writes it: two lines of headings, then a line per interface (*lo*,
	*net0*...) with its bytes, packets, errors and drops each way, and
	Linux's other columns, which are 0 (ADR 0190).

_/processes/net/route_
	The cards' routes, as Linux's _/proc/net/route_ lists its main
	table: a line of headings, then for each card the network of each
	address but a /32, and its default route, if it has a router.
	Addresses are in hexadecimal, as Linux writes them: an address's
	four bytes, in network order, read as a little-endian number
	(10.0.2.0 is *0002000A*). Each line is padded with spaces to 127
	characters. *lo*'s routes, in Linux's local table, are not listed.

_/processes/net/tcp_, _/processes/net/udp_
	The TCP and UDP sockets, as Linux's _/proc/net/tcp_ and _udp_ list
	them: a line of headings, then a line per socket with its two ends
	and Linux's number for its state, as hexadecimal (*0A* listening,
	*01* connected; *07* for every UDP socket), and the bytes queued to
	send and received. A socket that listens on every interface is
	listed once. The user is root's, and the timers, inode and other
	fields are 0.

_/processes/acpi/button/lid/_*name*_/state_
	For each lid of the firmware's (_PNP0C0D_, through uACPI), by its
	name in the ACPI namespace, as *LID0*, Linux's line: *state:* and six
	spaces, then *open* or *closed*. Without a lid, _/processes/acpi_ is
	not there.

# EXAMPLES

A process's status:

```
number: 9
name: nukesh
state: running
threads: 1
anonymous_memory: 212 pages
domain: system
kernel_memory: 40960 bytes
user: 0
user_group: 0
parent: 1
group: 9
session: 9
terminal: pts/0
terminal_group: 9
run: sleeping
started_ns: 1832004113
cpu_ns: 41202770
user_ns: 30419008
system_ns: 10783762
policy: other
priority: 0
nice: 0
io_class: none
io_level: 4
virtual_memory: 9416 KiB
resident_memory: 3104 KiB
minor_faults: 811
major_faults: 37
read_bytes: 1402880
written_bytes: 2210
voluntary_switches: 64
involuntary_switches: 3
traced: no
directory: /home
```

Its files:

```
slot=1 kind=thread rights=read+control+copy+transfer
slot=4 kind=random rights=read+copy+transfer
fd=0 kind=char access=rw id=30 size=0 fs=dev mount=7 rights=read+write+copy+transfer offset=0 path=/devices/pts/0
fd=3 kind=file access=r id=1201 size=5120 fs=tmpfs mount=3 rights=read+copy+transfer offset=4096 path=/system/settings/profile
```

The mounts after Spark has built the hierarchy on a machine with one
disk:

```
tmpfs / tmpfs rw
bootfs /system bootfs ro
tmpfs /system/settings tmpfs rw
tmpfs /system/state tmpfs rw
tmpfs /system/live tmpfs rw
tmpfs /tmp tmpfs rw
dev /devices dev rw
proc /processes proc ro
sys /kernel sys ro
vda /volumes/vda fat rw label=RADIX16
```

# SEE ALSO

*systools*(1), *ps*(1), *top*(1), *lsof*(8), *ss*(8), *radix-sysfs*(5),
*radix-files*(7), *radix-processes*(7), *radix-scheduling*(7), *spark*(8)
