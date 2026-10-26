<!--
SPDX-FileCopyrightText: 2026 The Dukovo contributors
SPDX-License-Identifier: Apache-2.0 OR MIT
-->

# Processes, loading and the first program

This note describes Radix as of Phase 6: process objects and their life,
what a process is charged for, how programs are loaded and placed at
random addresses, the boot archive, how the kernel starts Spark, and the
radix-rt runtime that Spark and later programs are built on. It ends with
the semi-formal argument that a process's end frees everything it held,
and the known limits. The user-facing descriptions are radix-process(2),
radix-startup(2), spark(8) and dukovo-boot(7); the decisions behind the
design are ADRs 0047 to 0051.

## Crates

```text
kernel/radix            user/process.rs: process objects, their methods and
                        their end; user/account.rs: charges to a resource
                        domain; user/loader.rs: the kernel's loader;
                        user/init.rs: starting Spark; user/launch.rs: the
                        self-tests' groups of programs
kernel/lib/elf          radix-elf: the checked ELF64 reader, placement and
                        relocation, shared by the kernel and radix-rt
kernel/lib/bootfs       radix-bootfs: the checked ustar reader, and a writer
                        for ob (feature `write`)
kernel/hal              cmdline.rs: `init=` and the words after `--`
userspace/libradix      entry point, system calls, printing (ADR 0036)
userspace/radix-rt      the runtime: heap, arguments, BootFs, spawn
userspace/spark         Spark (v0 in Phase 6; v1 in Phase 7, see storage.md)
userspace/hello         the test program Spark starts
tools/xtask             userspace.rs: builds the programs and writes the
                        boot archive; test.rs: the `spark` boot test
```

## Process objects

A process (`ProcessObject`) has:

- a **number**, unique since boot, and a **name** of up to 15 bytes that
  its threads take when they start and the kernel log shows;
- a **capability space** of 16 to 4,096 slots, the one its threads name
  capabilities in;
- a hidden **keep space** of 33 slots: slot 0 holds the original
  capability of its address space, and slots 1 to 32 hold what its maker
  keeps for it with `process.keep`, such as the frames of its image;
- its **address space**, until it has ended;
- the **account** its objects are charged to and its threads run in,
  in a resource domain;
- its **state**: the phase, the exit code, the threads that have not
  exited, the notification `notify_exit` named, and whether someone has
  taken on finishing it;
- the **processes made for it** (as weak references), which end when it
  does;
- an optional `on_exit` function the kernel calls once it has exited
  (for Spark: a panic).

The holder of its capability with the control right builds it from
outside: `create` makes processes, endpoints and notifications charged
to it; `set_name`; `address_space` hands out its address space so that
frames can be mapped into it; `give` moves and `share` derives a
capability into its capability space; `keep` moves one into its keep
space; `start` starts a thread at an entry point and stack; `exit` ends
it; `notify_exit` asks for a notification when it has exited; `info`,
with the read right, tells its phase, code, number and live threads.
Each program also holds its own process in `SLOT_PROCESS`, with the read
and control rights but not copy or transfer: it can end itself or start
threads, but cannot hand control of itself to anyone.

### Life

```text
            start              last thread exits, a fault, exit,
  created ────────▶ running    last capability deleted, maker ends
     │                 │                     │
     └─────────────────┴──────▶ ending ──────┘
                                  │ last thread gone: finish
                                  ▼
                               exited ── last capability deleted: reaped
```

`end(code)` moves a created or running process to **ending** with its
code, asks each of its threads to stop (`request_stop` and a wake-up, so
that a thread waiting in the kernel returns at once), and tells the
caller whether it is the one to finish it: when no thread is left and
nobody has taken that on (`take_finish`, under the state lock, so
exactly one caller does). A thread stops at its next entry into the
kernel or return from an interrupt, and the last thread of an ending
process to leave finishes it (`thread_exited`). A fault in any thread
ends the process with `EXIT_FAULT`; the last thread to exit normally
ends it with its own status.

