<!--
SPDX-FileCopyrightText: 2026 The Dukovo contributors
SPDX-License-Identifier: Apache-2.0 OR MIT
-->

# 0090. Editors, a C++ runtime, and the settings the system starts with

- Status: Accepted; the default settings are in `/system/resources/default-settings` since [0114](0114-plain-names-inside-system.md)
- Date: 2026-10-23

## Context

A server needs an editor at its console. The plan names nano as the
system's editor, with the key bindings most programs use on by
default, and any other one a user's `EDITOR` away. POSIX also names vi
and ex, which administrators and scripts reach for, and which `crontab
-e`, `git commit` and `less`'s `v` start when nothing else is said.

The Phase 9 command set has 7-Zip, which is in C++, as are many of the
programs later phases port. Radix had no C++ standard library: the
C library is built with libstdc++'s freestanding headers, which have
no streams, threads or exceptions to link.

Programs also look for settings in `/etc` that Radix has not had:
nano for its `nanorc`, vi for the user it runs as in `/etc/passwd`.
Spark mounts an empty tmpfs on `/system/etc` at every boot until
Phase 10 keeps it on a disk.

## Decision

**nano is the editor.** GNU nano 9.2 is a port (GPL-3.0-or-later, a
program of its own, ADR 0087). `EDITOR=nano` is in the environment
Spark gives (spark(8)). The system's `nanorc` binds the keys nano's
`--modernbindings` option gives, which a nanorc cannot turn on: Ctrl+S
saves, Ctrl+Q quits, Ctrl+C, X and V copy, cut and paste, Ctrl+Z and Y
undo and redo, Ctrl+F finds, and F1 shows the help. It turns on syntax
highlighting for the file types nano knows.

**vi, ex and view are nvi2** 2.2.2 (BSD-3-Clause): Zhihao Yuan's
continuation of Keith Bostic's nvi of 4.4BSD, which FreeBSD, NetBSD
and macOS ship, with wide characters. It keeps the lines it edits in
the 4.4BSD database library, which is a port of its own: Debian's
db1-compat 2.1.3, only the static library and `db.h`. That library is
under the University of California's license with the advertising
clause, which the University rescinded in 1999; the Port area allows
`BSD-4-Clause-UC` for it. nvi keeps recovery files in
`/var/tmp/vi.recover`. The BSD functions it needs that the C library
lacks are in the port's own `compat/`: three list macros of FreeBSD's
`<sys/queue.h>`, `getprogname`, `issetugid` (always 0: a program's file
gives it no rights on Radix) and base64. Its build is a makefile the
port writes, as nvi2's own is in CMake and expects a BSD system.

**Default settings.** The settings files the system ships go in the
boot archive under `/system/lib/defaults/etc`, and at each boot Spark
copies each one that `/system/etc` lacks into it, never replacing one
that is there. Ports add theirs (`nanorc`), and so does Dukovo
(`userspace/defaults`): `/etc/passwd` and `/etc/group` with the one
user Radix has, root (user and group 0, home `/`, shell nukesh), so
that programs that look themselves up find someone. Spark also makes
`/system/var/tmp`, for temporary files that are to outlive a restart
once `/system/var` is kept on a disk.

**The C++ runtime is LLVM's**: libc++, libc++abi and libunwind 20.1.8
(Apache-2.0 WITH LLVM-exception), the llvm-runtimes port, as static
libraries with libc++'s headers in the sysroot. They are the C++
runtime of the compiler the ports already use, under the same license
as the compiler-rt builtins every program links, and they run on
musl-like C libraries such as mlibc. Four one-line patches tell them
about Radix: the character table libc++ brings itself, the highest
error number, the stack note in libunwind's assembly, and that
libunwind finds a program's unwind tables with `dl_iterate_phdr`, as a
static program has no `_dl_find_object` to look up. Threads are
POSIX threads; the time zone database waits for tzdata (Phase 12).
GCC's libstdc++ is not used: it would add a second C++ ABI to the
system, and the GPL with an exception, for no gain.

**The toolchain gains C++ and CMake.** `x86_64-radix-c++` is the C
compiler's wrapper with Clang's C++ driver, libc++'s headers before the
C library's, and libc++, libc++abi and libunwind linked before the C
library. `out/toolchain/cmake/radix.cmake` is a CMake toolchain file
for ports that build with CMake, which describes Radix to CMake as a
Unix that takes Linux's compiler and linker settings; CMake 3.28 or
later joins the host's requirements. Ports get `CXX`, `CXXFLAGS` and
`CMAKE_TOOLCHAIN_FILE`.

**The C library gains message catalogs**: `catopen`, `catgets` and
`catclose`, musl's, which libc++'s `std::messages` needs and nvi uses.
No catalogs are installed, so programs show their own messages.

**A port can name the directories of a large repository it needs**
(`paths` in its recipe): only those are fetched, with a partial clone
checked out sparsely, and copied into its tree. llvm-runtimes fetches a
few tens of megabytes of LLVM's repository this way, not its gigabytes.

**7-Zip is `7zz`**, 7-Zip 26.02's standalone program, with every format
7-Zip reads and writes but RAR. Its source is Debian's copy of 7-Zip's,
which leaves out the RAR code, whose unRAR license forbids using it to
make a RAR compressor and so is not free; libarchive's own decoder
already lets `tar` read RAR archives (ADR 0088). 7-Zip is under the
LGPL-2.1-or-later, with parts under BSD-3-Clause and BSD-2-Clause, and
public-domain code. The Port area allows `LGPL-2.1-or-later` on ADR
0087's terms: 7zz is a program of its own, which nothing links. Debian's
patch that hides a password as it is typed is applied.

## Consequences

- `nano` and `vi` work at the console and in a pseudo-terminal, and the
  `utilities` boot test types at both. `man` has a page for each.
- C++ programs build like C ones, and the `cxx` boot test checks
  exceptions through frames and threads, RTTI, static and thread-local
  objects, streams, `std::format`, `std::regex`, threads, futures,
  clocks and `std::filesystem` at every boot. A C++ program in
  `/system` is larger than the same program in C would be: about
  300 KiB with libc++'s strings and containers, and about 700 KiB once
  it uses streams, which bring the locales.
- Every port is rebuilt when the toolchain changes, and llvm-runtimes
  takes a few minutes of that.
- `/etc/passwd` lists root alone until Phase 13 brings users. Edits to
  `/etc` last until the next boot until Phase 10.
- A setting the system ships later reaches a running system's `/etc`
  at its next boot, without undoing an edit to a file that was there.
