spark(8) "Dukovo" "Dukovo Manual"

; SPDX-FileCopyrightText: 2026 The Dukovo contributors
; SPDX-License-Identifier: Apache-2.0 OR MIT

# NAME

spark - the first program, which starts every other

# SYNOPSIS

Kernel command line: [*init=*_path_] [_flags_ ...] [*--* [_program_ [_argument_ ...]]]

# DESCRIPTION

Spark is the first program Dukovo runs. At the end of boot the Radix
kernel finds it in the boot archive, starts it in a process of its own,
and starts nothing else: every other program is started by Spark, or by
a program Spark started. The kernel keeps Spark's process for good, and
Spark must not end. If it does, the kernel stops with a panic that names
its exit code, since nothing could start a program again.

This page describes Spark v0, as of Phase 6 of the roadmap. It starts
the one program its arguments name, logs over the kernel log, waits for
the program to exit, reaps it and checks that all of the program's
memory came back. Then it waits. Later phases have it mount file
systems and create the directory layout (Phase 7) and supervise the
system's services, with restarts and resource domains (Phase 9).

# STARTING

The kernel starts Spark when the boot loader loaded a boot archive (a
module with the word *bootfs*, see *dukovo-boot*(7)) and no self-test
was asked for. It looks for the program at *init=*_path_ in the archive,
_/system/bin/spark_ by default, so another program can be tried as the
first one without rebuilding anything. Its arguments are its path and
then the words after a lone *--* on the kernel command line, at most 32;
the kernel's own flags end at the *--*.

Spark gets what every program gets (see *radix-startup*(2)) and three
things more:

- the boot archive, mapped read-only at a random address, whose place
  and size the start-up page gives;
- a memory object of 4 MiB in slot 8, the first of the extra slots, from
  which it makes the memory of the programs it starts;
- an image placed at a random address, as every position-independent
  program is (see *radix-processes*(7)).

The kernel logs:

```
Init: the boot archive is 79 KiB with 5 entries; starting /system/bin/spark
Init: /system/bin/spark is process 1
```

# WHAT SPARK v0 DOES

. It mounts the boot archive, which it checks whole first.
. It reads the program its first argument names, _/system/tests/hello_ by
  default, and passes it the arguments after that one.
. It starts the program in a process made for Spark, with 2 MiB of
  memory from its pool, the capabilities every program gets, and an
  exit notification for itself (radix-rt's *spawn*).
. It waits for the program to exit and logs the exit code.
. It reaps the program by deleting its capabilities, and checks that every
  page of the memory it gave came back to the pool.
. It waits for good.

Its lines in the kernel log start with *spark:*, after the kernel's
*spark[*_thread_*]:* prefix:

```
spark: Spark v0 is process 1, moved by 0x1034cc078000
spark: boot archive of 79 KiB with 2 files at 0x2075af09e000
spark: started /system/tests/hello as process 2
spark: process 2 (/system/tests/hello) exited with code 0x0
spark: reaped process 2; all of its memory came back
spark: nothing more to do; waiting
```

# EXAMPLES

Boot the default image in QEMU, where Spark starts _/system/tests/hello_:

	./ob run

Give the test program arguments, with no other kernel flags:

	./ob run --cmdline "-- /system/tests/hello one two"

The *spark* boot test (see *ob*(1)) boots with the command line
*lockdep -- /system/tests/hello one two* under UEFI and checks each of
the lines above.

# FILES

_/boot/bootfs_
	The boot archive in the boot image: a ustar file with
	_/system/bin/spark_ and the programs it starts.

_/system/bin/spark_
	Spark, in the boot archive.

_/system/tests/hello_
	The test program Spark v0 starts by default. It prints its process
	number, its arguments and how far it was moved, fills its heap, and
	exits with 0.

# DIAGNOSTICS

*Init: the boot loader loaded no `bootfs` module, so nothing starts*
	The boot image has no boot archive; the kernel stays at its prompt.

*Init:* _path_ *is not in the boot archive*
	*init=* names a file the archive does not have.

*Init: the boot archive is damaged:* _reason_
	The archive failed its check; nothing in it is used.

*Spark (process* _n_*) exited with code* _code_
	The panic when Spark ends.

*spark: could not start* _path_*:* _reason_
	Spark could not build the program's process; the reason names the
	step and the kernel's error.

# SEE ALSO

*dukovo-boot*(7), *radix-processes*(7), *radix-process*(2),
*radix-startup*(2), *ob*(1)