**Finishing** ends the processes made for it, destroys both capability
spaces (which releases everything it held, and may release more
processes), lets go of the address space (its threads' own references
keep it alive while they finish leaving it), sets **exited**, signals the
`notify_exit` notification, logs a nonzero exit code, wakes anyone
waiting on the process in the kernel, and calls `on_exit`. What is left
is the number and the exit code. The registry entry, with the charge,
goes when the last capability does: that **reaps** the process.

### Ending under the store's lock

A process also ends when the capability store releases it, because its
last capability was deleted. That happens with the store's lock held, so
finishing it, which changes the store, cannot happen there and then.
`released` ends the process and, if it is to be finished, links it onto
a pending list through its own `pending` field; `caps::with` calls
`finish_pending` after every change, before the lock goes, which
finishes each process on the list with the store passed in. Finishing
one may release or end more, which join the list and are finished in the
same loop, so a long chain of processes is finished iteratively, never
by deepening recursion. Ending never allocates.

### Processes made for a process

`create` records each process it makes in the maker's list, under the
store's lock and before the new capability exists, so a failure leaves
nothing to undo but the new spaces. Once the maker has ended its list
is closed and `create` fails with `gone`. When the maker finishes, it
ends each process still alive on the list (`EXIT_STOPPED`) and puts
those with no thread left on the pending list. So a program cannot leave
anything behind: not even a process that holds the only capability to
itself, or two that hold each other's, which nothing else could reach
or end. The list prunes the records of processes that have gone each
time one is added, and lets go of nothing but those records under its
lock.

### Lock order

The capability store, then a process's list of made processes, then a
process's state, then a thread's scheduler state (`sched::wake`). The
list and the state are never held together.

## Charges

Every object `process.create` makes is charged to the target process's
account, by the bytes the kernel uses for it, before it is made:

| Object | Charged |
| --- | --- |
| process with `n` slots | the process object, an address space object, `(n + 33)` capability slots and the top page table |
| endpoint | the endpoint object |
| notification | the notification object |

An account (`Account`) keeps a byte count and charges its domain whole
pages as the count crosses page boundaries, so many small objects cost
the domain what they really use. The `Charge` is kept in the object's
registry entry and gives the bytes back when the entry goes, after the
object has closed. A new process gets an account of its own in the
target's domain, for what it makes in turn.

## Loading a program

The kernel's loader (`user/loader.rs`) starts Spark and the self-tests'
programs; radix-rt's `spawn` does the same in user space. Both use
`radix-elf`, so they read, check, place and relocate an image the same
way (ADR 0048).

1. `radix_elf::read` checks the whole image: offsets and sizes inside
   the file, segments in order and sharing no page, none writable and
   executable, a non-executable stack, the entry point in an executable
   segment, and for a position-independent image no interpreter, no
   needed libraries and only `R_X86_64_RELATIVE` relocations into its
   writable data.
2. A fixed image (`ET_EXEC`) is placed where it was linked, which must
   be between `USER_START` and the stack; its stack's top is
   `STACK_TOP`. A position-independent one (`ET_DYN`) is moved by a
   random page-aligned bias that puts it inside the 1 TiB window from
   `IMAGE_BASE`, and its stack's top is a random page in the 64 GiB below
   `STACK_TOP`.
3. The loader makes the process and gives it the capabilities
   radix-startup(2) lists: its address space, its own process (read and
   control), a memory object, the random number generator and the kernel
   log, and from `SLOT_FIRST_EXTRA` on those its starter passes.
4. It makes the stack, image and start-up page frames from the program's
   memory object, largest first, and keeps them in the keep space.
5. It copies each segment in, applies the relocations, and maps the
   segments with their own access (read-only data after relocation,
   `PT_GNU_RELRO`, read-only from the start), the stack read-write and
   the start-up page read-only at `STARTUP_ADDRESS`.
6. For Spark, it copies the boot archive into frames of its own (a
   memory object and a frame per 4 MiB, kept in the kernel's capability
   space for Spark, which has room for them however many there are) and
   maps them read-only at a random page in the 1 TiB window from
   `LOADER_BASE`.
