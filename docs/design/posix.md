<!--
SPDX-FileCopyrightText: 2026 The Dukovo contributors
SPDX-License-Identifier: Apache-2.0 OR MIT
-->

# The POSIX layer: processes, signals, terminals and the C library

This note describes Radix's POSIX layer as of Phase 8: anonymous memory
and copy-on-write, the process tree, signals, `fork` and `exec`, file
descriptors and paths, pipes and `poll`, pseudo-terminals, clocks and
timers, futexes, file locks, scheduling and limits; then the C library
(mlibc and its Radix system layer), the toolchain and ports, Rust's
standard library, dynamic linking and the locale. It ends with the lock
order, the semi-formal arguments that no wake-up or timer is lost and
that every lock goes with its owner, and the known limits. The
user-facing description is radix-posix(7), with radix-process(2),
radix-signal(2), radix-terminal(2) and radix-file(2); the decisions are
ADRs 0060 to 0075.

## Crates and files

```text
kernel/lib/posix        radix-posix: the process tree, signal sets, actions
                        and pending queues, wait statuses, exec blocks, #!
                        lines and the System V initial stack (host-tested)
kernel/lib/tty          radix-tty: byte rings and the line discipline
                        (Ldisc, Settings), POSIX's terminal interface
                        (host-tested)
kernel/fs/vfs/locks.rs  radix_vfs::locks: what a file's list of advisory
                        locks becomes (host-tested, with a model)
kernel/lib/elf          radix-elf: read_program, for exec (interpreters and
                        thread-local storage allowed)
kernel/radix/src/user/memory/anon.rs    anonymous regions, faults, fork
kernel/radix/src/user/memory/futex.rs   futex_wait and futex_wake
kernel/radix/src/user/posix/            the tree, signals, fork, exec,
                        timers, poll, priority and limits, as radix-process(2)
kernel/radix/src/fs/pipe.rs   pipes, on the detached pipefs
kernel/radix/src/fs/tty.rs    pseudo-terminals, /dev/ptmx, /dev/pts, /dev/tty
kernel/radix/src/fs/locks.rs  the table of file locks, and lock waits
ports/                  the ports: mlibc (with sysdeps/radix, patches and
                        musl/), mlibc-tests, freestnd-cxx-hdrs, locales,
                        rust-std, lua
tools/xtask/src/ports.rs      ob ports: fetching, patching, building, the
                        toolchain wrapper and the Meson cross files
tools/xtask/src/libc_check.rs the libc crate's Radix module against mlibc
userspace/posix/        the C tests and their runner; dynamic/
userspace/programs/     Rust programs on std (std-test)
userspace/targets/      the x86_64-unknown-radix target
```

## Why the kernel keeps POSIX's state

Radix's own model is capabilities: a process belongs to whoever holds its
capability, and a program gets exactly what its starter gives it. A C
library could build much of POSIX on that alone, as WASI's does, but some
of it must span processes or happen between two instructions of a
program: the tree that `wait` reads after the parent's address space has
changed hands, process groups for job control, signals that interrupt a
thread wherever it is, a pipe that knows when the last writer in any
process has gone, and the controlling terminal of a session. Those are in
the kernel; their rules are plain data in `radix-posix` and `radix-tty`,
with no locks and no `unsafe` code, so that host tests and property
tests check them. Everything else (paths against a working directory,
`select`, `posix_spawn`, stdio, locales) is the C library's.

Radix takes Linux's numbers for what programs see (errno values, signal
numbers and frames, `struct stat`, `termios` and `ioctl` requests,
clocks, the auxiliary vector), so that mlibc's Linux ABI headers, Rust's
`libc` crate and ported programs describe it with little new
(ADR 0068).

## Anonymous memory and copy-on-write

An address space holds anonymous regions besides frames: runs of
addresses with an access and nothing behind them yet, sorted in a list
under the space's regions lock, which a fault, `mmap` or `munmap`
searches by halves, so a program with many mappings pays little for
them. A fault in one (or a kernel copy to or
from user memory that faults there) makes the page: a frame from the
allocator owned by `Anon`, zeroed or filled from the file the region
maps, charged to the space's domain. Entries for pages under `PROT_NONE`
keep their frame without access, so `mprotect` loses nothing.

