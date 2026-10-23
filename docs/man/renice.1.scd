renice(1) "Dukovo" "Dukovo Manual"

; SPDX-FileCopyrightText: 2026 The Dukovo contributors
; SPDX-License-Identifier: Apache-2.0 OR MIT

# NAME

renice - change the nice level of running processes

# SYNOPSIS

*renice* [*-n*] _priority_ [*-p* | *-g* | *-u*] _id_ ...

# DESCRIPTION

renice sets the nice level of processes to _priority_, from -20 (the
largest share of the CPU) to 19 (the smallest), as util-linux's renice
does, with *setpriority*(2), and says each one's old and new levels.
The ids after *-p* are processes (the default), after *-g* process
groups, and after *-u* users; they may be mixed.

The nice level is that of the fair scheduling class
(*radix-scheduling*(7)); a real-time thread keeps its priority. Lowering
a process's level below 20 less its RLIMIT_NICE fails, as on Linux.

# OPTIONS

*-n*, *--priority* _n_
	Set each one's level to _n_, as util-linux's renice does; with
	*POSIXLY_CORRECT* in the environment, *-n* adds _n_ to it, as
	POSIX says.

*--relative* _n_
	Add _n_ to each one's level.

*-p*, *--pid*; *-g*, *--pgrp*; *-u*, *--user*
	The ids that follow are processes, process groups or users.

*-h*, *--help*
	A usage line.

# EXAMPLES

Give a build less of the CPU:

	renice -n 10 -p 42
	renice --relative 5 -g 40

# SEE ALSO

*ionice*(1), *taskset*(1), *nice*(1), *systools*(1),
*radix-scheduling*(7)