7. It writes the start-up page: its version, CPU, role and argument, the
   time-stamp counter's frequency, the kinds of the extra capabilities,
   the free memory, the image's bias, the boot archive's place, and the
   arguments, NUL-terminated, after the fields. The thread's number is
   filled in once the thread exists but before it may run.
8. It starts the thread at the entry point with the start-up page's
   address in its first argument register.

If any step fails, the process made so far is ended, which frees
everything it was given.

### Address space layout

```text
0x0000_0001_0000   start-up page (read-only)
0x0000_0040_0000   a fixed program's image (usertests)
0x1000_0000_0000   image window, 1 TiB: position-independent programs
0x2000_0000_0000   loader window, 1 TiB: Spark's view of the boot archive
0x3000_0000_0000   PROGRAM_BASE: the program's own (radix-rt's heap)
0x7fef_f000_0000   lowest top of the stack (STACK_TOP - STACK_WINDOW)
0x7ff0_0000_0000   STACK_TOP, the highest top of the 256 KiB stack
```

That gives 28 bits of entropy to the image and the archive, as Linux's
default for `mmap` on x86-64, and 24 to the stack.

## The boot archive

`ob build` writes `/boot/bootfs`, a POSIX ustar file holding the
programs of `ARCHIVE` in `tools/xtask/src/userspace.rs` under their
directories (`system/commands/spark`, `system/tests/hello`), owned by root,
mode 0755 and dated from the commit. GRUB loads it as a module with the
word `bootfs`. `radix_bootfs::open` checks it in one pass before anything
uses it (ADR 0049), in the kernel before Spark starts and again in Spark
when it mounts it. After that, finding a file or listing a directory
cannot fail; both read the headers in turn.

## Starting Spark

After "Boot complete.", if no self-test was asked for and the boot loader
loaded a `bootfs` module (`user/init.rs`):

1. The kernel checks the archive and finds `init=PATH` in it,
   `/system/commands/spark` by default.
2. It makes a kernel capability space, which is never destroyed:
   Spark's original process capability goes in slot 0, a 4 MiB memory
   object in slot 1, and the boot archive's memory objects and frames,
   two slots per 4 MiB of it, from slot 2 on. (Until Phase 9 they were
   among Spark's 32 kept capabilities, which held an archive of at most
   56 MiB.)
3. It loads Spark with the archive, that memory object as its first extra
   capability (`SLOT_FIRST_EXTRA`, with the memory rights), its path and
   the words after a lone `--` on the command line as its arguments (at
   most 32), any CPU, the root domain, and `on_exit` set to a panic.

Spark v0 (Phase 6) mounts the archive, reads the program its first
argument names, spawns it with the rest of its arguments and 2 MiB of
memory from the pool, waits for its exit notification, reaps it, checks
that the pool has no page in use, and then waits for good on a
notification nothing signals. Since Phase 7 the kernel also gives Spark
the root of the file tree, and Spark v1 builds the hierarchy on it first
(docs/design/storage.md). Since Phase 9, Spark v2 supervises the
system's services, each in a resource domain of its own made through the
`services` domain the kernel gives it, and shuts the machine down with
the power capability (spark(8), ADRs 0098, 0099 and 0101).

## radix-rt

```text
args.rs    the arguments from the start-up page
fs.rs      BootFs: the boot archive as a read-only file system
heap.rs    the global allocator
lock.rs    a spin lock that yields while it waits
slots.rs   free slots from 48 on, in an atomic bitmap
spawn.rs   the user-space loader: spawn, Child::wait, Child::reap
vm.rs      address ranges from a random page above PROGRAM_BASE
```

`spawn` makes the child's process charged to the caller's, names it,
retypes the child's memory from the pool the caller names, reads and
places the image (as above), loads it through a temporary mapping in the
caller's own space, makes and fills the stack and the start-up page,
gives the child the capabilities radix-startup(2) lists and any the
caller shares, keeps its frames, asks for a notification on its exit,
and starts it. On any failure it ends the child and gives back what it
took. `Child::wait` waits on the notification and reads the exit code;
`Child::reap` deletes the capabilities, which reaps the process and,
once nothing made from it is left, gives the pool its pages back.

