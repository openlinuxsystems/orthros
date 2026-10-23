radix-posix(7) "Dukovo" "Dukovo Manual"

; SPDX-FileCopyrightText: 2026 The Dukovo contributors
; SPDX-License-Identifier: Apache-2.0 OR MIT

# NAME

radix-posix - POSIX programs on the Radix kernel: the C library, fork and
exec, signals, terminals, pipes, timers, locks and Rust's standard library

# DESCRIPTION

Radix runs two kinds of program. A *native* program is built by its
starter from outside and holds only the capabilities it was given
(*radix-processes*(7)). A *POSIX* program is a C or Rust program written
for Unix: it is linked with the C library, mlibc, and may *fork*, *exec*,
take signals, read a terminal, wait for its children and resolve paths
from a working directory. Both are processes of the same kind, and a
POSIX program holds capabilities too: its file descriptors are
capability slots, and its root directory is the one it was given.

The kernel keeps what must outlive a process or be shared between
processes: the process tree, signals, pipes, pseudo-terminals, timers,
file locks and limits (radix-process(2)'s methods from 11 on,
*radix-signal*(2), *radix-terminal*(2)). The C library's system layer,
_ports/mlibc/sysdeps/radix_, turns each C library call into capability
methods. Radix takes Linux's numbers wherever programs see them: errno
values, signal numbers and frames, *struct stat*, *termios* and *ioctl*
requests, clock numbers and the auxiliary vector.

This page describes the POSIX layer as of Phase 8 of the roadmap. Its
design, with the arguments that no wake-up is lost and that every lock
goes with its owner, is in _docs/design/posix.md_; the decisions are ADRs
0060 to 0075.

# BUILDING PROGRAMS

*ob build* builds the C library and every port (*ob*(1), PORTS) with the
build host's Clang 20, LLD and LLVM tools. It writes a toolchain to
_out/toolchain_:

*x86_64-radix-cc*
	Compiles C for Radix (Linux's x86-64 ABI, with *\_\_radix\_\_* and
	*\_\_dukovo\_\_* defined and Linux's own macros taken away) against
	the C library's headers only, and links a static
	position-independent program by default. *-dynamic* links a program
	whose interpreter is _/system/lib/ld.so_, on the shared C library;
	*-shared* links a shared library.

_out/toolchain/meson/radix.ini_
	A Meson cross file for programs; the C library has one of its own,
	_radix-libc.ini_.

*x86\_64-radix-ar*, *-ld*, *-nm*, *-objcopy*, *-ranlib*, *-readelf*, *-strip*
	LLVM's tools, under the names build systems look for.

*x86\_64-unknown-radix*
	The Rust target (_userspace/targets/_), with the standard library
	built from the pinned toolchain's *rust-src* into
	_out/toolchain/rust_. Programs on it live in _userspace/programs/_,
	a Cargo workspace of their own.

System programs are static, one file each with nothing to resolve at
start. Bundles may ship private libraries and load plugins with *dlopen*
(see *DYNAMIC LINKING*).

# STARTING A PROGRAM

A POSIX program starts from radix-process(2)'s *exec*: in its own
process, which it replaces, or in a process *create* made and nobody has
started, which becomes the caller's child (*posix\_spawn*'s way; Spark
starts POSIX programs so, with *--posix*, see *spark*(8)). *exec* takes a
file capability and a block of arguments, environment and extra strings,
the first of which is the working directory.

The program gets:

- its segments mapped from the file and filled as they are touched, an
  interpreter (*PT_INTERP*) mapped at a random place if it names one,
  or, for a script that starts with *#!*, its interpreter run with the
  script's path, one level deep;
- a stack of *RLIMIT_STACK* bytes (8 MiB by default, 128 KiB to 1 GiB)
  below a random top, laid out as the System V ABI and Linux lay it out:
  arguments, environment and the auxiliary vector;
- the start-up page and the slots every program has (*radix-startup*(2)),
  its root directory in *SLOT_ROOT* (7) and its file descriptors from
  slot 64.

