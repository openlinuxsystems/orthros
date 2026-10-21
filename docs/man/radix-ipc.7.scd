radix-ipc(7) "OrthrOS" "OrthrOS Manual"

; SPDX-FileCopyrightText: 2026 The OrthrOS contributors
; SPDX-License-Identifier: Apache-2.0 OR MIT

# NAME

radix-ipc - user mode, system calls, endpoints, notifications, channels,
rings and the random capability in the Radix kernel

# DESCRIPTION

Programs run in user mode, each in an address space and a capability
space of its own, and reach the kernel through twelve system calls.
Everything a program can do it does through a capability (see
*radix-capabilities*(7)): it sends messages to other programs over
endpoints, signals them through notifications, shares memory with them in
frames, batches work through rings, and gets random numbers from the
random capability it receives at start.

This page describes these mechanisms as of Phase 6 of the roadmap. How
programs are loaded and started, and the processes they run in, are in
*radix-processes*(7). Each system call and each interface has its own
page in section 2, generated from the interface definitions in
_abi/\*.ridl_. The design, with the argument that the IPC mechanism is
correct, is in _docs/design/ipc.md_.

# USER MODE

A program may use its address space from 0x10000 to one page below the
end of the lower half (0x7ffffffff000); the pages below 0x10000 stay
unmapped so that null pointers fault. It maps pages of frames there with
*radix_map*(2) and removes them with *radix_unmap*(2). No page is ever
writable and executable at once.

An exception in user mode, such as a page fault, a protection fault, an
invalid instruction, a breakpoint or a division by zero, ends the thread
that made it and its process, with the exit code *EXIT_FAULT*
(0xdead0000), and nothing else. The kernel logs both:

```
User: thread 9 (fault) ended: page fault writing 0x10000 (not allowed) at pc 0x401b51, sp 0x7feffffffed0
User: process 3 (fault) exited with code 0xdead0000
```

# SYSTEM CALLS

A program enters the kernel with the *syscall* instruction: the number in
*rax*, the arguments in *rdi*, *rsi*, *rdx*, *r10*, *r8*, *r9*, *r12*,
*r13*, *r14* and *r15*. The status comes back in *rax* and the results in
the same ten registers. *rcx* and *r11* are overwritten; every other
register keeps its value, and a call that fails leaves 0 in every result
register.