The pool exists because of how memory objects work (radix-memory(2), as
seL4's untyped memory): a memory object hands out pages from a mark
that only moves back when everything made from it has gone. A child's
memory is one object retyped from the pool, and the child makes all its
frames from that, so deleting the child's capabilities returns the whole
of it to the pool in one go.

## Correctness

The claims, and why they hold:

1. **A process is finished exactly once.** Every path that may finish
   it (`exit`, `thread_exited`, `released`, a maker's end) goes through
   `take_finish`, which sets `finishing` under the state lock and
   returns true only for the first caller that sees it ending with no
   thread left.
2. **No thread runs in a process once it is finished.** A thread is
   added to the list under the state lock only while the process is
   created or running, and `start` checks that again after making the
   thread, failing it if the process began to end in between; a thread
   leaves the list only in `thread_exited`, after it has left user mode
   for good. Finishing needs the list empty.
3. **A finished process holds nothing.** Both capability spaces are
   destroyed, which releases every capability it held and every object
   whose last capability that was; its address space is emptied, so
   every page it mapped and every page table is let go before its parent
   hears of the end, and its reference dropped; and every process made
   for it is ended.
   Its threads hold the remaining references to the empty address space
   until each is reaped. `exec` empties the old space likewise.
4. **Nothing is left behind.** Every process is reachable from its
   capabilities or from the process it was made for; when the last of
   those goes, it ends and is finished (claims 1 and 3), and when it is
   reaped its registry entry and charge go. The `fuzz` boot test checks
   this from the outside: the store's census and the registry hold the
   same before and after programs that make processes and call every
   process method at random.
5. **Ending never waits on memory or recursion.** `end` takes only the
   state lock and wakes threads; the pending list is linked through the
   processes themselves; the made list is taken whole.
6. **The lock order holds** (see *Lock order*): `released` and
   `finish_pending` run under the store's lock and take the others
   inside it; `create` takes the made list under the store's lock; no
   path takes the store's lock while holding a process's.
7. **Charges balance.** A charge is taken before an object exists and
   given back when its registry entry goes; a failed creation drops it.

The `user` boot test's probe checks every process method's errors and
success, including that a process made for a child that holds the only
capability to itself ends when the child does. The `spark` boot test
checks the whole start of the first program under UEFI: the kernel's
lines, Spark's and hello's, the arguments, both biases (page-aligned,
inside the image window, different), the exit code, and that all of the
child's memory came back. `radix-elf` and `radix-bootfs` have unit and
property tests that feed them garbage.

## Known limits

- The lower page tables a process's mappings need are made from the
  kernel's memory and not charged yet; only the top table is.
- Until Phase 9 every process ran in the root domain. Since then Spark
  makes a domain for each service it supervises, below `services`, and
  starts the service again after the out-of-memory policy stops it; the
  kernel's own stand-in supervisor remains for the domains it makes
  (ADRs 0029 and 0098).
- Spark v2 orders services only by their `order`, with no dependencies
  or readiness between them (ADR 0101).
- radix-rt's heap never gives its 64 KiB chunks back, and the address
  ranges it maps at are not reused. A freed large block stays mapped and
  is reused only for a later block of the same size (since Phase 7, when
  reading files made programs free and make large buffers in turn).
  Programs on the Rust `std` port (Phase 8) use the C library's
  allocator instead; Spark and the other radix-rt programs keep it.
- The kernel copies the boot archive into Spark's frames rather than
  mapping the module's pages; finding a file reads the headers in turn.
- A program that radix-rt's `spawn` or the kernel loads may not use
  shared libraries, an interpreter or thread-local storage; one that
  `exec` loads from a file may (docs/design/posix.md).
- A process name is at most 15 bytes, and a process's arguments must fit
  in its start-up page with the fields (about 3.9 KiB).
- A process made for another ends with it, so only Spark's children
  outlive the program that asked for them; a program that wants a
  service to outlive it asks Spark to start it (Phase 9).
