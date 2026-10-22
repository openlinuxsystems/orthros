<!--
SPDX-FileCopyrightText: 2026 The Dukovo contributors
SPDX-License-Identifier: Apache-2.0 OR MIT
-->

# 0068. mlibc is the C library, with Radix's own system layer and Linux's ABI numbers

- Status: Accepted
- Date: 2026-10-22

## Context

The plan names mlibc as the C library (spec 7.1): a permissively
licensed (MIT) C library written to be ported, with every system
dependency behind one layer of `sysdeps`, its POSIX, glibc and BSD
interfaces as separate options, and a dynamic linker of its own. The
alternatives were musl, which assumes Linux's system calls throughout,
and newlib or a library of our own, which would leave POSIX's harder
parts (threads, locales, `printf`'s floating point, the dynamic linker)
to write. Radix is not Linux: it has capabilities and methods, not
Linux's system calls. But it takes Linux's numbers wherever it can
(errno values, signals, `ioctl` requests, structure layouts), so that
headers, ported programs and Rust's `libc` crate can describe it with
little new.

## Decision

- mlibc **7.0.0** (`ports/mlibc`, pinned by commit, with its frigg and
  libsmarter subprojects at the commits its wrap files pin) is the C
  library, built with the host's Clang 20 and LLD for no OS, so that
  nothing of Linux's leaks in, through Meson and a cross file `ob`
  writes. libstdc++'s freestanding headers (`freestnd-cxx-hdrs`, GCC
  14.3.0, GPL-3.0-or-later WITH GCC-exception-3.1) compile its C++ and
  never reach an image.
- Its system layer is our own `sysdeps/radix`, in C++, in the port's
  directory: each `Sysdeps<Tag>` carries a C library call to Radix's
  capability methods through the C headers `ridlc` generates. It builds
  mlibc's POSIX, glibc and BSD options, and not its Linux one, whose
  headers come from Linux (GPL-2.0) and whose descriptors (epoll,
  timerfd, signalfd, eventfd) Radix does not have.
- Radix takes **Linux's x86-64 ABI** for what the C library shares with
  programs: errno values (each kernel error names its errno in
  `abi/errors.ridl`), signal numbers and frames, `struct stat`,
  `termios`, `ioctl` numbers, clock numbers and the auxiliary vector.
  The port links mlibc's `abis/linux` headers into its `abi-bits`, but
  for `ioctls.h` and `auxv.h`, which are Radix's own.
- Changes to mlibc's own code are patches in `ports/mlibc/patches`,
  each saying what it changes and why; fixes of general use go
  upstream (CONTRIBUTING.md).
- It builds a static `libc.a` with its start files for system programs
  (ADR 0005, ADR 0048), and a shared `libc.so` with its dynamic linker
  for bundles (ADR 0071).

## Consequences

- C programs build for Radix with `x86_64-radix-cc` as they would for
  Linux with musl, and mlibc's own test suite runs in the `posix` boot
  test: every one of its 132 tests that apply to Radix passes, and each
  one left out has its reason in `ports/mlibc-tests/build.sh`. So does
  libc-test, the musl project's tests of C libraries, which holds mlibc
  to what musl and glibc do: 110 of its tests pass, and those left out
  have their reasons in `ports/libc-test/build.sh`.
- Linux's numbers make Rust's `libc` crate, Lua and later ports need
  only small changes, and the libc crate check (ob(1), PORTS) holds the
  crate to the C library's headers.
- mlibc's gaps are ours to fill: its `printf`, `strtod` and `strftime`
  needed exactness from musl (ADR 0072), libc-test found bugs that
  patches 0013 to 0022 fix, each meant for upstream, and calls it
  leaves "not implemented" fail at run time, which the audits list.
- A new mlibc release is a recipe change plus a rebuild of the patches;
  the port's tests say whether it still works.