[[ *Number*
:< *Call*
:< *What it does*
|  0
:  *radix_cap_call*(2)
:  Calls an endpoint and waits for the reply, or calls a method of any other object
|  1
:  *radix_send*(2)
:  Sends a message on an endpoint
|  2
:  *radix_recv*(2)
:  Receives a message from an endpoint
|  3
:  *radix_reply_recv*(2)
:  Replies to the last caller, then receives
|  4
:  *radix_notify*(2)
:  Signals a notification
|  5
:  *radix_wait*(2)
:  Waits for a notification, with a timeout or as a poll
|  6
:  *radix_yield*(2)
:  Gives the CPU to another ready thread
|  7
:  *radix_map*(2)
:  Maps pages of a frame into an address space
|  8
:  *radix_unmap*(2)
:  Unmaps pages from an address space
|  9
:  *radix_ring_setup*(2)
:  Makes a ring and maps its shared pages
|  10
:  *radix_ring_enter*(2)
:  Submits ring entries and waits for completions
|  11
:  *radix_debug_log*(2)
:  Writes a line to the kernel log

The status is 0 for success. Otherwise it is
(_detail_ << 32) | (_domain_ << 16) | _code_: domain 1 is the kernel,
whose codes, with the errno value each maps to, are listed in
*radix-errors*(2), and the detail says more where it can, such as the
rights that were missing.

Every other operation is a *method*: *radix_cap_call*(2) on a capability,
with a label of (_interface_ << 16) | _method_ and the method's arguments
as the message's words. The methods every capability has (*info*,
*derive*, *move_to*, *delete*, *revoke*, *seal*, *unseal*) are in
*radix-capability*(2), with labels from 0xffff0000 up; the others are in
the page of the kind they work on: *radix-memory*(2),
*radix-address-space*(2), *radix-thread*(2), *radix-random*(2) and
*radix-ring*(2). A label the object's interface does not have fails with
*no_such_method*.

# MESSAGES

A message is an *info word* and up to eight data words, all in registers:

[[ *Bits*
:< *Meaning*
|  0 to 31
:  The label: whatever the protocol says, or a method
|  32 to 35
:  How many data words carry meaning, 0 to 8
|  36 to 38
:  How many capabilities the message carries, 0 to 4
|  40
:  Move the capabilities instead of copying them
|  41
:  Do not block: fail with *would_block* instead of waiting

The other bits must be 0. See *radix-endpoint*(2).

# ENDPOINTS

An endpoint carries synchronous messages. *radix_send*(2) waits until a
receiver takes the message, and *radix_recv*(2) until a message comes;
whichever comes second hands the message over, and the first waits in
line, first come first served. The receiver gets the sender's *badge*, so
a server tells its clients apart by the badged capabilities it gave them.
Sending needs the *send* right and receiving the *receive* right.

A *call* (*radix_cap_call*(2) on an endpoint) sends and then waits for the
reply. The thread that receives the call owes the reply, and only it can
give it, with *radix_reply_recv*(2), which then receives the next message.
If it receives again without replying, or exits, the caller fails with
*gone*. When the endpoint's last capability is deleted, every thread
waiting on it fails with *gone*; a thread that is stopped while it waits
fails with *interrupted*.

A message and its reply between two threads on one CPU take two system
calls and two switches, with no interrupt: the woken thread runs on the
waker's CPU when it may.

## Capabilities in messages

A message carries up to four capabilities when the sender's endpoint
capability has the *grant* right: the first data words name the sender's
slots, and the receiver finds in their place the slots the capabilities
arrived in. These are the first empty slots of the receiver's *receive
window*, a range of its capability space that it sets with
*radix-thread*(2)'s *set_receive_window* (programs start with slots 32 to
47). The capabilities are copied, which needs their *copy* right, or
moved with flag 40; placing them in another capability space needs their
*transfer* right too. Either every capability arrives or none does: a
window without room fails the send with *no_room*. A reply carries
capabilities the same way when the call came through a capability with
*grant*.

# NOTIFICATIONS

A notification is a word of bits. *radix_notify*(2) sets the bits of the
capability's badge (bit 0 when it has none) and wakes a waiter;
*radix_wait*(2) returns the word and clears it, sleeping while it is 0.
Signals that arrive while nobody waits add up in the word, so none is
lost, though several of the same bit count as one. See
*radix-notification*(2).

# CHANNELS

A shared-memory channel is two programs mapping the same frame, which
holds a queue of sixteen 64-byte messages each way, and each program
waiting on a notification of its own when its queue is empty or the
other's is full. A program about to wait says so in a word of the queue
and looks again first; the other end signals its notification only when
that word is set, so a program that never has to wait makes no system
call at all. The messages never pass through the kernel, which takes
part only to wake a program. Channels are part of _libradix_, not of the
kernel.

# RINGS

A ring batches operations: *radix_ring_setup*(2) makes one with a
power-of-two number of submission entries, up to 4,096, and twice as many
completion entries, and maps it into the program; *radix_ring_enter*(2)
takes new entries, carries them out and waits for completions. Nothing a
program writes into the ring's shared indexes makes the kernel write
outside the ring or lose a completion: a full completion queue stops
submission, with *busy* when nothing could be taken. A ring's pages are
charged to the creating thread's resource domain (see
*radix-scheduling*(7)).

The operations are *nop*, *notify*, *wait* (completes when a notification
is signalled, without blocking a thread), *msg_ring* (posts a completion
to another ring), *random* (16 random bytes) and *timeout*. See
*radix-ring*(2).

# RANDOM NUMBERS

Every program receives a capability to the kernel's random number
generator, the one the kernel seeds at boot (see *orthros-boot*(7)), in
slot 4. Its *read* method fills a buffer of up to 64 KiB, as *getrandom*(2)
does on Linux, and *words* returns 64 bytes in registers without touching
memory. Both need the *read* right. See *radix-random*(2).

# STARTING PROGRAMS

Every program runs in a process (*radix-process*(2)): an address space,
a capability space and its threads, which end together. The kernel starts
only the first program, Spark (*spark*(8)), from the boot archive, and
the self-tests' programs from the *usertests* image; every other program
is started by another, through radix-rt's *spawn*. Either way a program
starts with a read-only *start-up page* at 0x10000, whose address is in
*rdi*, and these capabilities:

[[ *Slot*
:< *Capability*
|  1
:  The program's thread
|  2
:  Its address space
|  3
:  A memory object for its own frames
|  4
:  The random number generator
|  5
:  The kernel log
|  6
:  Its own process, with the read and control rights
|  8 to 23
:  What its starter passes it, one kind per slot as the start-up page says
|  32 to 47
:  The receive window

The start-up page gives the program's arguments, where its image was
placed, its role and argument (for test programs), the CPU it is pinned
to if any, the time-stamp counter's frequency and its thread number; see
*radix-startup*(2). *radix-processes*(7) describes loading, the address
space layout and how processes end. The self-tests' programs are
described in *radix-debug*(7) under *user*, *ipc*, *fuzz* and *bench*.

# C AND RUST

*ob build* writes C headers of the whole interface to
_out/sysroot/include/radix/_, with an inline function per system call and
method and the errno value of every error. Rust programs use the
_radix-abi_ crate, generated from the same definitions, through
_libradix_.

# BENCHMARKS

*ob bench* (see *ob*(1)) measures what the system calls, a context
switch, a fresh page, a TLB shootdown, a memory copy and each kind of
round trip cost, on one CPU, across two, and (for system calls and
endpoint calls) on both CPUs at once, and the closest operations of
Linux beside them, in a virtual machine set up the same way: *getppid*,
*fcntl*, *getrandom*, *sched_yield*, *mmap* and *mprotect*, a pipe for an
endpoint call, an eventfd for a notification, a futex for a channel and a
Unix socket for a ring. CI runs it with *--gate* on every pull request
and merge, and fails if Radix is slower than Linux at any of them.
Numbers measured without KVM say nothing about the kernel, and the
report says so. *ob bench --instructions* counts the instructions of each
operation under QEMU instead, the same on every machine, and CI fails if
an operation on one CPU takes more than its budget.

# LIMITS

- A message has at most 8 data words and 4 capabilities.
- A ring has at most 4,096 submission entries.
- A *read* of the random capability fills at most 64 KiB.
- A kernel log line is cut after *WRITE_MAX* bytes (see
  *radix-kernel-log*(2)).
- A program started by the kernel or radix-rt has 256 slots; a process
  made directly may have 16 to 4,096.

# DIAGNOSTICS

At boot the kernel reports user mode and the test image:

```
User mode: SYSCALL/SYSRET entry, user half 0x10000-0x7ffffffff000; SMEP on, SMAP on (lifted for user copies only)
User: the usertests image is 152 KiB
```

A line a program writes with *radix_debug_log*(2) appears in the kernel
log with the program's name and thread number.

# SEE ALSO

*radix-errors*(2), *radix-capability*(2), *radix-endpoint*(2),
*radix-notification*(2), *radix-ring*(2), *radix-random*(2),
*radix-process*(2), *radix-startup*(2), *radix-processes*(7),
*radix-capabilities*(7), *radix-scheduling*(7), *radix-debug*(7),
*orthros-boot*(7), *spark*(8), *ob*(1)