The page allocator's record of each frame counts the spaces that map it.
`fork_into` gives the child an entry for every page the parent has and
counts each shared; a private page is then mapped read-only in both. A
write fault on a page shared by more than one space copies it for the
writer: the copy is mapped writable (with the old entry's TLB entries
gone) before the original's count drops. A page whose count is one is
made writable in place. A `MAP_SHARED` region stays writable in both.
Every page a space maps is charged to its domain once, so a fork charges
the child's domain for everything up front and a domain's limit bounds
what its programs can touch.

`exec` and `mmap` of a file map the file's pages as the process's own
copy, filled when touched (ADR 0060). A shared writable mapping of a file
needs the page cache to map its own pages, and is refused until then.

## The process tree

One tree (`TREE`, a spin lock around a `radix_posix::tree::Tree`) holds
each member's number, parent, process group, session, life (running,
stopped, zombie) and what its parent's `wait` will read, with the
member's `ProcessRef`, so that a zombie lasts until it is waited for. A
process joins when it is forked, when `exec` starts a process `create`
made, or when it first calls a method that needs the tree, as the root of
a session of its own (Spark, and programs started the Phase 6 way).

A process is reported to its parent when it finishes
(`ProcessObject::finish`), which may run with the capability store
locked, so nothing that holds the tree takes the store. A process with no
parent is reaped at once; a child whose parent ends loses its parent.
There is no init that adopts orphans: Radix's capabilities already say
who may control a process, and Spark supervises what it starts through
its own exit notifications. `kill` may reach the caller, its descendants
and its session, the nearest rule to POSIX's same-user rule while every
process is user 0.

## Signals

A process has a `signal::Actions` (one action per signal) and pending
signals for the process; each thread has a mask, an alternate stack and
pending signals of its own (`ThreadSignals`). Standard signals pend as a
bit; real-time signals queue with their values, up to `QUEUE_MAX`
(1,024) per process, taken lowest number first.

Sending (`send_process`, `send_thread`) does what the signal does at once
(SIGCONT continues; SIGKILL, and a default action that ends the process,
end it), makes it pending, picks a thread that does not block it, marks
that thread `signalled` and wakes it, so a wait it is in ends as
interrupted. On every return to user mode, `on_return` calls `deliver`,
which takes the thread's signals one at a time: a stop stops the whole
process (each thread waits in `deliver` until SIGCONT), and a handler gets
Linux's x86-64 frame on the thread's stack or alternate stack: the
restorer, a `ucontext_t`, the `siginfo_t`, and the XSAVE area, 64-byte
aligned below the red zone (`UserMode::push_signal_frame`).
`signal_return` restores every register from the context, so a handler
may change them, and returns through the slow path, which restores all
registers rather than SYSRET's subset.

A system call a signal interrupts says how it may be restarted
(`Restart`): `Always` (a pipe read, a lock wait) runs again when no
handler ran or the handler has SA_RESTART; `NoHandler` (poll,
sigsuspend, sigtimedwait, timed futex waits) only when no handler ran;
`Never` fails with EINTR.

Faults become signals for the faulting thread. One whose signal is
blocked or has no handler ends the process with that signal, as Linux
does, rather than looping on the fault.

## fork and exec

`fork` builds the child completely before it runs: a capability space of
the caller's size with every capability that has the copy and transfer
rights copied to the same slot with its slot flags (the same open file
description for a file), the kept capabilities, the address space by
`fork_into`, the signal actions, terminal, limits and mask, and one
thread with the caller's registers, extended state and thread pointer.
The child holds its own original capability, so it does not end with
the caller (ADR 0063).

`exec` has two halves. The first does everything that can fail in a new
address space no thread runs in: reads the program's headers through
`radix_elf::read_program`, maps its segments as anonymous regions filled
from the file, maps an interpreter or runs a `#!` interpreter (one
level), applies a static program's relative relocations, writes the
System V stack (`radix_posix::exec`) below a random top and maps the
start-up page. Only then does the second half touch the caller: stop its
other threads, swap the space in (remaking the original capability in
the hidden slot 0, so every capability to the old space goes), empty the
`close_on_exec` slots, reset handled signals, delete POSIX timers, and
start the new thread. On a process `create` made that has not started,
the second half instead joins it to the tree as the caller's child and
starts it: `posix_spawn`'s way, which Spark uses.

## File descriptors and paths

