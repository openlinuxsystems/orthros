<!--
SPDX-FileCopyrightText: 2026 The Dukovo contributors
SPDX-License-Identifier: Apache-2.0 OR MIT
-->

# 0070. Rust's standard library is ported to a radix target on mlibc, built from the toolchain's rust-src

- Status: Accepted
- Date: 2026-10-22

## Context

Most of Dukovo's own programs from Phase 9 on are Rust: Nukesh, the
uutils core utilities, consoled, and later Lumen and the apps. They need
Rust's standard library: files, threads, processes, time, environment,
panics that unwind with backtraces. Until Phase 8 native programs had
only radix-rt (ADR 0051), a heap and a few calls. The standard library
already runs on many Unix systems through their C library, so on top of
mlibc (ADR 0068) Radix can be one more. Rust's own build of the
standard library for a new target would mean building the compiler; the
pinned nightly's `rust-src` component has the library and every crate
it needs, vendored.

## Decision

- A target **`x86_64-unknown-radix`** (`userspace/targets/`): family
  `unix`, OS `radix`, environment `mlibc`, static position-independent
  programs linked by `x86_64-radix-cc`, panics that unwind.
- The **`rust-std` port** builds the standard library from the pinned
  toolchain's `rust-src`, offline from the crates it vendors, with five
  patches: the `radix` OS in `std` (Linux's stat fields as the Hurd's
  module has them, the program's path from the auxiliary vector, the
  Unix ways Radix has: close-on-exec through `fcntl`, `preadv`,
  `flock`, `clock_nanosleep`, thread-local destructors), in the
  `backtrace` crate (symbols through `dl_iterate_phdr` and gimli), in
  `unwind` (the pure-Rust `unwinding` crate, as std uses it on Xous),
  in the `libc` crate (a Radix module with mlibc's types, structures,
  constants and functions) and in `unwinding` itself. It installs a
  sysroot in `out/toolchain/rust`, a toolchain port that never reaches
  an image.
- Programs on it live in `userspace/programs/`, a workspace of its own
  built with the pinned toolchain against that sysroot, with the patched
  `libc` crate in place of crates.io's. The first is `std-test`, which
  checks the environment, files and their locks, threads, time,
  processes, pipes, panics, backtraces and memory, and runs in the
  `posix` boot test.
- After every build of the port, `ob` checks the `libc` crate's Radix
  module against mlibc's headers: every type's and structure's size and
  alignment, every field's offset and every constant, as Clang sees
  them, and that the C library defines every function the module
  declares (ob(1), PORTS). Any difference fails the build.
- The image's license manifest lists the crates a program on std links,
  with the notices of those rust-src vendors.

## Consequences

- Phase 9's Rust programs build with plain Cargo and std; uutils and
  other crates that support Linux need little more than `target_os =
  "radix"` beside Linux's.
- The `libc` check found that mlibc aligns `struct sched_param` as a
  pointer, which the module now does too; a wrong layout can no longer
  reach a program silently.
- Updating the pinned nightly means rebasing the five patches on its
  `rust-src`; the port fails to build, not to run, when they no longer
  apply.
- What std cannot do on Radix yet, such as catching a stack overflow
  (it needs `pthread_getattr_np`), is listed in docs/design/posix.md.
  The patches are kept upstreamable, and go upstream once the target
  is stable.
