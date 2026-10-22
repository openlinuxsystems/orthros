<!--
SPDX-FileCopyrightText: 2026 The Dukovo contributors
SPDX-License-Identifier: Apache-2.0 OR MIT
-->

# 0011. The kernel has its own target and builds core with -Zbuild-std

- Status: Accepted
- Date: 2026-10-20

## Context

Phase 0 built the stub kernel for Rust's built-in `x86_64-unknown-none`
target (ADR 0010), which links position-independent code and ships a
precompiled `core`. Phase 1 needs:

- a kernel linked in the top 2 GiB of the address space, which the
  `kernel` code model makes cheap and position-independent code makes
  pointless before KASLR (Phase 2);
- frame pointers in every function, including those in `core` (panic
  formatting, slice and integer helpers), because panic reports and `kdb`
  walk the stack by frame pointer: the reference laptop has no serial port
  and no debugger, so the report has to be right without DWARF unwinding;
- no red zone and no SSE in kernel code, since interrupts arrive on the
  kernel stack and the kernel does not save vector registers.

The precompiled `core` of the built-in targets omits frame pointers, and a
built-in target cannot change the code model or relocation model for
`core`.

## Decision

The kernel builds for `kernel/targets/x86_64-radix-kernel.json`: the
`kernel` code model, static relocation, frame pointers always, red zone
disabled, soft float with SSE and MMX off, inline stack probes, and
`panic = abort`. `ob build` passes `-Zjson-target-spec`,
`-Zbuild-std=core,compiler_builtins` and
`-Zbuild-std-features=compiler-builtins-mem`, so `core` and
`compiler_builtins` are compiled with the same settings. The output is an
`ET_EXEC` ELF linked by `rust-lld` with `kernel/arch/x86_64/linker.ld`.

The toolchain stays the pinned nightly; `rust-src` is a required component
and `ob doctor` checks it. The AArch64 HAL stubs are checked against the
built-in `aarch64-unknown-none`, which needs no custom target because
nothing runs.

## Consequences

- Backtraces follow frame pointers through every frame, including `core`.
- Every clean kernel build compiles `core` and `compiler_builtins` too,
  which makes it slower; incremental builds are unaffected.
- Custom target specifications and `build-std` are unstable. A toolchain
  bump may need the JSON adjusted (Rust validates it against the compiler's
  data layout) and is a reviewed change on its own, as the coding standard
  already requires.
- Cargo prints a future-incompatibility notice about `core` for soft-float
  targets; `ob` silences it with `CARGO_FUTURE_INCOMPAT_REPORT_FREQUENCY`.
- Revisit when KASLR (Phase 2) needs a relocatable kernel, or if a
  built-in target gains the needed options.
