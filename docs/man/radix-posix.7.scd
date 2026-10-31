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
	whose interpreter is _/system/libraries/ld.so_, on the shared C library;
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
*PATH=/system/commands*, *HOME=/*, *TERM=dumb*, *LANG=C.UTF-8* and
*EDITOR=nano*, the root with the read and write rights, and a new
pseudo-terminal as descriptors 0, 1 and 2, whose output it copies to the
kernel log.

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
read, SIGILL, SIGFPE and SIGTRAP. A read of memory that went bad, which
a machine check reports, is SIGBUS with *si_code* *BUS_MCEERR_AR*,
*si_addr* the page and *si_addr_lsb* 12, as on Linux; the page has left
the program's memory, so reading it again is SIGBUS once more. A fault whose signal has no handler or
is blocked ends the process, killed by that signal. A page that finds no
memory ends the process as SIGKILL would.

Real-time signals queue with their values, up to 1,024 per process
(what *RLIMIT_SIGPENDING* says), and are taken lowest number first and
in order within a number. *kill* may signal the caller, its descendants and the
processes of its session that act for the same user (EPERM otherwise);
*kill(-1, ...)* signals all of those but the caller and process 1.
Tracing (*rtrace*(1)) reaches only the caller's descendants, as Linux's
Yama has it at level 1 (ADR 0214).

Spark, process 1, takes only the signals it has a handler for, as Linux's
init does: the others, SIGKILL and SIGSTOP included, are dropped, and a
write of Spark's to a pipe or socket nobody reads fails with EPIPE
without ending it. A fault it has no handler for still ends it, and with
it the system.

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
*st_ctim* is when the file's names, times or contents last changed on
tmpfs and Btrfs, and *st_mtim* on FAT and exFAT. *st_nlink* is its number
of names (1 for a directory), and *st_rdev* a device node's number.
*st_mode*'s permission bits, *st_uid* and *st_gid* are what tmpfs, Btrfs
and ext4 keep (ADR 0213). FAT and exFAT keep none, so their files and
directories are user 0's with mode 0755, as Linux's vfat shows them with
its default umask; the boot archive's are user 0's with the modes *ob*
gave them; and the kernel's own trees (*/devices*, */processes*,
*/kernel*) show Linux's usual modes: 0755 for a directory, 0644 for a
file, 0660 or 0666 for a device, 0600 for a pipe and 0777 for a socket.

*utimensat*, *futimens* and the older *utime* and *utimes* set a file's
times on tmpfs and FAT, which keeps them in two-second steps and the
last read only to the day, as Linux's vfat does. Times before 1970 fail
with EINVAL, and devices and pipes, which keep no times, with EPERM.
*futimens* needs a descriptor open for writing (EACCES otherwise),
where Linux lets the file's owner, and so does Rust's *File::set_times*;
the filetime crate, which the core utilities use, sets a path's times by
its path.

_/devices/fd/_*N* names descriptor _N_, and _/devices/stdin_,
_/devices/stdout_ and _/devices/stderr_ name 0, 1 and 2, as do the same
names in _/dev_, for *open*, *stat* and *access* and their *at* forms;
the shell passes pipes by these names. The C library
gives them: opening one copies the descriptor, as *dup* does, as on the
BSDs, so the copy shares its offset, never has more access than it
(EACCES), and *O_TRUNC* does nothing. A descriptor not open has no name
(ENOENT). _/devices/fd_ itself cannot be listed.

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

Sockets are *AF_UNIX* stream sockets and *AF_INET* stream (TCP) and
datagram (UDP) sockets (*NETWORK SOCKETS*, below), as *socket*,
*socketpair* and *accept* make them, with *SOCK_NONBLOCK* and
*SOCK_CLOEXEC* if asked.
Each connection is a pipe's buffer each way (*radix-process*(2)'s
*socket_pair*). *read*, *write*, *send*, *recv*, *sendto*, *recvfrom*,
*sendmsg* and *recvmsg* move bytes, with *MSG_DONTWAIT*, *MSG_NOSIGNAL*
and *MSG_WAITALL*; *MSG_MORE* is taken and does nothing. Up to
*PIPE_BUF* bytes of one send go in whole. *shutdown* shuts one way or
both, for every descriptor of the socket. *fstat* says *S_IFSOCK*.

