<!--
SPDX-FileCopyrightText: 2026 The Dukovo contributors
SPDX-License-Identifier: Apache-2.0 OR MIT
-->

# 0058. Every kernel function's stack frame is held to 2 KiB, checked after every release build

- Status: Accepted
- Date: 2026-10-22

## Context

A kernel thread, and a system call on its thread's kernel stack, runs on
16 KiB, with an unmapped guard page below (docs/design/scheduler.md).
Phase 7's storage paths are the deepest yet: a system call into a
directory method, the VFS, FAT, the block cache, the block layer and the
driver. While building them, a
mount panicked with a double fault: a function that built an array of a
few hundred entries on its stack took 6 KiB at once. Nothing showed it
until a call went that deep, and the same can happen to any array that
grows, in any path. Linux limits frames with `-Wframe-larger-than` at
compile time; Rust has no such lint, but LLVM can record each function's
frame (`-Zemit-stack-sizes`).

## Decision

- The kernel is built with `-Zemit-stack-sizes`, so LLVM writes every
  function's address and frame size into a `.stack_sizes` section.
- After every release build, `ob` reads that section and refuses the
  kernel if any function's frame is over 2 KiB, an eighth of a thread's
  stack, naming each one and its size. The few functions allowed more
  are listed, with the reason, in `tools/xtask/src/frames.rs`: code that
  runs once on the boot CPU's 64 KiB stack, the panic handler, and boot
  tests. The section is stripped from the installed kernel.
- Large arrays are put on the heap, or built there a slot at a time
  (`radix_block::pagemap`'s nodes), and big searches use bit sets rather
  than queues (the lock checker's). Copies of whole tables go one entry at
  a time.
- Debug builds are not held to the budget: without optimisation, frames
  are several times larger. CI and every image use release builds.

## Consequences

- A big frame fails the build of the change that adds it, with its name,
  instead of a double fault in some later test or on some user's
  machine.
- The budget bounds single frames, not whole call chains; the deepest
  paths are still exercised by the boot tests, and a whole-graph check
  can be added from the same section if a chain ever overflows.
- A debug kernel may overflow a 16 KiB stack on a deep path; it is a
  tool for stepping through code in gdb, not one to run the tests on.
