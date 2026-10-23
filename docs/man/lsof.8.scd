lsof(8) "Dukovo" "Dukovo Manual"

; SPDX-FileCopyrightText: 2026 The Dukovo contributors
; SPDX-License-Identifier: Apache-2.0 OR MIT

# NAME

lsof - list what processes hold open

# SYNOPSIS

*lsof* [*-atos*] [*-p* _list_] [*-c* _name_] [*-u* _list_] [*-d* _list_] [*+D* | *+d* _directory_] [*--files*] [_file_ ...]

# DESCRIPTION

lsof lists, a line each, what every process holds, from each one's
_/proc/<n>/files_ (*radix-proc*(5)), with lsof's columns: COMMAND, PID,
USER, FD, TYPE, DEVICE, SIZE/OFF, NODE and NAME. Each process's
working directory comes first, as *cwd*.

A file or directory shows the path it was opened by, worked out from
the directory it was opened beneath without following links. A file
renamed since keeps its old path, and a path too long to keep shows its
start, followed by *(cut)*. Its FD is its descriptor with *r*, *w* or
*u* (both); its DEVICE is its file system's kind and its mount's
number, joined by a colon (*fat:2*), and its NODE its number in that
file system.

A process holds more than files: every capability in its space is a
line too, as Radix's handles are (*radix-capabilities*(7)). Its FD is
the startup slot's name (*rtd* the root directory, *prc* the process,
*thd* the thread, *spc* the address space, *rnd* the random source,
*log* the log) or *s* and the slot's number, its TYPE the kind of
object (*ENDP* an endpoint, *NTFY* a notification, *MEM* a memory
object, *PROC*, *THRD*, *SPACE*, *DOMN*, *RING*, *KLOG*...), and its
NAME its rights, such as *send+grant*.

Until users come, in Phase 13 of the roadmap, every process is root's.
Radix has no network files until Phase 12: *-i* lists nothing and
exits 1.

# OPTIONS

Without selecting options every process is listed. Several select any
process one of them chooses, or with *-a* only one all of them choose.

*-p* _list_
	The processes with these numbers, by commas; *^*_n_ leaves process
	_n_ out.

*-c* _name_
	The processes whose names start with _name_.

*-u* _list_
	The processes of these users, by name or number.

*-d* _list_
	Only these descriptors, by number or FD name (*cwd*, *rtd*...).

*+D* _directory_
	Only the files under _directory_, at any depth.

*+d* _directory_
	Only the files in _directory_ itself.

_file_
	Only the lines for _file_, by path, or by node when its path was
	cut.

*--files*
	Only files and directories, without the other capabilities.

*-a*
	Choose the processes every selecting option chooses.

*-t*
	Only the numbers of the processes found, a line each, for scripts.

*-o*
	The SIZE/OFF column shows offsets, as *0t*_n_.

*-s*
	The SIZE/OFF column shows sizes. By default it shows the size of
	a file and the offset of a pipe, socket or device.

*-n*, *-P*, *-l*, *-b*, *-w*, *-V*, *-U* and *-R* are taken and change
nothing.

# EXIT STATUS

0, or 1 if a search (*-p*, *-c*, *+D*, *+d* or a _file_) found nothing.

# EXAMPLES

Who holds a disk image open:

	lsof /volumes/vdb/home.img

What a shell holds, without its other capabilities:

	lsof --files -p 42

# SEE ALSO

*ps*(1), *rtrace*(1), *systools*(1), *radix-proc*(5),
*radix-capabilities*(7)