*bind* gives a socket a name, a file of the socket kind on tmpfs, which
lasts until it is removed (EADDRINUSE if a file has the name; EPERM on
FAT, which keeps no such files); opening it fails with ENXIO. *listen*
has a named socket take connections, with a backlog of up to 4,096, and
*connect* connects to the name: it waits while the backlog is full, or
fails with EAGAIN on a non-blocking socket, and fails with ECONNREFUSED
once no socket listens there. *accept* and *accept4* take the
connection that has waited longest. *getsockname*, *getpeername*,
*recvfrom* and *accept* give a socket's name as it was bound, or an
unnamed *AF_UNIX* address. *getsockopt* answers *SO_TYPE*, *SO_DOMAIN*,
*SO_PROTOCOL*, *SO_ERROR*, *SO_ACCEPTCONN*, *SO_RCVBUF*, *SO_SNDBUF* and
*SO_PEERCRED*, which gives the process at the other end as it was when
the connection was made, with user and group 0.

*sendmsg* passes up to 253 descriptors of files and directories with
*SCM_RIGHTS*, which ride on the first byte it sends; one with no bytes
sends nothing. A receive stops before a byte that brought descriptors,
so the receive that starts there gets them, in the lowest free
descriptors, closed by exec with *MSG_CMSG_CLOEXEC*. *recvmsg* closes
those its control buffer has no room for and says so with
*MSG_CTRUNC*; *read* and *recv* close them all. At most 1,024
descriptors are on their way each way at once (ETOOMANYREFS). Sockets
of the *AF_UNIX* family are not passed (EOPNOTSUPP), so none can keep
itself open (ADR 0091); network sockets are.

*AF_UNIX* datagram and sequenced-packet sockets fail with
ESOCKTNOSUPPORT, Linux's abstract names and *MSG_PEEK* and *MSG_OOB*
with EOPNOTSUPP, and control messages other than *SCM_RIGHTS* with
EINVAL. *AF_INET6* fails with EAFNOSUPPORT until IPv6 (Phase 12c), and
other families with EAFNOSUPPORT.

## NETWORK SOCKETS

*AF_INET* sockets are the kernel's TCP and UDP (ADR 0183). A program
reaches the network only through the network capabilities it was given
(*radix-network*(2)), in the eight slots from *SLOT_NETWORK*, which last
through *fork* and *exec*; Spark gives a service those its file asks for
(*spark*(8)). *socket*, *bind*, *connect*, *sendto* and *sendmsg* to an
address try each capability in turn, and fail with EACCES when none
allows the call: one with the send right connects out to any address
and sends datagrams anywhere, one with the receive right binds to the
ports of its range and listens there. A program given none fails to
make a network socket at all. A socket that connects or sends unbound
is bound to a port the kernel picks, from 49152 to 65535; one that
listens unbound is bound to such a port through a capability with the
receive right first, as Linux binds it.

*connect* waits for the connection, or on a non-blocking socket fails
with EINPROGRESS, after which *poll* says the socket is writable once
it is made and *SO_ERROR* says how it went; connecting again says
EALREADY until then, and EISCONN once connected. A connection to a port
no one listens on fails with ECONNREFUSED. A listening socket takes up
to four handshakes at once on each interface, whatever its backlog.
Each TCP socket has 64 KiB each way, and each UDP socket 64 KiB and 16
datagrams each way. Each interface holds up to 16,384 sockets, and
ENOBUFS says it holds that many. A TCP connection closed by its program
holds no buffers in TIME-WAIT. A TCP connection over the loopback interface passes
its bytes straight into the other end's buffer, so while it does, it
holds 64 KiB each way, the receiver's, and a writer waits for the
reader once that is full. A datagram is sent and received whole: *sendmsg*
gathers its buffers into one, *recvmsg* spreads one over its buffers,
and what has no room is lost. *recvfrom* and *recvmsg* give a datagram's
sender, and no address for a stream.

