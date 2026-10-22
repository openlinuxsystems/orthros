<!--
SPDX-FileCopyrightText: 2026 The Dukovo contributors
SPDX-License-Identifier: Apache-2.0 OR MIT
-->

# 0023. Capabilities carry copy and transfer rights, seal with endpoints, and revoke in marked steps

- Status: Accepted
- Date: 2026-10-21

## Context

The roadmap's Phase 3 asks for capability spaces, eleven object types,
rights masks, badges, sealing, and a derivation tree with delegation and
revocation, and spec 15 asks for an argument that capabilities cannot be
forged or widened. The specification fixes the object types and that every
resource is reached through a capability (spec 2.3), but leaves open the
rights set, what sealing means, what deleting a capability in the middle
of a tree does, how a long revoke shares the CPU, how deep trees may grow,
and how memory becomes other objects. seL4, CHERI and Fuchsia's handles
answer these differently.

## Decision

- **Rights.** Twelve rights in a 16-bit mask: `read`, `write`,
  `execute`, `send`, `receive`, `grant`, `signal`, `control`, `seal`,
  `unseal`, and two about capabilities themselves, `copy` (needed to make
  anything from a capability) and `transfer` (needed to put it, or what is
  made from it, in another space). Each kind has a fixed set of rights,
  plus `copy` and `transfer`. Separating `copy` from `transfer` lets a
  process hand out a capability that the receiver can use but not pass on
  (no `copy`), or copy only within its own space (no `transfer`), as
  Fuchsia's `DUPLICATE` and `TRANSFER` rights do.
- **Badges** are a 64-bit value on endpoint and notification capabilities,
  0 meaning none, set once on derivation and inherited, as in seL4.
- **Sealing** follows CHERI's sealed capabilities, with an endpoint as the
  key instead of an object type number: `seal` on an endpoint seals,
  `unseal` on the same endpoint unseals. A sealed capability can be stored
  and copied exactly, and nothing else, so servers can hand out opaque
  handles they alone can open.
- **Ranges.** IRQ lines, I/O ports and MMIO pages are capabilities to
  ranges, not table objects, and derivation can narrow them.
- **Memory** objects are retyped into frames, DMA buffers and smaller
  memory objects from a watermark that only rises while anything retyped
  from them lives, as seL4's untyped memory is; the kernel's memory objects
  come from the frame allocator.
- **The derivation tree** spans all spaces and is kept in the slots
  themselves. Deleting a capability moves its children up to its parent,
  so they stay revocable from above; moving keeps a capability's place.
  Derivations are at most 64 deep; the stored depth is an upper bound, so
  walks up the tree need no visited set.
- **Revocation** marks the capability and deletes its subtree leaf by leaf
  within a budget per call (64 in the kernel, which lets go of the lock
  between calls). While the mark is set, nothing can be derived from the
  capability or below it, and it cannot move, which makes every revoke end.
- **One store, one lock.** `radix-cap` holds every space, the object table
  and the tree, and checks everything before changing anything. Its checks
  are pure functions on capabilities, so Kani can prove them. The kernel
  keeps one store behind one lock.
- `radix-cap` uses the nightly library feature `vec_push_within_capacity`:
  the kernel builds `alloc` without infallible allocation (ADR 0017), and
  this is the way to fill a vector whose room was reserved with
  `try_reserve`.

## Consequences

- No capability can be forged or widened, and revocation reaches
  everything derived; `docs/design/capabilities.md` argues both, backed by
  Kani proofs, property tests, a TLA+ model and the `caps` boot test.
- Deleting an intermediate capability does not lose its children, unlike a
  design where they would become unrevocable roots; deleting an original
  does make its children roots, which the holder chose by deleting it.
- One lock serializes every capability operation; finer locking waits for
  Phase 5's IPC benchmarks to show it is needed, and would need the model
  to grow.
- Revisit if the 64-level limit or the 65,536-slot spaces get in the way
  of real programs, or if a new object kind needs rights outside the
  twelve.
