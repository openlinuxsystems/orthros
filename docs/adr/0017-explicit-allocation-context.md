<!--
SPDX-FileCopyrightText: 2026 The Dukovo contributors
SPDX-License-Identifier: Apache-2.0 OR MIT
-->

# 0017. Every kernel allocation states its context, and the global allocator refuses

- Status: Accepted
- Date: 2026-10-20

## Context

Spec section 3.4 asks the kernel's allocators to be explicit about context,
the way Linux's GFP flags are: whether the caller may sleep, which memory it
needs, and from which NUMA node. The roadmap's Phase 2 adds "graceful
failure everywhere": running out of memory must be an error the caller
handles, not a panic.

Rust's `alloc` crate offers two styles. Its default APIs (`Box::new`,
`Vec::push`, `format!`) allocate through the `#[global_allocator]`, which
has no way to receive flags, and abort when allocation fails. Its fallible
APIs (`Vec::try_reserve`, `Box::try_new_in`) return errors, and the
`_in` variants take an allocator value, which can carry flags. The
compiler can remove the aborting APIs: `alloc` built with
`--cfg no_global_oom_handling` does not have them.

## Decision

- Every call into the frame allocator, the heap and the page-table code
  takes `radix_mm::Flags`: a `Context` (`Atomic` or `MaySleep`), a zone
  limit (`Dma32` or none), a NUMA node policy and whether to zero. Failure
  is `AllocError`; nothing in the memory manager panics for lack of memory.
- The kernel's `#[global_allocator]` is `radix_mm::ExplicitOnly`, which
  refuses every request.
- `ob build` compiles `alloc` with the kernel (`-Zbuild-std=core,
  compiler_builtins,alloc`) and with `--cfg no_global_oom_handling`, so
  code that calls `Box::new` or `Vec::push` does not compile.
- Code that wants `alloc` collections passes a `Kmalloc`, the heap with
  fixed flags (`HEAP.with(Flags::MAY_SLEEP)`), to the `_in` APIs and
  uses the fallible ones: `Vec::try_reserve` followed by
  `push_within_capacity`, and `Box::try_new_in`.

## Consequences

- The compiler, not review, enforces that every allocation names its
  context and handles failure.
- `Box::try_new_in` and `Vec::push_within_capacity` are unstable library
  features; the kernel already builds with the pinned nightly (ADR 0011),
  and a toolchain bump may need small changes.
- Third-party `no_std` crates that allocate through the global allocator
  cannot be used in the kernel unless they offer allocator-aware APIs.
- Allocations through `ExplicitOnly` fail at run time rather than compile
  time when code uses the global allocator in a way that survives
  `no_global_oom_handling` (such as `Vec::try_reserve` on a plain `Vec`).
  That fails safely, as an error, and shows up in testing.
- ADR 0011 listed `-Zbuild-std=core,compiler_builtins`; `alloc` is now
  built the same way.