Descriptor _n_ is slot `SLOT_FD_BASE` (64) + _n_; slots below are the
start-up slots every program has. A file capability is the open file
description, so `dup`, `fork` and `exec` need nothing from the C library.
`close_on_exec` is a flag on the slot in the capability store, so a copy
of a capability does not inherit it.

The kernel resolves every path beneath a directory capability and never
above it (ADR 0054), following symbolic links on the way (ADR 0081). The
C library keeps the working directory as a canonical absolute string. It
passes the kernel an absolute path as it is, beneath `SLOT_ROOT`, and a
relative one joined to the working directory; it works out no `.` or
`..` itself, since only the kernel knows where a link leads. `chdir`
and `realpath` ask the kernel for the canonical path (`real_path`).
Directory descriptors (`openat`) go to the kernel with their capability,
beneath which `..` fails with `outside` (EXDEV). So the C library notes
the path, beneath the root, by which each directory descriptor was
opened (ADR 0089), copies it with the descriptor on `dup`, and forgets
it on `close`: a path with a `..` beneath a descriptor goes to the
kernel from the root by that path, and `fchdir` changes to it. A
descriptor from before exec has none, so `..` above it still fails
with EXDEV and `fchdir` with ENOSYS.

`/dev/fd/N`, `/dev/stdin`, `/dev/stdout` and `/dev/stderr` are the C
library's (ADR 0084): `open`, `stat` and `access` match the resolved
path and work on the descriptor, as `dup` and `fstat` do, since no
filesystem differs from one process to the next.

## Pipes

A pipe is a `radix-tty` `Ring` of `CAPACITY` (64 KiB) under a sleeping
lock, with two nodes on `pipefs`, a filesystem mounted nowhere. Each end
is a node, so its open file descriptions are ordinary file capabilities,
and the pipe learns an end has gone when its node drops. The waits are
the kernel's wait queue's on the pipe's address, with conditions that
read only atomic counts kept beside the ring. A write of up to `ATOMIC`
(4,096) bytes waits for room for all of it, so writers never interleave;
copies to and from user memory are made with the ring's sleeping lock
held, so a page fault in the copy may sleep.

## Sockets

A socket pair (ADR 0085) is two pipes, one each way: each socket is a
node on `pipefs`, and its end of the connection counts as the reader of
one pipe and the writer of the other, so reads, writes, waits, SIGPIPE
and poll are the pipe's. An end's `shutdown` lets go of its reader or
writer count early, once, with a flag per direction; dropping the end,
with the socket's node, lets go of what is left.
`send` and `receive` are the pipe's write and read with the caller's
`dont_wait` and `no_signal`.

A named socket (ADR 0091) moves once from fresh to listening or to
connected. `bind_socket` makes a tmpfs node of the socket kind holding a
`Binding`, through which a connecting socket finds the `Listener` while
the socket listens; `connect_socket` makes the two pipes, keeps one end
and queues the other at the listener, and `accept` gives the queued end
a socket of its own. The socket's state is a spin lock taken only to
move its phase; the listener's queue is a sleeping lock, and its waits
read an atomic count beside it. Files sent with `send_files` ride on a
byte of the pipe: each pipe keeps a queue of attachments, each the
position of its byte in all that was written and the files, and a read
ends before the next attachment's byte unless it starts there, so the
files come out with their bytes. A socket is never sent, so the files
in flight hold no socket, and no cycle of sockets can keep itself alive.

## poll

`poll` copies the `pollfd` array in, looks at each slot, and if none is
ready waits for a change anywhere: everything a poll looks at (pipes,
terminals, notifications) calls `changed` after it changes, which moves
the global `GENERATION` and wakes every poller. A woken poller looks at
all its entries again. This costs a barrier and a load per change while
nobody polls, and wakes every poller on every change while some do: fine
for a shell and its jobs, and the first thing to replace with per-object
wait lists when a profile shows many pollers (see the limits).

## Terminals

A pseudo-terminal is an `Ldisc` and two rings behind the terminal's lock,
with a node per open of either side (`Handle`), so it counts the
descriptions of each. The master is what drives a screen; the terminal
side is what programs hold. What is written to the master is fed to the
line discipline as typed input, which edits lines, echoes into the output
ring, and names signals for the foreground group; what programs write
goes through output processing to the master's reads.

