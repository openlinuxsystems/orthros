radix-proc(5) "Dukovo" "Dukovo Manual"

; SPDX-FileCopyrightText: 2026 The Dukovo contributors
; SPDX-License-Identifier: Apache-2.0 OR MIT

# NAME

radix-proc - the processes and what the kernel holds, as text in /proc

# DESCRIPTION

A *proc* filesystem, which Spark mounts at _/proc_ (*spark*(8)), shows
the processes the kernel holds and some of its state as plain text. It
is read-only. Every file is written when it is opened, so a program that
reads it in pieces sees one snapshot; a file opened again shows the state
of that moment. Sizes in *stat* are 0 until a file is opened, as on
Linux, so read a file to its end rather than by its size.

Most files are lines of *key*: _value_, one per line, in a fixed order.
Keys are lower case, with *\_* between words. New keys may be added in
later versions, never in the middle of the existing ones, so a reader
should look keys up by name.

This page describes _/proc_ as of Phase 7 of the roadmap.

# FILES

_/proc/_*n*_/status_
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

	*domain*
		The resource domain it is charged to.

	*kernel_memory*
		The kernel memory charged for it, in bytes, followed by
		*bytes*.

_/proc/mounts_
	One line per mount, in the order they were made, of four fields
	separated by spaces, and a fifth when the filesystem has a label:
	where it was mounted from (a device name such as _vda1_, or the
	filesystem's kind), where it is, its kind (*tmpfs*, *fat*, *proc*,
	*sys*, *dev* or *bootfs*), *rw* or *ro*, and *label=*_label_. The
	first four fields are those of Linux's _/proc/mounts_.

_/proc/meminfo_
	Memory in KiB, each value followed by *KiB*: *total*, the memory the
	kernel manages; *free*; and what each owner holds: *kernel*,
	*page_tables*, *heap_slabs*, *large_heap_blocks*, *mapped_areas*,
	*per_cpu_caches*, *memory_objects* and *page_cache*. The page cache
	is the files' data in memory (*radix-files*(7)), which the kernel
	gives back when memory runs low.

_/proc/uptime_
	*uptime_ns*: nanoseconds since the kernel started.

# EXAMPLES

A process's status:

```
number: 2
name: storage
state: running
threads: 1
domain: system
kernel_memory: 40960 bytes
```

The mounts after Spark has built the hierarchy on a machine with one
disk:

```
tmpfs / tmpfs rw
bootfs /system bootfs ro
tmpfs /system/etc tmpfs rw
tmpfs /system/var tmpfs rw
tmpfs /tmp tmpfs rw
tmpfs /run tmpfs rw
dev /dev dev rw
proc /proc proc ro
sys /sys sys ro
vda /volumes/vda fat rw label=RADIX16
```

# SEE ALSO

*radix-sysfs*(5), *radix-files*(7), *radix-processes*(7),
*radix-scheduling*(7), *spark*(8)
