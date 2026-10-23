ps(1) "Dukovo" "Dukovo Manual"

; SPDX-FileCopyrightText: 2026 The Dukovo contributors
; SPDX-License-Identifier: Apache-2.0 OR MIT

# NAME

ps - a snapshot of the processes

# SYNOPSIS

*ps* [_option_ ...]

# DESCRIPTION

ps lists processes, one a line, with the columns asked for, as Linux's
ps (procps-ng) does, from _/proc_ (*radix-proc*(5)). It takes procps's
three kinds of options: UNIX options, which start with a dash (*-ef*);
BSD options, letters without a dash (*aux*); and GNU long options
(*--sort*).

Without options it lists the processes on its own terminal. A list of
numbers alone (*ps 1,7*) selects those processes.

# SELECTING

*-e*, *-A*
	Every process.

*-a*
	Every process with a terminal, but session leaders.

*-d*
	Every process but session leaders.

*-N*, *--deselect*
	Every process the other options do not select.

*-p*, *-q*, *--pid*, *--quick-pid* _list_
	The processes with these numbers.

*--ppid* _list_
	The children of these processes.

*-s*, *--sid* _list_
	The processes of these sessions.

*-g* _list_
	The processes of these process groups.

*-t*, *--tty* _list_
	The processes with these terminals (*pts/0*, or *0*).

*-C* _list_
	The processes with these names.

*-u*, *-U*, *--user*, *--User* _list_
	The processes of these users, by name or number.

*a*
	(BSD) Every process with a terminal.

*x*
	(BSD) Processes without a terminal too.

*e*, *g*, *A*
	(BSD) Every process.

# SHOWING

*-f*
	Full format: UID, PID, PPID, C, STIME, TTY, TIME and CMD, the
	command with its arguments.

*-F*
	Extra full: SZ, RSS and PSR too.

*-l*
	Long format, with F, S, UID, PRI, NI, ADDR, SZ and WCHAN.

*-j*
	Jobs format: PID, PGID, SID, TTY, TIME and CMD.

*-o*, *--format* _format_
	The columns of _format_: keys separated by commas or blanks, each
	with an optional *=*_header_, which takes the rest of the format.
	Given more than once, the columns add up.

*-O* _format_
	The default columns, with those of _format_ after PID.

*-L*
	A line per thread, with its number in LWP (and NLWP with *-f*).

*-T*
	A line per thread, with its number in SPID.

*-m*
	A line per thread, after its process.

*-H*, *--forest*, *f*
	The processes as a tree, children under their parent.

*--sort* _keys_
	Sort by the keys, each with *+* (increasing, the default) or *-*
	(decreasing) before it, the first first.

*--no-headers*, *h*
	No header line.

*--headers*
	The header line, as by default.

*-w*, *w*, *--width*, *--cols*, *--columns* _n_
	Do not cut lines at the terminal's width.

*u*, *j*, *l*, *s*, *v*
	(BSD) The user, jobs, long, signal and virtual memory formats.

*H*
	(BSD) Threads, as *-m*.

*-V*, *--version*
	Print ps's version.

*-y*, *-c*, *-M*, *-Z*, and BSD *r*, *c*, *S*, *T*, *X*, *n*, *Z* are
taken and change nothing.

# KEYS

The keys *-o* and *--sort* take, with the names procps also takes for
them:

[[ *Key*
:- *Header*
:- *What it shows*
|  *pid*, *tgid*
:  PID
:  the process's number
|  *ppid*
:  PPID
:  its parent's
|  *pgid*, *pgrp*
:  PGID
:  its process group's
|  *sid*, *sess*, *session*
:  SID
:  its session's
|  *tty*, *tt*, *tname*
:  TT
:  its terminal, or *?*
|  *stat*
:  STAT
:  its state letter and flags (below)
|  *s*, *state*
:  S
:  its state letter
|  *time*, *cputime*
:  TIME
:  its CPU time, [_dd_-]_hh_:_mm_:_ss_
|  *bsdtime*
:  TIME
:  its CPU time, _mm_:_ss_
|  *args*, *cmd*, *command*
:  COMMAND
:  its command and arguments
|  *comm*, *ucmd*, *ucomm*
:  COMMAND
:  its name
|  *uid*, *euid*, *ruid*, *suid*, *fsuid*
:  UID
:  its user's number
|  *user*, *euser*, *uname*, *ruser*, *suser*, *fsuser*
:  USER
:  its user's name
|  *gid*, *egid*, *rgid*
:  GID
:  its group's number
|  *group*, *egroup*, *rgroup*
:  GROUP
:  its group's name
|  *c*, *cp*
:  C
:  its CPU use, whole percent
|  *pcpu*, *%cpu*
:  %CPU
:  its CPU time over its life, in percent
|  *pmem*, *%mem*
:  %MEM
:  its resident memory, in percent of all
|  *vsz*, *vsize*
:  VSZ
:  its mapped memory, in KiB
|  *rss*, *rssize*, *rsz*
:  RSS
:  its resident memory, in KiB
|  *sz*
:  SZ
:  its mapped memory, in pages
|  *stime*, *start_time*
:  STIME
:  when it started: the time today, else the date
|  *start*, *bsdstart*
:  START
:  the same, BSD's way
|  *lstart*
:  STARTED
:  when it started, in full
|  *etime*, *etimes*
:  ELAPSED
:  how long it has run, as a time or in seconds
|  *ni*, *nice*
:  NI
:  its nice level
|  *pri*, *priority*
:  PRI
:  its priority
|  *rtprio*
:  RTPRIO
:  its real-time priority, or *-*
|  *cls*, *class*, *policy*, *sched*
:  CLS
:  its policy: TS, B, IDL, FF or RR
|  *psr*, *cpu*
:  PSR
:  the CPU it last ran on
|  *wchan*, *nwchan*
:  WCHAN
:  what it waits for
|  *lwp*, *tid*, *spid*
:  LWP
:  a thread's number
|  *nlwp*, *thcount*
:  NLWP
:  its threads
|  *majflt*, *minflt*
:  MAJFLT, MINFLT
:  its page faults
|  *f*, *flag*, *flags*, *addr*, *tpgid*
:  F, ADDR, TPGID
:  Linux's, always 0, *-* and -1
|  *ioclass*
:  IOCLASS
:  its I/O class (*ionice*(1))
|  *domain*
:  DOMAIN
:  its resource domain

The state letters: *R* running or ready to, *S* sleeping, *D* waiting
for a disk, *T* stopped, *Z* a zombie, *I* a thread not started yet, *X*
a thread that has ended. *stat* adds *<* for a real-time or high
priority, *N* for a low one, *s* for a session leader and *l* for a
process with more than one thread.

# EXAMPLES

Every process, in full:

	ps -ef

Every process, with its CPU and memory use, the busiest first:

	ps aux --sort=-%cpu

Chosen columns:

	ps -eo pid,ppid,stat,etime,args

The threads of process 12:

	ps -L -p 12

# SEE ALSO

*top*(1), *pgrep*(1), *kill*(1), *systools*(1), *radix-proc*(5)