A terminal remembers the session it controls and its foreground group;
each process remembers the terminal its session had when it looked
(`PosixState::terminal`), which forks inherit. Job control checks are
made with the terminal's state, then the tree, locked in that order, and
signals are sent with neither held. `/dev/tty` opens the caller's
controlling terminal through that remembered handle.

## Clocks and timers

Clocks are read through radix-process(2)'s `clock`: the wall clock from
the hardware, the monotonic and boot-time clocks from the scheduler's
clock, and CPU time from what the scheduler counts at ticks. A sleep is a
timed futex wait on a word nobody wakes, which reports what was left.

Each process's `PosixState` holds its three interval timers and its
POSIX timers. One kernel thread runs them all (`expire_due`): it copies
the processes in `ARMED` (weak references, so a process that ended drops
out), sends the signal of each timer that has expired, and sleeps until
the next timer on the kernel's clock, or for `CPU_TICK_NS` (10 ms) while a
CPU-time timer is armed. A process whose timers are all off is taken out
of `ARMED` (`unregister`). A POSIX timer with an interval waits until its
signal is taken, then counts the expiries missed meanwhile as overruns
(`taken`); one whose signal was dropped while pending is set again
(`discarded`).

## Futexes

A futex word is named by its address space and address, hashed into one
of 64 buckets, each a spin lock around a list of waiters in arrival
order. `futex_wait` compares the word with the bucket locked and queues
the waiter before letting go; `futex_wake` takes the same lock. A woken
waiter is taken off the list by its waker and told through its own flag;
one that leaves for another reason (a timeout, a signal) takes itself
off unless a waker already did, which then counts as its wake-up, so a
wake is never spent on a waiter that has gone.

## File locks