The auxiliary vector has Linux's entries (*AT_PHDR*, *AT_ENTRY*,
*AT_RANDOM*, *AT_EXECFN*, *AT_PAGESZ* and the rest) and two of Radix's:
*AT_RADIX_CWD* (0x52440001), the working directory, and
*AT_RADIX_STARTUP* (0x52440002), the start-up page's address.

Spark gives every POSIX program it starts the environment
*PATH=/system/bin*, *HOME=/*, *TERM=dumb* and *LANG=C.UTF-8*, the root
with the read and write rights, and a new pseudo-terminal as descriptors
0, 1 and 2, whose output it copies to the kernel log.

# PROCESSES

Each process has a number, which *getpid* returns, and a place in one
process tree: its parent, its process group and its session. A thread's
number comes from the same count, so a process's first thread has the
process's number.

*fork* makes a child that holds its own capability, so it outlives its
parent, with:

- a capability space of the caller's size, with a copy of every
  capability that has the *copy* and *transfer* rights, in the same slot
  with the same slot flags: so every file descriptor, sharing its open
  file description;
- the caller's memory, copy-on-write;
- the caller's signal actions, controlling terminal, limits and file
  mode mask, and the calling thread's signal mask, alternate signal
  stack and scheduling;
- one thread, going on from the call;
- no pending signals, no timers and none of the caller's record locks.

*exec* replaces the program: the caller's other threads stop, the new
address space takes the old one's place, descriptors with *FD_CLOEXEC*
close, handled signals go back to their defaults, POSIX timers are
deleted (interval timers stay) and limits, the mask, the process number
and the working directory carry over. Everything that can fail is done
before the caller is touched, so a failed *exec* returns to a caller
that has lost nothing.

*wait*, *waitpid* and *waitid* take Linux's options and return Linux's
status words, with the CPU time the child used. A process whose parent
has gone, or never had one, is reaped as it ends: there is no process
that adopts orphans. A child is reaped at once too while its parent
ignores SIGCHLD or has *SA_NOCLDWAIT*.

*setpgid*, *setsid*, *getpgid* and *getsid* follow POSIX: a group's
members share a session, a session leader leads its own group, and
process groups exist while they have members. *posix\_spawn* is the C
library's, on *fork* and *exec*; *vfork* is *fork*.

# SIGNALS

Signals have Linux's numbers: SIGHUP 1 to SIGSYS 31, then real-time
signals 32 to 64. A process has one action per signal; each thread has a
mask, its pending signals and an alternate stack (*sigaltstack*). A
signal sent to a process goes to a thread that does not block it.

A thread takes pending signals on its way back to user mode, after any
system call, interrupt or fault. A handler gets the frame Linux's x86-64
handlers get: a *ucontext_t* with every register, the mask and the
alternate stack, the *siginfo_t*, and the extended state (x87, SSE, AVX)
in XSAVE form; returning from it restores what the handler left in the
context. SIGKILL and SIGSTOP cannot be caught, blocked or ignored.

A call that waits fails with EINTR when a signal's handler runs, and is
run again instead when the handler has *SA_RESTART* or no handler ran.
Calls that wait for a time or for a signal (*poll*, *select*,
*nanosleep*, *sigsuspend*, *sigtimedwait* and timed futex waits) are run
again only when no handler ran, as on Linux.

Faults are signals for the thread that faulted: SIGSEGV for memory it may
not touch, SIGBUS for a misaligned access or a file page that cannot be
read, SIGILL, SIGFPE and SIGTRAP. A fault whose signal has no handler or
is blocked ends the process, killed by that signal. A page that finds no
memory ends the process as SIGKILL would.

Real-time signals queue with their values, up to 1,024 per process
(what *RLIMIT_SIGPENDING* says), and are taken lowest number first and
in order within a number. *kill* may signal the caller, its descendants and the
processes of its session; *kill(-1, ...)* signals all of those but the
caller. Users come with Phase 13, and with them POSIX's rules.

# FILE DESCRIPTORS AND PATHS

File descriptor _n_ is capability slot 64 + _n_. A file capability is an
open file description, with its offset and status flags, so *dup*,
*dup2* and *F_DUPFD* copy the capability and the copies share both;
*FD_CLOEXEC* is the slot's *close_on_exec* flag. The capability space
grows as descriptors are opened, up to the soft *RLIMIT_NOFILE* (1,024 by
default) and never past 4,032 descriptors.

The C library keeps the working directory as a canonical string. It
hands the kernel an absolute path, or a relative one joined to the
working directory, as a path beneath the root directory, and the kernel
works out *.*, *..* and symbolic links; *..* never climbs above the
root. A path relative to a directory descriptor (*openat* and the rest)
goes to the kernel with that directory's capability, beneath which *..*
cannot climb. So the C library notes the path each directory descriptor
was opened by, and a path with *..* in it goes from the root by that
path instead (ADR 0089). *chdir* and *fchdir* check the directory and
keep its canonical path.

*stat* fills in Linux's *struct stat*: the kind of file, its size, the
blocks it has, its number (*st_ino*), the number of the mount it is on
(*st_dev*), when its contents last changed (*st_mtim*) and when it was
last read (*st_atim*). Reading a file does not change *st_atim*, as with
Linux's *noatime*: it is when the file was made, or what a program set.
*st_ctim* is *st_mtim*, since the kernel keeps no time of the last
change to a file's status. Until users come in Phase 13, everything
belongs to user 0, regular files and directories have mode 0755,
devices 0660 or 0666, pipes 0600 and sockets 0777.

*utimensat*, *futimens* and the older *utime* and *utimes* set a file's
times on tmpfs and FAT, which keeps them in two-second steps and the
last read only to the day, as Linux's vfat does. Times before 1970 fail
with EINVAL, and devices and pipes, which keep no times, with EPERM.
*futimens* needs a descriptor open for writing (EACCES otherwise),
where Linux lets the file's owner.

_/dev/fd/_*N* names descriptor _N_, and _/dev/stdin_, _/dev/stdout_ and
_/dev/stderr_ name 0, 1 and 2, for *open*, *stat* and *access* and
their *at* forms; the shell passes pipes by these names. The C library
gives them: opening one copies the descriptor, as *dup* does, as on the
BSDs, so the copy shares its offset, never has more access than it
(EACCES), and *O_TRUNC* does nothing. A descriptor not open has no name
(ENOENT). _/dev/fd_ itself cannot be listed.

*statvfs* and *fstatvfs* say the filesystem's block size and blocks,
and how many files and directories it can hold: a tmpfs one for every
two pages of memory, as on Linux, and the boot filesystem the ones it
has. FAT keeps no count of files, so *f_files* is 0 for it, as for
Linux's vfat.

# PIPES AND POLL

*pipe* and *pipe2* make a kernel pipe of 64 KiB. Writes of up to
*PIPE_BUF* (4,096) bytes go in whole, never mixed with other writers';
longer ones go in as room comes. With no writer left, an empty pipe
reads as its end; a write with no reader left raises SIGPIPE and fails
with EPIPE. *O_NONBLOCK* makes either end fail with EAGAIN instead of
waiting.

*poll* and *ppoll* take *struct pollfd* as it is. Pipes, sockets and
terminals say when they can be read or written, or have hung up; other
files are always ready. *select* and *pselect* are the C library's, on *poll*.
There is no *epoll*, *eventfd*, *signalfd* or *timerfd*.

# SOCKETS

*socketpair* makes two connected *AF_UNIX* stream sockets, with
*SOCK_NONBLOCK* and *SOCK_CLOEXEC* if asked: a pipe's buffer each way
(*radix-process*(2)'s *socket_pair*). *read*, *write*, *send*, *recv*,
*sendto*, *recvfrom*, *sendmsg* and *recvmsg* move bytes, with
*MSG_DONTWAIT*, *MSG_NOSIGNAL* and *MSG_WAITALL*; *MSG_MORE* is taken and
does nothing. Up to *PIPE_BUF* bytes of one send go in whole. *shutdown*
shuts one way or both, for every descriptor of the socket. *fstat* says
*S_IFSOCK*, *getsockname* and *getpeername* give an unnamed *AF_UNIX*
address, and *getsockopt* answers *SO_TYPE*, *SO_DOMAIN*, *SO_PROTOCOL*,
*SO_ERROR*, *SO_ACCEPTCONN*, *SO_RCVBUF* and *SO_SNDBUF*.

Datagram and sequenced-packet pairs fail with ESOCKTNOSUPPORT, and
*MSG_PEEK*, *MSG_OOB* and control messages (passing descriptors) with
EOPNOTSUPP. *socket* fails with EAFNOSUPPORT: named sockets come later
in Phase 9, network sockets with Phase 12.

# TERMINALS

Opening _/dev/ptmx_ makes a pseudo-terminal and gives the opener its
master side; *grantpt*, *unlockpt* and *ptsname* give the terminal side,
_/dev/pts/N_, which lasts until the master goes and hangs it up. Between
the two is POSIX's general terminal interface as Linux's N_TTY line
discipline behaves: canonical lines with their editing characters, echo,
the signal characters (^C SIGINT, ^\\ SIGQUIT, ^Z SIGTSTP), *IXON* flow
control, CR and NL translation both ways, *VMIN* and *VTIME*, and window
sizes with SIGWINCH. Settings are Linux's *struct termios*, and the
requests are Linux's *ioctl* numbers for what POSIX's terminal calls do:
*TCGETS* to *TCSETSF*, *TCXONC*, *TCFLSH*, *TIOCSCTTY*, *TIOCNOTTY*,
*TIOCGPGRP*, *TIOCSPGRP*, *TIOCGSID*, *TIOCGWINSZ*, *TIOCSWINSZ*,
*FIONREAD*, *TIOCGPTN* and *TIOCSPTLCK* (*radix-terminal*(2)).

A terminal may be the controlling terminal of one session: the first one
its leader opens without *O_NOCTTY* while it has none, or one it asks for
with *TIOCSCTTY*. A background process of the session that reads it gets
SIGTTIN, and one that writes it with *TOSTOP* set, or changes its
settings, SIGTTOU, as job control needs. _/dev/tty_ opens the caller's
controlling terminal. When the session leader ends, or the master goes,
the foreground group gets SIGHUP and SIGCONT.

There is no terminal for the console in the kernel: *consoled*, in
Phase 9, holds a master for each virtual terminal.

# CLOCKS, SLEEPING AND TIMERS

*clock_gettime* reads Linux's clocks by Linux's numbers: *CLOCK_REALTIME*
from the hardware's wall clock, *CLOCK_MONOTONIC* and *CLOCK_BOOTTIME*
from the scheduler's clock, their *\_COARSE* and *\_RAW* forms, and
*CLOCK_PROCESS_CPUTIME_ID* and *CLOCK_THREAD_CPUTIME_ID*. Every clock
counts in nanoseconds. Nobody may set the clock yet (*clock_settime*
fails with EPERM).

*nanosleep* and *clock_nanosleep* sleep as timed futex waits, and tell
how long was left when a signal ends them. *alarm* and *setitimer*'s
three timers (*ITIMER_REAL*, *ITIMER_VIRTUAL* and *ITIMER_PROF*) send
SIGALRM, SIGVTALRM and SIGPROF; *timer_create*'s timers run on the
real-time, monotonic, boot-time and process CPU clocks, signal the
process or one thread, or start a thread (*SIGEV_THREAD*), and count
overruns while their signal waits. One kernel thread runs every timer. A
CPU-time timer fires within 10 ms of when it is due.

# THREADS

*pthread_create* starts a thread in the same process
(radix-process(2)'s *start*), with its thread-local storage set up by the
C library. Locks, condition variables and *pthread_join* sleep on
futexes (radix-address-space(2)'s *futex_wait* and *futex_wake*), which
are private to a process. *pthread_setname_np* names a thread in up to
15 bytes, as Linux allows, and the kernel log and the debugger show the
name. *gettid* returns the thread's number.

# SCHEDULING AND LIMITS

*nice*, *setpriority*, *sched_setscheduler* and *sched_setparam* set a
thread's policy (*SCHED_OTHER*, *SCHED_BATCH*, *SCHED_IDLE*, *SCHED_FIFO*,
*SCHED_RR*), real-time priority (1 to 99) and nice level (-20 to 19),
which the scheduler carries out (*radix-scheduling*(7)). As on Linux
they belong to threads, and a process's number names its first thread.
A thread starts with its maker's scheduling. Changing another process
needs what *kill* needs; asking for more of the CPU follows Linux's rules
for a process without privileges: a nice level down to 20 less
*RLIMIT_NICE*, and real-time policies up to *RLIMIT_RTPRIO*. Both are 0
by default, so a process can only lower itself.

*setrlimit* and *getrlimit* keep Linux's sixteen limits with Linux's
defaults; *fork* copies them and *exec* keeps them. A hard limit can
only be lowered. The kernel enforces *RLIMIT_STACK*, *RLIMIT_NICE* and
*RLIMIT_RTPRIO*, and the C library *RLIMIT_NOFILE*. The others are kept for programs to read, and enforce
nothing yet. *umask* is kept and inherited, but has no effect until
files have permissions.

# FILE LOCKS

*flock* locks a whole file for an open file description. *fcntl*'s
*F_SETLK*, *F_SETLKW* and *F_GETLK* lock ranges of bytes for the process,
and *F_OFD_SETLK*, *F_OFD_SETLKW* and *F_OFD_GETLK* for an open file
description, as Linux's do; *lockf* is built on them. Whole-file locks
meet only each other, and range locks of both kinds meet each other.
Locks are advisory: they never stop a read or a write.

A process's range locks go when it closes any descriptor of the file or
ends, and a fork's child has none of them; a description's locks go with
its last descriptor. A lock in the way fails with EAGAIN, or with
*F_SETLKW* and *flock* without *LOCK_NB* is waited for until it goes or a
signal comes. A process lock that would wait in a cycle of processes
waiting for each other's locks fails with EDEADLK instead.

# MEMORY

*mmap* of anonymous memory makes a region whose pages are made, zeroed,
the first time the program touches them, and charged to its process's
resource domain. After a *fork* private pages are shared copy-on-write
and *MAP_SHARED* ones stay shared. *mmap* of a file gives the process its
own copy of the file's pages, filled as they are touched; a shared
writable mapping of a file fails with ENODEV. *mprotect* and *munmap*
work as on Linux, *msync* has nothing to write back, and
*posix_madvise* takes advice. Linux's *madvise* and its *MADV_* names
are not there, as they are not POSIX's. There is no program break:
*malloc* maps memory, and *sbrk* fails.

# LOCALE

Programs start in the C locale, as POSIX says. *setlocale(LC_ALL, "")*
with Spark's *LANG=C.UTF-8* gives the C.UTF-8 locale: UTF-8 characters
with Unicode 16's classes and case mappings, compiled from glibc's locale
data into _/system/lib/locale/C.utf8_. Asking for any other locale fails
until Phase 12 brings named locales and time zones; until then local
time is UTC.

# DYNAMIC LINKING

mlibc's dynamic linker is _/system/lib/ld.so_ and the shared C library
_/system/lib/libc.so_. A dynamically linked program's libraries are
looked for in the run path of the object that needs them, where
*$ORIGIN* is that object's directory, then in *LD_LIBRARY_PATH* and
_/system/lib_. *dlopen*, *dlsym*, *dlclose*, constructors, and
thread-local storage in libraries opened later, all work. System
programs never use it; bundles (Phase 13) may.

# RUST

Rust programs build with plain Cargo for *x86\_64-unknown-radix*
(*target_os = "radix"*, *target_family = "unix"*,
*target_env = "mlibc"*) and use the whole standard library: files and
their locks, directories, threads, processes and pipes, time,
environment, panics that unwind, and backtraces with symbols. The
*libc* crate's Radix module is checked against the C library's headers
after every build: every type's size and alignment, every field's
offset, every constant, and every function the C library must define.

# USERS

Users come with Phase 13. Until then every process is user 0 and group
0, *setuid* and its kin succeed only for 0, files have no permissions to
check, and *chmod*, *chown* and *umask* change nothing.

# TESTS

The *posix* boot test (*ob*(1)) boots with *--posix /system/tests/posix/run*:
Spark runs the test runner on a pseudo-terminal, which makes a session
of its own with it as the controlling terminal and runs each test as a
shell runs a job, in a process group of its own, in the foreground. The
tests are:

*hello*, *fork*, *exec*, *signal*, *pipe*, *socket*, *poll*, *tty*, *job*, *time*, *files*, *locks*, *thread*, *spawn*, *limits*, *format*
	C programs in _userspace/posix_, one per area. *format* holds
	*printf*, *strtod*, *scanf*, *strftime* and the time conversions to
	1,428 answers generated with glibc 2.39.

*dynamic*
	A program with a private library found through *$ORIGIN*, and a
	plugin it opens with *dlopen*.

*std-test*
	The test of Rust's standard library.

*utilities*
	The ported utilities as scripts use them, through the links in
	_/bin_: uutils' core utilities, *find* and *xargs*, each given text
	on its standard input and checked for what it writes and its exit
	status, and their manual pages.

*shell*
	The shell, *nukesh*(1): the scripts of its test suite, each giving
	what bash gives, or dash for those run as _/bin/sh_; then the shell
	at a pseudo-terminal, with its plain line reader and with its line
	editor, typed at: commands, completion, history, *^C*, and jobs
	stopped with *^Z* and brought back with *fg*.

*libc*
	mlibc's own test suite: every one of its 135 tests that apply to
	Radix (_ports/mlibc-tests/build.sh_ says why each other is left
	out). *libc:*_name_ runs only test _name_, with its output.

*libc-test*
	The musl project's tests of C libraries: every one of its 114
	functional and regression tests that apply to Radix
	(_ports/libc-test/build.sh_ says why each other is left out).
	*libc-test:*_name_ runs only test _name_, with its output.

*lua*
	Lua 5.4.7's own test suite, run as its *all.lua* runs it for users.

A test prints *<name>: every check passed* and exits with 0, or prints
each check that failed and exits with how many did. The runner prints
*posix: <test> passed* or *FAILED* for each and *posix: N of M tests
passed*; the boot test fails on any failure.

# LIMITS

- Only socket pairs until named sockets (later in Phase 9) and
  networking (Phase 12). No hard links until Btrfs
  (Phase 10: *link* fails with EPERM), no named pipes (*mkfifo* fails
  with ENOSYS) and no device nodes (*mknod* fails with EPERM). Symbolic
  links work on tmpfs and in the boot archive, not on FAT.
- A directory descriptor from before exec has no path: *..* above it
  fails with EXDEV, and *fchdir* to it with ENOSYS. One whose directory
  was moved while it was open still goes by the old path for both.
  *waitid* needs *WEXITED*.
- Futexes in memory several processes share are not shared: a
  process-shared mutex or semaphore does not wake waiters in another
  process.
- A shared writable mapping of a file fails with ENODEV.
- *clock_gettime* is a system call; there is no vDSO.
- A *poll* waits for a change anywhere, so many pollers on a busy system
  wake for each other's changes.
- No System V IPC, POSIX message queues or POSIX asynchronous I/O
  (Radix's own is its rings, *radix-ring*(2)), and no *epoll*,
  *eventfd*, *signalfd*, *timerfd* or *inotify*.
- CPU time spent in the kernel counts as user time.
- Rust's standard library cannot catch a stack overflow yet (it needs
  *pthread_getattr_np*); the program ends with SIGSEGV instead.
- Calls mlibc leaves "not implemented" end the program with a message
  in the kernel log. _docs/design/posix.md_ keeps the list.

# SEE ALSO

*radix-process*(2), *radix-signal*(2), *radix-terminal*(2),
*radix-file*(2), *radix-directory*(2), *radix-address-space*(2),
*radix-startup*(2), *radix-errors*(2), *radix-processes*(7),
*radix-files*(7), *radix-scheduling*(7), *spark*(8), *ob*(1)