*getsockopt* and *setsockopt* take *SO_REUSEADDR* (kept, changing
nothing), *SO_KEEPALIVE*, *SO_BROADCAST*, *SO_RCVBUF* and *SO_SNDBUF*
(fixed sizes), *SO_RCVTIMEO*, *SO_SNDTIMEO*, *TCP_NODELAY*,
*TCP_KEEPIDLE*, *TCP_KEEPINTVL*, *TCP_KEEPCNT* and *IP_TTL*. Keep-alive
probes go as on Linux: after 7,200 seconds of quiet, then 75 seconds
apart, and 9 unanswered reset the connection. TCP's congestion control
is CUBIC, its loss recovery uses SACK, and PAWS drops segments with old
timestamps. *getsockopt* also answers *SO_TYPE*, *SO_DOMAIN*, *SO_PROTOCOL*,
*SO_ERROR* and *SO_ACCEPTCONN*. *SOCK_RAW* fails with EPERM, and control
messages with EOPNOTSUPP.

# TERMINALS

Opening _/devices/ptmx_ makes a pseudo-terminal and gives the opener its
master side; *grantpt*, *unlockpt* and *ptsname* give the terminal side,
_/devices/pts/N_, which lasts until the master goes and hangs it up. Between
the two is POSIX's general terminal interface as Linux's N_TTY line
discipline behaves: canonical lines with their editing characters, echo,
the signal characters (^C SIGINT, ^\\ SIGQUIT, ^Z SIGTSTP), *IXON* flow
control, CR and NL translation both ways, *VMIN* and *VTIME*, and window
sizes with SIGWINCH. Settings are Linux's *struct termios*, and the
requests are Linux's *ioctl* numbers for what POSIX's terminal calls do:
*TCGETS* to *TCSETSF*, *TCXONC*, *TCFLSH*, *TIOCSCTTY*, *TIOCNOTTY*,
*TIOCGPGRP*, *TIOCSPGRP*, *TIOCGSID*, *TIOCGWINSZ*, *TIOCSWINSZ*,
*FIONREAD*, *TIOCGPTN* and *TIOCSPTLCK* (*radix-terminal*(2)).

As on Linux, typing the terminal has no room for waits: a write to the
master blocks, or with *O_NONBLOCK* stops short or fails with EAGAIN,
until a read makes room, and the master polls writable only while typing
would be taken. The signal, flow and editing characters need no room.
In canonical mode only typing past a line that fills the input, with no
complete line before it, is dropped, since no read could take it.

