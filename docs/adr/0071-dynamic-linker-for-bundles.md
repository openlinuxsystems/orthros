<!--
SPDX-FileCopyrightText: 2026 The Dukovo contributors
SPDX-License-Identifier: Apache-2.0 OR MIT
-->

# 0071. mlibc's dynamic linker is /system/lib/ld.so, for bundles; system programs stay static

- Status: Accepted; the dynamic linker is `/system/libraries/ld.so` since [0114](0114-plain-names-inside-system.md)
- Date: 2026-10-22

## Context

System programs are linked statically (ADR 0005, ADR 0048): one file
each, nothing to resolve at start, nothing another package can break.
Bundles, the self-contained apps of Phase 13 on, are different: a port
of a large program ships libraries of its own (a browser's, a game's),
and some programs load plugins with `dlopen`. Both need a dynamic
linker and a shared C library, and the plan puts mlibc's in Phase 8.
The kernel's `exec` already maps a program's interpreter (ADR 0063).

## Decision

- mlibc builds its **dynamic linker** as `/system/lib/ld.so` and a
  **shared C library** as `/system/lib/libc.so`, beside the static one,
  both in every image. Radix's system layer goes into the dynamic
  linker as Linux's does (built with `MLIBC_BUILDING_RTLD`, leaving out
  what only the C library proper may call), and compiler-rt's builtins
  are linked into both. A library is looked for in the run path of the
  object that needs it, where `$ORIGIN` is that object's directory, then
  in `LD_LIBRARY_PATH` and `/system/lib`.
- `x86_64-radix-cc` links a static position-independent program by
  default. **`-dynamic`**, which it takes away before calling Clang,
  links a program whose interpreter is `/system/lib/ld.so`, on the
  shared C library; **`-shared`** links a shared library. System
  programs are never linked with either; in `/system` only the tests of
  dynamic linking are.
- The `posix` boot test's `dynamic` test is built as a bundle is: a
  program with a private library in `lib/` beside it, found through
  `$ORIGIN/lib`, with a constructor, data and thread-local storage, and
  a plugin opened with `dlopen`, whose thread-local storage each thread
  gets when it first asks.

## Consequences

- A bundle can ship and load its own libraries without anything outside
  `/system` changing, and a dynamically linked program costs nothing to
  the static ones.
- The shared C library is part of the system: its ABI is mlibc's, and a
  system update that changes it must keep bundles working, which the
  `dynamic` test starts to check.
- Library search does not yet know bundles: Phase 13's bundle format
  decides their layout, and may give the dynamic linker a path of its
  own per bundle.
