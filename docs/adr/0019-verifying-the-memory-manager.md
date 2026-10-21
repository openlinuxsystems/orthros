<!--
SPDX-FileCopyrightText: 2026 The OrthrOS contributors
SPDX-License-Identifier: Apache-2.0 OR MIT
-->

# 0019. The memory manager is verified with Kani proofs and proptest properties

- Status: Accepted
- Date: 2026-10-20

## Context

The roadmap's Phase 2 asks for "Kani proof harnesses for allocator
invariants" and "host-side property tests with proptest", and its exit
criterion requires the Kani harnesses to pass in CI. Both tools are
third-party dependencies of a kernel crate, which the coding standard says
need an ADR. The decisions left open are which Kani release, how the proofs
are bounded and run, and how proptest enters the build.

Kani is a bounded model checker for Rust built on CBMC. It proves a
property for every value of its symbolic inputs, but only up to the loop
bounds a harness gives, and its cost grows quickly with state size. It
treats atomics as ordinary memory operations, so it says nothing about
concurrency.

## Decision

- `radix-mm` keeps its allocators' logic in safe code that runs on the
  host and under Kani: the buddy allocator, size classes, magazines, the
  mapping-window ranges and the PCID allocator. Harnesses live in
  `kernel/mm/src/proofs.rs` behind `cfg(kani)`; each states its bounds,
  chosen so that it finishes within minutes, and `docs/design/memory.md`
  lists what each proves.
- Kani is pinned to 0.68.0, in `tools/xtask/src/doctor.rs` and CI, kept in
  step by a unit test. `ob test kani` runs the harnesses; `ob doctor` notes
  whether the pinned Kani is installed. Plain `ob test` leaves them out,
  because they take about ten minutes; CI runs them in a job of their own.
- proptest 1.x, without default features (no forking, no timeouts), is a
  dev-dependency of `radix-mm` only. It never builds into the kernel.
  Property tests check random operation sequences against simple models at
  larger sizes than the Kani bounds allow.
- Concurrency is checked by multi-threaded host tests and by the
  `mm-stress` and `tlb` self-tests on 4 CPUs in QEMU.

## Consequences

- Each layer covers what the others cannot: Kani is exhaustive but small,
  proptest is large but sampled, the boot tests are real but slow.
- Code meant for proofs must stay generic or small enough to verify (the
  range table and PCID allocator are generic over their sizes for this).
- A Kani release bump is a reviewed change, like a toolchain bump, and may
  change how long the harnesses take.
- Revisit with the capability core (Phase 3), whose rights checks the plan
  also proves with Kani, alongside a TLA+ model.
