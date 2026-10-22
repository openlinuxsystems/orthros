<!--
SPDX-FileCopyrightText: 2026 The Dukovo contributors
SPDX-License-Identifier: Apache-2.0 OR MIT
-->

# 0048. Programs are static position-independent executables, placed at random addresses by whoever loads them

- Status: Accepted
- Date: 2026-10-21

## Context

The plan asks for an ELF64 loader for static and position-independent
programs and for user ASLR. Phase 5's test image was linked at fixed
addresses (0x400000), so every copy of it sat at the same place, and an
attacker who knew the image knew where every gadget in it was. Dynamic
linking has no place yet (mlibc's dynamic loader comes in Phase 8, for
bundles that ship private libraries), and system programs are linked
statically (spec 7.2).

## Decision

- The user target builds **static position-independent executables**
  (`static-pie`, as lld and Linux's `-static-pie` make them): an
  `ET_DYN` image with no interpreter and no needed libraries, whose
  only dynamic relocations are `R_X86_64_RELATIVE`. The loader applies
  them itself, as it copies the segments in; nothing in the program
  runs before its entry point to relocate it. A program linked at fixed
  addresses (`ET_EXEC`) still loads where it was linked, which the
  self-tests' `usertests` image keeps (`-no-pie`) so that its addresses
  stay put across runs.
- `radix-elf` checks an image whole before anything is mapped, because
  an image is data from outside: every offset and size stays inside the
  file, segments are in order and share no page, none is both writable
  and executable, the stack is not executable, the entry point is in an
  executable segment, and every relocation is `R_X86_64_RELATIVE` and
  lands in the image's writable data. Data that is read-only after
  relocation (`PT_GNU_RELRO`) is mapped read-only from the start. The
  same code loads programs in the kernel, into frames, and in user
  space, through a mapping; its property tests feed it garbage.
- Addresses come from radix-startup(2)'s windows. A position-independent
  image goes at a random page in the 1 TiB window from `IMAGE_BASE`; the
  top of the stack at a random page in the 64 GiB below `STACK_TOP`; a
  loader's own mappings for the program (Spark's view of the boot
  archive) at a random page in the 1 TiB window from `LOADER_BASE`. That
  is 28 bits of entropy for the image and those mappings, as Linux's
  default for `mmap` on x86-64, and 24 for the stack. The space from
  `PROGRAM_BASE` up is left to the program, and radix-rt draws its own
  random start there for the heap.
- The start-up page tells the program how far its image moved
  (`image_bias`), so that a debugger or a crash report can map its
  addresses back to the symbols it was linked with.

## Consequences

- Every program, Spark included, runs at a different address each boot;
  the `spark` boot test checks that Spark's and its child's biases are
  page-aligned, inside the window, and differ.
- Position-independent code costs a little on x86-64 (RIP-relative
  addressing is nearly free; the instruction budgets of ADR 0045 hold
  for the fixed `usertests` image, which the benchmarks run).
- A program's address space is only as random as the kernel's random
  number generator (ADR 0031), which is ready before the first program.
- Shared libraries, `PT_INTERP` and TLS segments are refused until
  Phase 8 brings mlibc's dynamic loader and thread-local storage.
