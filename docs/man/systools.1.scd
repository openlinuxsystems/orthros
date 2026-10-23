systools(1) "Dukovo" "Dukovo Manual"

; SPDX-FileCopyrightText: 2026 The Dukovo contributors
; SPDX-License-Identifier: Apache-2.0 OR MIT

# NAME

systools - Dukovo's administration tools, in one program

# SYNOPSIS

_tool_ [_argument_ ...]

*systools* _tool_ [_argument_ ...]

*systools --list*

# DESCRIPTION

systools is the program behind the commands an administrator expects
from Linux whose Linux versions read Linux's _/proc_, its utmp file or
its system calls, which Radix does not have: Dukovo has its own, written
in Rust for Radix, with the Linux tools' names, options and output, so
that what people type and what scripts read stay the same.

Each command is a link to _/system/bin/systools_ by its name, and
systools runs the tool its name says. *systools* _tool_ runs a tool by
name, and *systools --list* lists them.

The tools that show processes read *radix-proc*(5); *lsof*(8) lists the
capabilities each process holds; *rtrace*(1) reads a trace of a
process's system calls (*radix-trace*(2)); *dmesg*(1) reads the kernel's
log through _/dev/kmsg_ (*radix-files*(7)). Until users come, in Phase
13 of the roadmap, every process is root's, so the user columns all say
root and the options that select users select all or nothing.

A mistake in the command line makes a tool fail with exit status 2 (1
for those whose Linux versions use 1), after a message on standard
error that starts with the tool's name.

# TOOLS

[[ *Tool*
:- *What it does*
|  *chgrp*(1), *chmod*(1), *chown*(1)
:  change a file's group, mode or owner
|  *column*(1)
:  put lines in columns, or a table
|  *df*(1)
:  the space of each filesystem
|  *dmesg*(1)
:  the kernel's log
|  *free*(1)
:  memory in use and free
|  *hexdump*(1), *hd*(1)
:  a file's bytes, in hexadecimal or other formats
|  *ionice*(1)
:  a process's I/O class and level
|  *kill*(1)
:  send a signal
|  *lsof*(8)
:  what each process holds open
|  *pgrep*(1), *pkill*(1)
:  find or signal processes by name
|  *pidstat*(1)
:  what each process uses
|  *ps*(1)
:  the processes
|  *renice*(1)
:  a process's nice level
|  *rtrace*(1)
:  trace a process's system calls
|  *script*(1)
:  record a terminal session
|  *taskset*(1)
:  the CPUs a process may run on
|  *top*(1)
:  the processes, refreshed
|  *tree*(1)
:  directories as a tree
|  *uptime*(1)
:  how long the system has been up
|  *users*(1), *w*(1), *who*(1)
:  who is logged in, and what they are doing
|  *vmstat*(8)
:  memory, CPU and disk activity
|  *watch*(1)
:  run a command again and again
|  *which*(1)
:  where a command is
|  *xxd*(1)
:  a hex dump, and back

# SEE ALSO

*radix-proc*(5), *radix-posix*(7), *nukesh*(1)
