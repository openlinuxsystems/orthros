ionice(1) "Dukovo" "Dukovo Manual"

; SPDX-FileCopyrightText: 2026 The Dukovo contributors
; SPDX-License-Identifier: Apache-2.0 OR MIT

# NAME

ionice - get or set a process's I/O class and level

# SYNOPSIS

*ionice* [*-c* _class_] [*-n* _level_] [*-t*] *-p* _pid_ ...

*ionice* [*-c* _class_] [*-n* _level_] [*-t*] *-P* _pgid_ ...

*ionice* [*-c* _class_] [*-n* _level_] [*-t*] *-u* _uid_ ...

*ionice* [*-c* _class_] [*-n* _level_] [*-t*] _command_ [_argument_ ...]

# DESCRIPTION

ionice shows or sets the I/O priority of processes, as util-linux's
ionice does: a class, and a level in it, by which Radix's disks choose
whose reads and writes to serve first (*radix-scheduling*(7)). It calls
*radix-process*(2)'s *io_priority* directly. Without *-c* or *-n* it
shows each process's class and level; with them it sets them, for the
processes named, or for a command it then runs.

The classes, by name or number:

*0*, *none*
	None set: served as *best-effort* at level 4. The first process
	starts with none; one that *fork*(2) or *posix_spawn*(3) makes
	takes its maker's class and level.

*1*, *realtime*, *rt*
	Served before every other class, level 0 first. Linux lets only
	root set it; until users come, in Phase 13 of the roadmap, Radix
	lets any process that may signal the target.

*2*, *best-effort*, *be*
	Served by level, 0 first, before the idle class.

*3*, *idle*
	Served only when no other class waits.

# OPTIONS

*-c*, *--class* _class_
	The class to set.

*-n*, *--classdata* _level_
	The level to set, 0 to 7, for the realtime and best-effort classes.
	Without *-c* it sets best-effort.

*-p*, *--pid*; *-P*, *--pgid*; *-u*, *--uid*
	The ids that follow are processes, process groups or users.

*-t*, *--ignore*
	Run the command even if its priority could not be set.

# EXAMPLES

Show a process's class and level:

	ionice -p 42

Run a backup in the idle class:

	ionice -c idle tar -cf /volumes/vdb/home.tar /home

# SEE ALSO

*renice*(1), *taskset*(1), *systools*(1), *radix-scheduling*(7),
*radix-process*(2)