The kernel keeps every file's locks in one `TABLE`, a spin lock taken
after every other lock, holding for each file with locks its node's
address, a weak reference that keeps the address from being reused, and
its locks. What a list becomes is `radix_vfs::locks::set`, pure code with
a property test against a model that records each owner's kind for every
byte. A lock in the way makes the caller wait on a key derived from the
node's address until `CHANGES` moves; every release and every change
moves it with the table locked and wakes that key. Before a process lock
waits, `deadlocks` follows the chain of waiting processes from the holder
(at most `CHAIN_MAX`, 10 links, as Linux's MAX_DEADLK_ITERATIONS) and
fails with EDEADLK if it comes back to the caller.

Locks go with their owners: a description's when the description's last
reference drops (`description_gone`, from the `FileObject`'s drop, which
may run with the capability store locked, hence the leaf lock); a
process's on a file when it deletes any capability of the file
(`capability.delete`, and `exec`'s emptying of `close_on_exec` slots),
and on every file when it finishes (`process_gone`).

## Scheduling and limits

`priority` and `scheduling` keep each program thread's POSIX policy,
real-time priority and nice level (`Scheduling`) and set the scheduler's
`Policy` from them (ADR 0074). A thread starts with its maker's. The
rules for asking for more CPU are Linux's for a process without
`CAP_SYS_NICE`, through `RLIMIT_NICE` and `RLIMIT_RTPRIO`. Limits are
sixteen `Limit`s in `PosixState` with Linux's defaults; the kernel uses
the stack limit (to size a new program's stack), the nice and real-time
limits, and the C library the descriptor limit.

## The C library

mlibc 7.0.0 is built with Meson for no OS, with its POSIX, glibc and BSD
options and not its Linux one (ADR 0068). The Radix system layer,
`ports/mlibc/sysdeps/radix`, is C++: each `Sysdeps<Tag>` turns one C
library call into capability methods through the C headers `ridlc`
generates, and maps the kernel's status word to an errno (each kernel
error names its errno in `abi/errors.ridl`). Its files share
`radix.hpp`: the start-up slots, descriptors as slots, the working
directory and path resolution, and stat filling. Its `abi-bits` link to
mlibc's Linux ones, but for `ioctls.h` and `auxv.h`.

mlibc's own code is changed only by patches, each with what it changes
and why:

| Patch | What |
| --- | --- |
| 0001 | Meson: the `radix` system |
| 0002 | frigg's `printf` fixes from upstream |
| 0003 | `printf` as glibc does: `%p`, `%c`, `%s`, zero precision, `%C`, `%S`, positional arguments, unknown conversions |
| 0004 | Exact floating-point conversions from musl 1.2.5 (`printf`, `strtod`, `scanf`) |
| 0005 | stdio's errors as glibc's and musl's |
| 0006 | `strftime` and `wcsftime` from musl 1.2.5 |
| 0007 | Time conversions from musl 1.2.5's calendar code |
| 0008 | Random temporary-file names from `getentropy` |
| 0009 | Locales in `/system/lib/locale`, named as asked, `MB_CUR_MAX` |
| 0010 | `getpriority`'s failure value |
| 0011 | `preadv` and `pwritev` |
| 0012 | `/system/bin` as the default program path, and `confstr`'s failure |
| 0013 | `lrand48`, `nrand48` and `lcong48` from musl 1.2.5 |
| 0014 | Every resource limit's name, whatever the feature macros |
| 0015 | `strtol`'s bases, `memmem` and `wcsstr` with an empty needle, `wctob` |
| 0016 | Running out of memory: the allocator, `malloc`, `realloc` and a thread's TLS |
| 0017 | `setenv`, `putenv`, `unsetenv` and `clearenv` as POSIX says, out of memory included |
| 0018 | `scanf` and `wscanf` on musl 1.2.5's, and stdio's positions, errors, `fmemopen`, `fputc` and wide output |
| 0019 | Threads and processes: the last `pthread_exit`, mutex types, unlocking without a system call, process-wide `pthread_atfork`, `daemon` |
| 0020 | `strptime`'s conversions as POSIX has them |
| 0021 | `inet_pton` and `inet_ntop` from musl 1.2.5 |
| 0022 | `raise` signals the calling thread; the C library's own signals kept out of programs' masks |

Patches 0013 to 0022 fix what libc-test found (below); each says which
of its tests it answers. One of mlibc's own tests changes with patch
0021 (`ports/mlibc-tests/patches`): as in glibc and musl, `inet_pton`
refuses leading zeros in an IPv4 address and lets any IPv6 address end
in a dotted IPv4 one, where mlibc did the opposite.

The musl code lives in `ports/mlibc/musl/` with musl's copyright lines,
and `build.sh` copies it into the source (ADR 0072).

The C library is built twice from the same objects' sources: a static
`libc.a` with its start files for system programs, and a shared
`libc.so` with mlibc's dynamic linker, `ld.so`, for bundles (ADR 0071).
The system layer is built into the dynamic linker as Linux's is, with
`MLIBC_BUILDING_RTLD` leaving out what only the C library proper may
call; compiler-rt's builtins are linked into both.

## The toolchain and ports

The toolchain is the build host's Clang 20, LLD and LLVM tools, wrapped
by scripts `ob` writes (ADR 0069). `x86_64-radix-cc` compiles for
`x86_64-unknown-linux-musl` (Linux's x86-64 ABI) with `-nostdinc`, the
sysroot's headers, Linux's macros undefined and `__radix__` and
`__dukovo__` defined. When it links, it adds the start files, the C
library and the builtins around the caller's arguments, in one of three
modes: static (`-static-pie`, the default), `-dynamic` (a PIE whose
interpreter is `/system/lib/ld.so`) and `-shared`. The C library goes in
with `-Bstatic` or `-Bdynamic` stated, since rustc ends its arguments
with `-Bdynamic`.

A port is a recipe, patches and a build script; `ob ports` fetches each
source by commit into `target/ports/sources`, applies the patches to a
fresh copy in `target/ports/work`, and runs the script with the toolchain
in `PATH`, rebuilding a port only when its directory, the toolchain or a
port it depends on changed. Every port's license and notices go into the
image's license manifest.

## Rust's standard library

The `rust-std` port builds `std` for `x86_64-unknown-radix` from the
pinned nightly's `rust-src`, offline from the crates it vendors, with
five patches (ADR 0070): the `radix` OS in `std` (mostly Linux's and the
Hurd's Unix code, with Radix's stat fields and the program path from the
auxiliary vector), symbols in `backtrace` through `dl_iterate_phdr` and
gimli, the pure-Rust `unwinding` crate in `unwind`, a Radix module in
the `libc` crate, and `unwinding` itself. After every build,
`libc_check` compiles a C file of expressions with Clang to LLVM IR and
compares each type's size and alignment, each field's offset and each
constant with the `libc` module's, and checks that the C library defines
every function the module declares. It found that mlibc aligns
`struct sched_param` to 8 bytes, where the module had 4.

## Dynamic linking

`ld.so` searches the needing object's run path (with `$ORIGIN`), then
`LD_LIBRARY_PATH`, then `/system/lib`. The `dynamic` test builds a
bundle's shape: a program, a private library in `lib/` beside it found
through `$ORIGIN/lib`, with a constructor, data and thread-local storage,
and a plugin opened with `dlopen` whose thread-local storage each thread
gets when it first asks. Bundles' own layout and search path come with
Phase 13.

## The locale

The `locales` port compiles C.UTF-8 from glibc 2.42's locale sources
with the host's `localedef` (ADR 0073). mlibc reads glibc's compiled
format; it looks in `LOCPATH`, then `/system/lib/locale`.

## Lock order

From first to last:

1. the capability store;
2. the armed timer list (`ARMED`), never with the tree held;
3. a terminal's state;
4. the process tree (`TREE`);
5. a process's POSIX state;
6. a process's state (its thread list);
7. a thread's signals;
8. a thread's scheduler state.

A space's regions lock is taken with none of these held but the
capability store, and a futex bucket with none at all. The file locks'
`TABLE` is a leaf, taken after every other lock. Signals are never sent
with the tree or a terminal locked, since sending one may end a process,
whose finish takes the tree.

## Correctness

The kernel's waits follow one pattern: read a word, decide to wait, and
sleep on the kernel's wait queue with a condition that rereads the word
after the waiter is queued. Each argument below shows that the change a
waiter needs either happens before it reads the word, or wakes it after
it is queued.

**No poll misses a change.** A poller increments `POLLERS`, then reads
`GENERATION` and looks at its entries; `changed` makes its change, then
reads `POLLERS`, and moves the generation and wakes if any poller is
counted. Both sides have a sequentially consistent operation between
their write and their read (the poller's increment, and `changed`'s
barrier or the notification's read-modify-write). So either the poller's
look sees the change, or `changed` sees the poller and moves the
generation after the poller read it, and the poller's condition
(generation moved) is true when it is queued or it is woken once queued.

**No signal is missed by a thread about to wait.** A sender sets the
pending bit and the target thread's `signalled` flag with the process's
POSIX state and the thread's signals locked, and then wakes it. The
thread clears `signalled` and checks for pending signals under the same
two locks before it decides it has nothing to do, and every
interruptible wait's condition includes `sched::interrupted`, which reads
the flag. So a signal sent before the check is seen by the check, and one
sent after it sets a flag the wait's condition reads.

**No armed timer is forgotten.** Setting a timer takes `ARMED`'s lock,
puts the process in the list if it is not there, sets the timer in the
process's POSIX state, and only then lets the list go. `unregister`
takes the same lock and removes the process only if none of its timers is
set. So `unregister` runs either before the setter takes the lock (and
the setter puts the process back) or after it lets go (and sees the
timer set). Before this was so, `unregister` could see no timer, the
setter put the process in (it was already there) and set the timer, and
`unregister` take it out: the timer never fired. The `signal` test's
`rearms` part set and cleared timers 300 times with varying gaps, and
hung on that kernel within a few runs.

**No futex wake is lost.** `futex_wait` compares the word and queues the
waiter in one critical section of the bucket's lock; a waker changes the
word and then takes that lock to wake. If the waker's change came before
the comparison, the comparison fails and the waiter returns at once; if
after, the waiter is on the list when the waker looks.

**No lock waits for a release that has happened.** A lock waiter reads
`CHANGES` before it takes `TABLE`; every release and every change of a
file's locks moves `CHANGES` and wakes the file's key with `TABLE`
locked. So a release before the waiter's look is seen by the look, and
one after it moves `CHANGES` past the value the waiter's condition
compares.

**Every lock goes with its owner.** Each kind of owner has exactly one
end: a description's last reference drops once (`FileObject`'s drop
calls `description_gone`), and a process finishes once (`finish` calls
`process_gone` for every file). POSIX's extra rule, that closing any
descriptor of a file lets go of the process's locks on it, is applied by
both ways a descriptor closes: `capability.delete` and `exec`'s emptying
of `close_on_exec` slots. The table's lock is a leaf, so these may run
under any other lock, the store's included.

**A failed exec leaves the caller as it was.** Everything that can fail
(reading and checking the program, finding the interpreter, making the
space, its regions, its stack and start-up page, and charging them)
happens in a space nothing runs in. The caller is touched only after
the last fallible step.

**A write after fork is seen by one side only.** After `fork_into`
every private page is read-only in both spaces with a count of two. The
first write faults; the writer maps a copy and only then lowers the
original's count. The other space can make the original writable in
place only once the count is one, which is after the writer stopped
mapping it, so no page is writable in two spaces unless its region is
shared.

These arguments are not yet checked by a model; the timer race above
shows the kind of bug one would catch, and TLA+ models of the poll, futex
and timer protocols are candidates for the audit after Phase 9.

## Known limits

- Futex words in `MAP_SHARED` memory are named by each process's own
  space, so a wake in one process does not reach waiters in another:
  process-shared mutexes, condition variables and semaphores do not
  work across processes yet.
- A shared writable mapping of a file fails with ENODEV, and `msync` has
  nothing to write back: file mappings are private copies until the page
  cache maps its own pages.
- `clock_gettime` is a capability call; there is no read-only clock page
  (vDSO). The instruction budgets will show when it matters.
- `poll` wakes every poller on every change anywhere.
- A directory descriptor's path is the one it was opened by, and one from
  before exec has none (ADR 0089); `getcwd` is the C library's string.
- No named pipes (`mkfifo` fails with ENOSYS: no filesystem keeps them
  yet), no device nodes (`mknod` fails with EPERM), no hard links (EPERM)
  until Btrfs. Symbolic links work on tmpfs and in the boot archive;
  FAT has none (EPERM).
- `chmod`, `chown` and `umask` change nothing until users (Phase 13).
- The kernel keeps no time of the last change to a file's status, so
  `st_ctim` is `st_mtim`; reading a file never changes `st_atim`
  (ADR 0076).
- `waitid` needs `WEXITED`: the kernel always reports children that
  ended.
- CPU time is split between user mode and the kernel by the timer's
  samples, 100 a second, as Linux splits it (`times`, `getrusage`,
  `wait`'s times; ADR 0092); CPU-time timers fire within a tick (10 ms).
- Only `AF_UNIX` stream sockets, which pass no sockets: network
  sockets come with Phase 12. No System V IPC, POSIX message queues, POSIX
  asynchronous I/O, `epoll`, `eventfd`, `signalfd`, `timerfd` or
  `inotify`. Linux's `madvise` is not declared; `posix_madvise` is
  advice only.
- mlibc's hard-coded paths outside `/system` stay as they are until the
  phases that bring them: `/bin/sh` for `system`, `popen` and `wordexp`
  (Phase 9 decides where the shell is), `/etc/localtime`, `/etc/hosts`,
  `/etc/resolv.conf` and `/etc/protocols` (Phase 12), `/etc/passwd`,
  `/etc/group` and `/etc/shadow` (Phase 13).
- Rust's standard library cannot catch a stack overflow: it needs
  `pthread_getattr_np` for the guard page's place.
- Library search does not yet know bundles' layout (Phase 13).

mlibc 7.0.0 ends the program (SIGABRT, with a line in the kernel log)
when one of these is called, since it has no code for them:

- POSIX option: `getcontext`, `setcontext`, `makecontext` and
  `swapcontext`; `prlimit`; `remap_file_pages`; `semop`; `recvmmsg` and
  `sendmmsg`; `getdomainname` and `setdomainname`; `inet_makeaddr` and
  `inet_netof`; `fgetln` and `tempnam`;
  `pthread_rwlockattr_setkind_np` and `pthread_rwlockattr_getkind_np`.
- glibc option: `adjtimex`, `clock_adjtime` and `ntp_adjtime`;
  `backtrace_symbols`; `parse_printf_format`; `res_query`, `dn_comp` and
  `dn_expand`; `getsgnam_r`; `__freadable`, `__fwritable`, `__freadptr`,
  `__freadptrinc` and `_flushlbf`.
- BSD option: `ether_aton`, `ns_initparse`, `ns_parserr` and
  `ns_name_uncompress`.

A call whose system layer function Radix does not provide fails with
ENOSYS instead. Each is fixed when a port needs it, upstream where it is
general.