A terminal may be the controlling terminal of one session: the first one
its leader opens without *O_NOCTTY* while it has none, or one it asks for
with *TIOCSCTTY*. A background process of the session that reads it gets
SIGTTIN, and one that writes it with *TOSTOP* set, or changes its
settings, SIGTTOU, as job control needs. _/devices/tty_ opens the caller's
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
futexes (radix-address-space(2)'s *futex_wait* and *futex_wake*). A
futex in memory several processes map, such as a *MAP_SHARED* mapping
from before a *fork*, is the same futex in each, so process-shared
mutexes, condition variables and semaphores (*PTHREAD_PROCESS_SHARED*,
*sem_init* with _pshared_ set) work across processes. *pthread_setname_np* names a thread in up to
15 bytes, as Linux allows, and the kernel log and the debugger show the
name. *gettid* returns the thread's number.

*pthread_cancel* is deferred, as POSIX has it: the thread ends at the
next cancellation point it waits in or comes to, before the call there
has done anything, and nowhere else; its cleanup handlers run, last
pushed first, then its keys' destructors. The points are POSIX's: the
calls that may wait (*read*, *write*, their vector and positioned forms,
*send*, *recv* and their kin, *accept*, *connect*, *poll*, *ppoll*,
*select*, *pselect*, the sleeps, *pause*, *sigsuspend*, *sigwait*,
*sigtimedwait*, *sigwaitinfo*, the waits for children, *sem_wait*,
*sem_timedwait*, *pthread_cond_wait*, *pthread_cond_timedwait*,
*pthread_join*, *F_SETLKW* and *F_OFD_SETLKW*, *tcdrain* and *system*),
*open*, *openat*, *creat*, *close*, *fsync*, *fdatasync*, *msync* and
*pthread_testcancel*. As on musl, stdio, *popen*, *pclose*, *opendir*,
*closedir* and the mutexes are none. A cancel that comes while a signal
handler runs ends the wait the handler interrupted once it returns there.
A *system* whose thread is cancelled kills its command and waits for it;
SIGINT and SIGQUIT stay ignored while any thread's command runs, as on
glibc. *PTHREAD_CANCEL_ASYNCHRONOUS* ends the thread wherever it runs.

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
and *MAP_SHARED* ones stay shared, including those first touched after
the *fork*. *mmap* of a file gives the process its
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
data into _/system/libraries/locale/C.utf8_. Asking for any other locale fails
until Phase 12 brings named locales and time zones; until then local
time is UTC.

# DYNAMIC LINKING

mlibc's dynamic linker is _/system/libraries/ld.so_ and the shared C library
_/system/libraries/libc.so_. A dynamically linked program's libraries are
looked for in the run path of the object that needs them, where
*$ORIGIN* is that object's directory, then in *LD_LIBRARY_PATH* and
_/system/libraries_. *dlopen*, *dlsym*, *dlclose*, constructors, and
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
offset, every constant, and every function, which the C library must
define and declare with the same parameters and result.

# USERS

Files have owners, groups and permission bits (ADR 0213). *open*,
*mkdir*, *mknod*, *mkfifo* and *bind* give what they make the mode asked
for less the *umask*, which the kernel keeps for each process and applies
itself; it belongs to the caller's user and group, or to the directory's
group if the directory has the set-group-ID bit, which a new directory
takes too. *chmod*, *fchmod*, *fchmodat*, *chown*, *fchown*, *lchown* and
*fchownat* change them, and a *chown* takes a file's set-user-ID bit away,
and its set-group-ID bit if its group may run it, as Linux does.
*fchmodat* with *AT_SYMLINK_NOFOLLOW* on a symbolic link fails with
EOPNOTSUPP. On FAT and exFAT, which keep none, a change of permission
bits alone, or to user 0 and group 0, is taken and changes nothing, as
on Linux, so that *cp -p* and *tar* can copy to them; any other fails
with EPERM.
*fchmod* and *fchown* need a descriptor open for writing (EACCES
otherwise), as *futimens* does.

Every process acts for a name: a user, a group and other groups
(*radix-names*(2), ADR 0214), which *getuid*, *geteuid*, *getgid*,
*getegid* and *getgroups* give. A process gets it from the one that made
it, by *fork* or *posix_spawn*, and keeps it through *exec*; only a holder
of the names capability starts a process with another, as sign-in will
(Phase 13b.5). There is no setuid: *setuid*, *setgid*, *setgroups* and
their kin succeed when they change nothing and fail with EPERM
otherwise. Everything Spark starts acts for root, user 0 and group 0.

The kernel checks owners, groups and modes as Linux does (the second
gate): searching a directory needs its execute bit, reading or writing
a file its read or write bit, listing a directory its read bit, and
making, removing or renaming in a directory its write and execute bits
(in a sticky one, only the owner of the file or of the directory removes
or renames a file); a hard link needs the file's owner, or its read and
write bits, as Linux's *fs.protected_hardlinks* has it. Each fails with
EACCES. Only the owner changes a file's mode or sets its times to given
values, and to its own groups its group (EPERM otherwise); giving a file
to another user needs the bypass right. *exec* needs an execute bit,
and, unlike on Linux, a file the caller may read. *access* and
*faccessat* ask the same through *check_access* (*radix-directory*(2)).
A process whose root has the bypass right, as everything Spark starts
has, passes all of it but for running a file with no execute bit, as
Linux's root does. *opendir* and *chdir* need the directory's read and
execute bits, as on Linux; a directory opened with *O_PATH* needs
neither.

There are no login records yet, so *getlogin* fails and *logname* finds
no login name.

# TESTS

The *posix* boot test (*ob*(1)) boots with *--posix --network
/system/tests/posix/run*: Spark runs the test runner on a
pseudo-terminal, with Spark's own network capability. The runner makes
a session of its own with the terminal as the controlling one and runs
each test as a shell runs a job, in a process group of its own, in the
foreground. The tests are:

*hello*, *fork*, *exec*, *signal*, *pipe*, *socket*, *inet*, *poll*, *tty*, *job*, *time*, *files*, *locks*, *thread*, *cancel*, *spawn*, *limits*, *format*
	C programs in _userspace/posix_, one per area. *inet* runs TCP and
	UDP over the loopback interface, makes 300 connections in a row, more
	than the interface has sockets, and checks that a child with no
	network capability cannot make a network socket (EACCES), one that
	may only connect out cannot bind or listen, and one that may only
	listen on a range of ports cannot connect out. *format* holds
	*printf*, *strtod*, *scanf*, *strftime* and the time conversions to
	1,461 answers generated with glibc 2.39.

*library*
	The C library's corners, as glibc 2.39 has them: allocations near
	the top of memory and on large alignments, a null *environ*,
	*popen*, message catalogs, the functions mlibc had left
	unfinished, stacks a thread's creator gives it, and new threads'
	signal masks.

*dynamic*
	A program with a private library found through *$ORIGIN*, and a
	plugin it opens with *dlopen*.

*std-test*
	The test of Rust's standard library.

*utilities*
	The ported utilities as scripts use them, through the links in
	_/bin_: uutils' core utilities, *find* and *xargs*, GNU grep, sed,
	diff and patch, awk, bc, the compressors and their scripts, the
	archivers, *file*, *jq*, mandoc's tools and ncurses', each given
	text on its standard input and checked for what it writes and its
	exit status; the editors and the terminal tools typed at on a
	pseudo-terminal; *tmux*; the shell by each of its names; and every
	program's manual page and completion file.

*admin*
	The native administration tools (*systools*(1)) as administrators
	and their scripts use them: ps, top, pgrep, pkill and kill on
	children it starts and stops; who, w, users and uptime; free,
	vmstat and df; renice, ionice and taskset, set on children and
	inherited through *exec*; lsof on a file it holds; rtrace on a
	command, its children and a running process; dmesg and
	_/devices/kmsg_; xxd, hexdump, hd, column and tree against what the
	Linux tools write; script, watch and pidstat; shutdown and service
	through Spark; and the _/processes_ files they read (*radix-proc*(5)).

*commands*
	Every program in _/system/commands_ was run by *utilities* or *admin*,
	which note each program they run in _/tmp/posix-commands_; Spark
	and consoled, which other boot tests run, are the exceptions. A
	new program fails this test until a test runs it.

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

- *AF_UNIX* sockets are streams only, and are not passed over each
  other; *AF_INET* sockets are TCP and UDP over IPv4 only, without
  *MSG_PEEK* or *MSG_OOB*. Hard links, named pipes (*mkfifo*), device
  nodes (*mknod*) and socket names work on tmpfs and Btrfs, not on FAT
  or exFAT (EPERM); a device node opens only the devices with fixed
  Linux numbers (*radix-files*(7)). Symbolic links work on tmpfs, Btrfs
  and in the boot archive, not on FAT or exFAT.
- A directory descriptor from before exec has no path: *..* above it
  fails with EXDEV, and *fchdir* to it with ENOSYS. One whose directory
  was moved while it was open still goes by the old path for both.
- A shared writable mapping of a file fails with ENODEV.
- *clock_gettime* is a system call; there is no vDSO.
- A *poll* waits for a change anywhere, so many pollers on a busy system
  wake for each other's changes.
- No System V IPC, POSIX message queues or POSIX asynchronous I/O
  (Radix's own is its rings, *radix-ring*(2)), and no *epoll*,
  *eventfd*, *signalfd*, *timerfd* or *inotify*.
- CPU time is split between user mode and the kernel by the timer's
  samples, 100 a second, as on Linux, so a program that runs for less
  than a tick may show all its time as one or the other.
- Rust's standard library cannot catch a stack overflow yet (it needs
  *pthread_getattr_np*); the program ends with SIGSEGV instead.
- Calls mlibc leaves "not implemented" end the program with a message
  in the kernel log. _docs/design/posix.md_ keeps the list.

# SEE ALSO

*radix-process*(2), *radix-signal*(2), *radix-terminal*(2),
*radix-file*(2), *radix-directory*(2), *radix-address-space*(2),
*radix-startup*(2), *radix-errors*(2), *radix-processes*(7),
*radix-files*(7), *radix-scheduling*(7), *spark*(8), *ob*(1)
