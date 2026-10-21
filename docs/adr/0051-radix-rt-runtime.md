<!--
SPDX-FileCopyrightText: 2026 The OrthrOS contributors
SPDX-License-Identifier: Apache-2.0 OR MIT
-->

# 0051. radix-rt is the Rust runtime of native programs: a heap, arguments, the boot archive and spawning, on top of libradix

- Status: Accepted
- Date: 2026-10-21

## Context

`libradix` (ADR 0036) gives a program its entry point, the system calls
and printing, and nothing that allocates. Spark has to read files from
the boot archive, build a process for another program and keep track of
it, and Phase 8's Rust `std` port and mlibc need the same pieces
underneath them. Writing them once, in Rust, as a library every native
program can use keeps Spark small and makes the user-space loader the
same code for every program.

## Decision

- `userspace/radix-rt`, a `no_std` crate on top of `libradix` with `alloc`
  built from source, provides:
  - **a heap**, the global allocator: blocks of 16 to 2,048 bytes in eight
    size classes cut from 64 KiB chunks, larger blocks in frames of
    their own that are unmapped and deleted when freed, all made from
    the program's own memory object and mapped in order from a random
    page above `PROGRAM_BASE`;
  - **`args()`**, the program's arguments from its start-up page;
  - **`BootFs`**, the boot archive as a read-only file system (`mount`,
    `files`, `read`, `list`), over `radix-bootfs`;
  - **`spawn()`**, the user-space loader: it reads and places an image
    with `radix-elf` exactly as the kernel's loader does (ADR 0048),
    makes the process, its memory from a pool the caller names, its
    frames, stack and start-up page, gives it the capabilities
    radix-startup(2) lists and any the caller shares, keeps the frames
    of its image hidden, asks to hear of its exit, and starts it.
    `Child::wait` and `Child::reap` wait for the exit and let it go.
  - a slot allocator, an address-range allocator and a spin lock that
    yields, which the pieces above share.
- Native programs written in Rust (Spark, `hello`, the system tools of
  later phases) depend on `radix-rt`; the self-tests' `usertests` image
  stays on `libradix` alone, so that the benchmarks measure nothing but
  the kernel.
- Everything is built for the `x86_64-radix-user` target as static PIE.

## Consequences

- A program can start another with one call, and Spark's own code is
  only about policy.
- The heap is simple and fast enough for Phase 6, but never gives chunks
  back, and neither address ranges nor a freed large block's pages are
  reused; `docs/design/processes.md` lists this, and a production
  allocator replaces it with the Rust `std` port in Phase 8.
- Programs written in C get the same services from mlibc's Radix
  sysdeps in Phase 8, which can call into the same kernel interfaces.
