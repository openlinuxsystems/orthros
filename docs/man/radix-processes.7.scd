radix-processes(7) "Dukovo" "Dukovo Manual"

; SPDX-FileCopyrightText: 2026 The Dukovo contributors
; SPDX-License-Identifier: Apache-2.0 OR MIT

# NAME

radix-processes - processes, program loading, address space layout and
the boot archive in the Radix kernel

# DESCRIPTION

A *process* is an address space, a capability space and the threads that
run in them, which end together. It is a kernel object like any other
(see *radix-capabilities*(7)): whoever holds its capability with the
*control* right builds it from outside, and whoever holds it with the
*read* right can see how it is doing. A native program gets exactly the
capabilities its starter gives it, and nothing is inherited. POSIX
programs also have *fork*, *exec*, signals and a process tree, which
*radix-posix*(7) describes. The interface is *radix-process*(2).

This page describes processes as of Phase 8 of the roadmap. The design,
with the argument that a process's end frees everything it held, is in
_docs/design/processes.md_.

# LIFE OF A PROCESS

A process is *created* by *process.create*, with a capability space of 16
to 4,096 slots and an empty address space. Its builder names it, maps
frames into its address space (*process.address_space* hands out a
capability to it), gives or shares capabilities into its capability space
(*process.give*, *process.share*), keeps capabilities for it where its
threads cannot reach them (*process.keep*, used for the frames of its
image), and starts its first thread (*process.start*), which makes it
*running*.

A process ends:

- when its last thread exits, with that thread's exit code;
- when a fault stops any of its threads, with *EXIT_FAULT* (0xdead0000);
- when a holder of its control right calls *process.exit*;
- when its last capability is deleted, with *EXIT_STOPPED* (0xdead0001);
- when the process it was made for ends, also with *EXIT_STOPPED*;
- for a POSIX program, when a signal ends it (*radix-posix*(7)).

While it is *ending*, each of its threads stops at its next entry into
the kernel. When the last has gone, its capability spaces are emptied,
which frees everything it held, its address space goes, and the
notification named with *process.notify_exit* is signalled. It is then
*exited*: only its number and exit code are left, which *process.info*
reads, until its last capability is deleted, which *reaps* it.

A process made with *process.create* belongs to the process *create* was
called on. So nothing a program makes outlives it, not even a process
that holds the only capability to itself. Only Spark, which the kernel
starts and never lets go of, has children that outlive the program that
asked for them; and a process made by *fork* holds its own original
capability, so it outlives its parent (*radix-posix*(7)).

# CHARGES

Every object *process.create* makes is charged to that process's
resource domain (see *radix-scheduling*(7)), by the kernel memory it
takes: a process's object, its capability spaces and its top page table,
an endpoint, or a notification. A creation that would pass the domain's
limit fails with *no_memory*. The charge goes back when the object goes.

# LOADING A PROGRAM

Programs are statically linked ELF64 executables. A *position-
independent* one (static PIE, the default for every program but the
self-tests' image) runs wherever its loader puts it; a *fixed* one runs
at the addresses it was linked at. The kernel loads Spark and the
self-tests' programs, and radix-rt's *spawn* loads every other program
the same way, with the same checks:

- Before anything is mapped, the whole image is checked: every offset and
  size inside the file, segments in order and sharing no page, none both
  writable and executable, a stack that is not executable, the entry
  point in an executable segment, and for a position-independent image
  no interpreter, no shared libraries and only relative relocations into
  its writable data.
- The segments are copied into frames, relocated, and mapped with their
  own access; data that is read-only after relocation is read-only from
  the start.
- The program gets a 256 KiB stack, a read-only start-up page at
  0x10000 (see *radix-startup*(2)) and the capabilities it lists, and
  one thread at the entry point with the start-up page's address in
  *rdi*.
- The frames of the image, the stack and the start-up page are kept for
  the process, out of its reach, so that it cannot pull its own code
  from under itself.

# ADDRESS SPACE LAYOUT

[[ *Address*
:< *What*
|  0x10000
:  The start-up page, read-only
|  0x400000
:  A fixed program's image, where it was linked
|  0x100000000000
:  The image window, 1 TiB: a position-independent program's image, at a random page
|  0x200000000000
:  The loader's window, 1 TiB: its other mappings, such as Spark's view of the boot archive, at a random page
|  0x300000000000
:  The program's own space, up to its stack: radix-rt's heap starts at a random page here
|  0x7fef00000000 to 0x7ff000000000
:  The top of the stack, at a random page

Each window gives 28 bits of randomness, as Linux's default for *mmap*
on x86-64, and the stack 24. The kernel's random number generator, ready
before the first program, draws every place. The start-up page tells a
program how far its image moved, for debuggers and crash reports.

# THE BOOT ARCHIVE

The programs the system starts with come in the boot archive, a POSIX
ustar file that *ob build* writes to _/boot/bootfs_ and GRUB loads as a
module with the word *bootfs* (see *dukovo-boot*(7)). It holds
_/system/bin/spark_ and the programs Spark starts, owned by root, mode
0755, dated from the commit, so the same programs make the same bytes.
List it with *tar tvf out/sysroot/boot/bootfs*.

The kernel checks the whole archive before it uses any of it, and Spark
checks it again when it mounts it: every header's checksum, every number
and size, every path relative, at most 255 bytes and free of *.* and
*..*, the paths in order with each directory before what is in it, at
most 4,096 entries, and only regular files and directories. radix-rt
serves it as a read-only file system to Spark. The full file system
layer comes in Phase 7.

# THE FIRST PROGRAM

At the end of boot, unless a self-test runs, the kernel starts the
program *init=* names in the boot archive, _/system/bin/spark_ by default,
with the words after a lone *--* on the command line as its arguments.
It is the only program the kernel starts, and the kernel stops if it
ends. See *spark*(8).

# DIAGNOSTICS

The kernel logs a process that ends with a nonzero code, and a thread a
fault stopped:

```
User: thread 8 (hello) ended: page fault reading 0x0 (not mapped) at pc 0x10e4ff12c3a1, sp 0x7fe2b5a1fe80
User: process 2 (hello) exited with code 0xdead0000
```

When the kernel cannot load a program, it says why, or names the step
that failed and the kernel's error:

```
User: spark cannot run: a segment is writable and executable, or not readable, or the stack executable
Init: could not start /system/bin/spark: 0x10008
```

radix-rt's *spawn* returns the same reasons to its caller, which Spark
logs.

# LIMITS

- A process's name is at most 15 bytes, and it keeps at most 32
  capabilities out of its threads' reach.
- A program's arguments must fit in its start-up page, about 3.9 KiB.
- A program that radix-rt's *spawn* or the kernel loads may not use
  shared libraries, an interpreter or thread-local storage; one that
  *exec* loads from a file may (*radix-posix*(7)).
- The page tables below the top one are not charged to the process's
  domain yet, and every process runs in the root domain until Spark
  makes domains for the services it supervises (Phase 9).

# SEE ALSO

*radix-process*(2), *radix-startup*(2), *radix-memory*(2),
*radix-capabilities*(7), *radix-ipc*(7), *radix-scheduling*(7),
*spark*(8), *dukovo-boot*(7)
