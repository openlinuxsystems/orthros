<!--
SPDX-FileCopyrightText: 2026 The Dukovo contributors
SPDX-License-Identifier: Apache-2.0 OR MIT
-->

# 0036. Until Phase 6, the kernel starts user-mode test programs from one image the boot loader loads, and the store gains random, ring and kernel_log objects

- Status: Accepted
- Date: 2026-10-21

## Context

Phase 5's exit asks for user-mode test programs that ping-pong over IPC
and rings. Phase 6 brings the ELF loader, processes and init; before it
there is no file system to load programs from and no process to own
them. The test programs also need a few kernel objects the capability
core of Phase 3 did not have: the random number generator every program
receives (ADR 0031), rings (ADR 0034), and permission to write to the
kernel log.

## Decision

- The user programs are a Cargo workspace of their own, `userspace/`,
  built for a bare `x86_64-radix-user` target with `core` built from
  source: `libradix` (entry, system calls, printing, time, rings and
  channels) and `usertests`, one statically linked, non-relocatable ELF64
  image holding every test and benchmark program. `ob build` builds it
  into `out/sysroot/boot/usertests`, and the boot image carries it as a
  GRUB module named `usertests`.
- The kernel's `user::launch` starts programs from that image in a
  `Group`. Each program gets its own capability space of 256 slots, an
  address space with the image's segments, a 256 KiB stack and a
  read-only start-up page at 0x10000 (`radix-startup(2)`), and one
  thread pinned to a CPU. The start-up page names the program's *role*,
  so one image serves every test; the group puts shared endpoints and
  notifications in the slots from 8 on. The frames of the image, stack
  and start-up page are kept in a hidden space so a program cannot unmap
  its own code. A group's end destroys every space it made.
- The `user`, `ipc` and `bench` self-tests (`selftest=NAME`) start these
  groups and check how each program exited; `user` and `ipc` also check
  that the capability store and the object registry hold afterwards what
  they held before.
- The capability store gains three object kinds, `random`, `ring` and
  `kernel_log`, for fifteen in all. The rights stay the twelve of ADR
  0023. The kernel's object registry maps the store's object names to
  the kernel objects and is told when the store releases one, so the
  object closes with its last capability.
- The user crates' licenses go into the image's license manifest beside
  the kernel's, and `cargo deny` checks them with the kernel's
  permissive-only policy.

## Consequences

- Phase 5 tests every system call and IPC mechanism from real user mode
  without a loader or a file system, and the same image runs the
  benchmarks.
- `user::launch` is test scaffolding: Phase 6's loader and init replace
  it for real programs, and the test image then becomes ordinary
  programs started by init. The ELF reader (`radix-elf`) and the address
  space and frame objects carry over.
- Programs are pinned to a CPU each, which the benchmarks need; Phase 6
  leaves placement to the scheduler.
- A new kind of kernel object is a new `ObjectKind` in `radix-cap` and in
  `abi/capability.ridl`, with a registry entry and a close action.
