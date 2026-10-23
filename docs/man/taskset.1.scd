taskset(1) "Dukovo" "Dukovo Manual"

; SPDX-FileCopyrightText: 2026 The Dukovo contributors
; SPDX-License-Identifier: Apache-2.0 OR MIT

# NAME

taskset - get or set the CPUs a process may run on

# SYNOPSIS

*taskset* [*-ac*] *-p* [_mask_] _pid_

*taskset* [*-c*] _mask_ _command_ [_argument_ ...]

# DESCRIPTION

taskset shows or sets the affinity of a thread: the CPUs the scheduler
may run it on (*radix-scheduling*(7)), as util-linux's taskset does. It
calls *radix-process*(2)'s *set_affinity* and *affinity* directly. With
*-p* it shows, and given a _mask_ sets, the affinity of the thread
_pid_ names (with *-a*, of every thread of that process); otherwise it
runs _command_ with the affinity _mask_, which its threads and children
inherit.

A _mask_ is in hexadecimal, bit 0 for CPU 0 (*0x3*: CPUs 0 and 1); with
*-c* it is a list of CPUs and ranges (*0,2-3*, and *0-7:2* for every
other one). A mask with no CPU that is online fails.

# OPTIONS

*-p*, *--pid*
	Show or set a running thread's affinity.

*-c*, *--cpu-list*
	Masks as lists of CPUs.

*-a*, *--all-tasks*
	Every thread of the process.

# EXAMPLES

	taskset -p 42
	taskset -c -p 0-1 42
	taskset -c 3 make

# SEE ALSO

*renice*(1), *ionice*(1), *systools*(1), *radix-scheduling*(7)
