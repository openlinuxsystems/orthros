<!--
SPDX-FileCopyrightText: 2026 The Dukovo contributors
SPDX-License-Identifier: Apache-2.0 OR MIT
-->

# Capabilities

This note describes the Radix capability core as of Phase 3: the objects
and rights a capability can carry, the store that holds capability spaces
and the derivation tree, how delegation and revocation work, and why no
capability can be forged or have its rights widened (spec 15). The
user-facing description is `radix-capabilities(7)`; `radix-debug(7)`
covers the `caps` self-test and the `caps` command of `kdb`. The decisions
behind the design are ADR 0023 (the model) and ADR 0024 (the TLA+ tools).

## Crates

```text
kernel/cap              radix-cap: rights, object kinds, capabilities and their
                        checks (cap.rs), the store (store.rs); host tests,
                        property tests and Kani proofs
kernel/cap/tla          Capabilities.tla: the TLA+ model of derivation,
                        retyping and revocation, with three TLC
                        configurations
kernel/radix            caps.rs: the kernel's one store, its lock and the
                        frame allocator behind memory objects; captest.rs:
                        the `caps` self-test
tools/xtask             tla.rs: `ob test tla`
```

`radix-cap` has no dependencies, is `#![forbid(unsafe_code)]`, and builds
for the host, where its unit and property tests run, for Kani, and for the
kernel. The store takes its memory from an `Allocator` it is given: the
kernel passes the heap with `MaySleep` (ADR 0017), the host tests the
global allocator. Every allocation is fallible; a store that runs out of
memory refuses the operation with `NoMemory` and changes nothing.

## Objects, rights and capabilities

A `Capability` (`cap.rs`) is a small value: an `Object` (a kind, and either
an `ObjectId` in the store's object table or a range of interrupt lines,
I/O ports or MMIO pages), a set of `Rights`, a badge (0 for none) and the
`ObjectId` of the endpoint it is sealed with, if it is sealed. Its fields
are private to the crate, and it has no public constructor.

The twelve object kinds and their rights are listed in
`radix-capabilities(7)`; `ObjectKind::rights` is the table. Every kind has
the two rights about capabilities themselves: `copy`, needed to make
anything from a capability, and `transfer`, needed to place what is made,
or the capability itself, in another space. Only endpoints and
notifications carry badges. Three kinds are ranges rather than table
objects (`Irq`, `IoPorts`, `Mmio`); a range capability names the range
itself, so two disjoint ranges of I/O ports are different authority
without any object to share.

The checks are pure functions on capabilities, which is what lets Kani
prove them for every input:

| Function | Makes | Refuses when |
| --- | --- | --- |
| `check(kind, needs)` | nothing; says whether a use is allowed | sealed, another kind, or a right in `needs` missing |
| `derive(how)` | a copy with `rights ∩ mask`, maybe a badge, maybe a sub-range | no `copy`; sealed and not an exact copy; a badge on a kind without badges, on a badged capability, or of 0; a range on a kind without one, or outside its range |
| `seal_with(key)` | the same capability, sealed with the key's endpoint | the key is not an unsealed endpoint with `seal`; no `copy`; already sealed |
| `unseal_with(key)` | the capability with its seal removed | the key is not an unsealed endpoint with `unseal`; not sealed; sealed with another endpoint |
| `retyped(kind, id)` | the original of a new frame, DMA buffer or memory object | not an unsealed memory capability with `control` and `copy`; a kind not made from memory |
| `within(parent)` | nothing; the relation every derived capability has to its source | — |

`within` holds when a capability's rights are a subset of the parent's,
it names the same object or a part of the same range, and it has the
parent's badge (or the parent has none). It ignores the seal, which is
not authority.

## The store

`Capabilities` (`store.rs`) holds the capability spaces, the object table
and the derivation tree.

**Spaces** are vectors of slots, named by a `SpaceId` of an index and a
generation; a destroyed space's index is reused with the next generation,
so an old name stays wrong. A slot is named by a `SlotRef` (space and
slot number). Each slot holds an optional capability and its place in the
derivation tree: parent, first child, previous and next sibling (as slot
links, possibly in other spaces), a depth, and a revoke mark. The tree is
intrusive, so deriving, deleting and moving never allocate.

**Objects** live in a table of records named by an `ObjectId` (index and
generation, like spaces). Each record counts the capabilities that name
the object and the objects retyped from it, and remembers the memory it
covers and the object it was retyped from. An object is destroyed when
both counts reach 0; the store then tells the caller through a `Released`
callback, which gives memory that came from outside the store (the
kernel's frame allocator) back, and decrements the count of the object it
was retyped from, which may go in turn.

**Operations**:

- `create` makes an object of a kind that is neither a range nor made
  from memory, and puts its original capability, with every right of its
  kind, in an empty slot. `create_memory` does the same for a block of
  memory the caller hands over; `grant_range` for a range of interrupt
  lines, ports or MMIO pages. In the kernel only the kernel calls these;
  from Phase 5 they are how the kernel gives a process its first
  capabilities.
- `derive`, `seal` and `unseal` apply the check of the same name to the
  capability in one slot and put the result in an empty slot, as the
  source's child. Into another space they also need `transfer`.
- `retype` carves `count` blocks of 2^`order` pages from a memory object
  (`carve`): from the object's watermark, aligned to their size in
  physical memory, all inside the object. It reserves the table records
  first, so it either makes them all or changes nothing. The watermark
  only rises while anything retyped from the object lives, and returns to
  0 when the last of them goes.
- `move_cap` moves a capability to an empty slot, keeping its place in the
  tree; its parent and children are re-linked to the new slot. Into
  another space it needs `transfer`.
- `delete` removes a capability. Its children move up to its parent, in
  its place among its siblings, so they stay revocable from above.
- `revoke` deletes every capability below one, in every space, at most
  `budget` per call (see below).
- `destroy_space` deletes every capability in a space, as `delete` does,
  and frees the space.

Every operation checks everything before changing anything, so a refused
operation leaves the store as it was.

**Depth.** Each capability stores a depth one more than its parent's at
the time it was derived, at most `MAX_DEPTH` (64). A delete moves children
nearer the root without lowering their depth, so the stored depth is an
upper bound on the distance to a root, and strictly greater than the
parent's. That makes every walk up the tree end within 64 steps without a
visited set. Deriving from a capability at depth 64 fails with `TooDeep`.

**Revocation.** `revoke` sets the mark on the capability, then walks its
subtree depth-first and deletes leaves (a post-order walk that needs no
stack: from a deleted leaf it goes back to its parent and down again).
When the budget runs out it returns `Progress::More` and keeps the mark;
when nothing is left below, it clears the mark and returns
`Progress::Done`. While the mark is set:

- `derive`, `seal`, `unseal` and `retype` walk up from their source (at
  most 64 steps) and refuse with `Revoking` if they meet a mark, so nothing
  new appears in the subtree;
- the marked capability cannot move (`Revoking`), so the revoke's own
  name for it stays valid between calls.

The kernel calls `revoke` in steps of 64 deletions and lets go of the lock
in between (`caps::revoke`), so a revoke of a million capabilities does not
shut out other CPUs.

**Audit.** `audit` checks every invariant of the store: the tree's links
agree with each other and only join full slots, depths fall towards the
roots, every capability holds only rights of its kind and a badge only if
its kind has them, every child is `within` its parent or retyped from the
memory its parent names, every capability names a live object of its
kind, every object's two counts are right, and no object retyped from a
memory object lies above its watermark. "Retyped from" counts memory
retyped in turn from that memory: when the capability to an inner memory
object is deleted, what was retyped from it moves up to the outer
memory's capability, which still covers it. The audit takes time in
proportion to the store's size, times the nesting of memory objects for
retyped capabilities. The tests run it after operations; the kernel does
not run it on its own.

## In the kernel

`kernel/radix/src/caps.rs` keeps one store for the whole kernel, behind
one spin lock, set up right after the frame allocator. Memory objects get
their pages from the frame allocator, owned by `Owner::Memory`, and give
them back from the `Released` callback. Until the scheduler arrives
(Phase 4) the lock is a spin lock and the store allocates with `MaySleep`
under it, which is safe because nothing sleeps yet and the heap's reclaim
takes only the heap's own locks; Phase 4 turns it into a sleeping lock.

Lock order: the store's lock, then the heap's locks, then the frame
allocator's. The `Released` callback runs with the store locked and only
calls the frame allocator.

**Looking up without the lock.** Looking a capability up is the most
frequent use of the store: every system call on a capability does it.
Each CPU keeps the last capabilities its system calls found
(`user::objects::find`), sixteen of them indexed by slot, each with a
weak reference to the object it designates and the store's version
number at the time (ADR 0077). A lookup that finds the slot in this
CPU's cache at the current version returns it without taking the lock
or writing anything another CPU reads.

The version moves, under the store's lock and before it goes, whenever
a capability some CPU found leaves its slot, or an object some CPU found
leaves the registry. Finding a capability for a cache marks its slot
(`Store::get_to_cache`), and the store notes when a marked slot is
emptied, moved from or destroyed (`Store::take_stale`); the registry
does the same for its entries (`caps::mark_stale`). Other changes, such
as a capability put in an empty slot or one nobody looked up deleted,
leave every cache as it is: nothing in a cache was taken away.

That is correct because a capability stays in its slot, unchanged,
until it is deleted, moved or revoked, all of which empty the slot: so
if the version did not move, the capability a CPU found is still there,
and an operation that has emptied the slot but not moved the version yet
has not returned either, so the lookup counts as coming before it, which
a lookup that took the lock first could also have done. A revoke that
has returned has moved the version, and any thread that learns of it
(through a lock, an IPC or any other synchronization) sees the new
version, so it cannot use what was revoked. The unit tests check this
on random sequences of every operation (`a_cache_the_store_keeps_fresh_is_never_wrong`).
The cache keeps weak references to objects, so it keeps no object
alive. It is a per-CPU value that only its own CPU touches, with
preemption off (`percpu::Own`), so a hit takes no lock and no locked
instruction but the object's own reference count (ADR 0040).

## Why no capability can be forged or widened

This is the semi-formal argument the roadmap asks for. Each step names the
check that backs it (see *Verification*).

**Definitions.** A capability *exists* when a slot of a live space holds
it. Its *source* is the capability it was made from by `derive`, `seal`,
`unseal` or `retype`; an original has none. Write `c ≤ p` for
`c.within(&p)`.

**1. Capability values come only from the five constructors.** The
fields of `Capability` are `pub(crate)` and the crate forbids `unsafe`,
so code outside `radix-cap` cannot build or alter one; it can only receive
copies from the store's getters. Inside the crate, a `Capability` is built
only by `Capability::original` (private, used by `create`,
`create_memory` and `grant_range` for an object or range that has no
capability yet), `derive`, `seal_with`, `unseal_with` and `retyped`. From
Phase 5, programs name capabilities by slot number and never hold the
values at all.

**2. Capabilities never change in place.** The store writes a slot's
capability only in `place` (an empty slot gets a new capability) and in
`remove` and `move_cap` (the slot is emptied, or the capability moves
unchanged). So once in a slot, a capability's rights, object and badge
never change. *(Code inspection; the property tests compare every slot
with the model after every operation; the TLA+ property `CapsImmutable`.)*

**3. Each constructor grants no more than its source.**

- `derive`: the result is `≤` the source, has rights `source ∩ mask`,
  keeps the object and the seal, and gets a badge only if the source had
  none. *(Kani: `derivation_never_widens`, for every capability and
  derivation.)*
- `seal_with` changes only the seal, so the result is `≤` the source and
  the source `≤` it. *(Kani: `sealing_changes_only_the_seal`.)*
- `unseal_with` gives back exactly the capability that was sealed, and
  only to a holder of the sealing endpoint with `unseal`. *(Kani:
  `unsealing_needs_the_key`.)*
- A sealed capability can be used for nothing and copied only exactly.
  *(Kani: `sealed_capabilities_stay_sealed`.)*
- `retyped` makes the original of a new object, carved from memory the
  source names, with rights in both the source's and the new kind's.
  *(Kani: `retyping_never_widens`; `carving_stays_above_the_watermark`
  shows the new objects lie inside the memory and never overlap.)*

**4. `≤` is transitive** *(Kani: `within_is_transitive`)*, so a chain of
derivations grants no more than its first capability.

**5. Using a capability is exact.** `check` allows a use exactly when the
capability is unsealed, of the kind the operation works on, and holds
every right it needs. *(Kani: `check_is_exact`.)* The store's `lookup` is
`get` followed by `check`.

**6. Names cannot be forged.** A `SlotRef` reaches a slot only if its
space index and generation name a live space and the slot number is in
range; destroyed spaces bump their generation. An `ObjectId` is checked
the same way, and an object lives while any capability names it, so no
capability ever names a reused record. *(Unit test
`stale_names_are_refused`; the forging part of the `caps` self-test.)*

**7. Moving into another space needs `transfer`.** `derive`, `seal`,
`unseal` and `retype` check it through `may_derive`, `move_cap` itself.
*(Unit test `transfer_is_a_right_of_its_own`; the widening part of the
self-test tries it for every rights mask without `transfer`.)*

**Theorem.** Every capability that exists is either the original of an
object or range the kernel created, with exactly its kind's rights, or
was made by one of the four operations from a capability that existed at
the time, and grants no more than it. By 2 nothing changes afterwards, so
by 3 and 4 and induction over the store's history, every capability
grants at most what an original the kernel made granted: no capability is
forged and no rights are widened. By 5 that is also all a holder can do
with it, by 6 holding a stale name gives nothing, and by 7 a capability
without `transfer` cannot leave its space.

## Why revocation reaches everything

Call a capability's *derivation history* the capabilities that still exist
among its source, its source's source, and so on.

**Invariant (`TreeIsDerivation`).** The tree ancestors of every
capability are exactly its derivation history.

- `derive`, `seal`, `unseal` and `retype` place the new capability under
  its source, so its ancestors are the source and the source's
  ancestors: its history.
- `delete` removes a capability from every history it was in, and moves
  its children up to its parent, so each descendant loses exactly that one
  ancestor.
- `move_cap` changes the capability's slot in the tree and in every
  history alike.
- `revoke` deletes only leaves, each of which is a `delete`.

*(TLC checks this invariant in every reachable state of the model, and the
store's audit checks the tree's consistency after every operation in the
property tests and in the self-test's random phase.)*

**Revocation is complete.** By the invariant, the capabilities made from
`c`, directly or not, that still exist are exactly `c`'s descendants.
`revoke` deletes descendants until there are none. While its mark is set,
nothing new can be made from `c` or anything below it, and nothing can
move into the subtree except by being derived there, so the subtree only
shrinks. When `revoke` returns `Done`, nothing made from `c` exists.
*(TLA+: `NoDerivationUnderRevoke`, `RevokeIsComplete`; unit test
`revoke_in_steps_blocks_new_derivations`; the revocation and concurrent
parts of the self-test.)*

**Revocation ends.** Each call with a budget of at least one deletes at
least one capability or finishes, and the subtree cannot grow, so a revoke
of `n` capabilities ends after at most `n / budget + 1` calls.
*(TLA+: `RevokeTerminates`, under weak fairness for the revoke's own
steps.)*

**Concurrency.** The store is one structure behind one lock, so each
operation is atomic. Between the steps of a revoke, other CPUs may run any
operation; the mark is what keeps them from adding to the subtree. The
TLA+ model splits a revoke into single deletions with arbitrary other
operations in between, which covers every interleaving of budgeted calls.
The self-test's concurrent phase has every other CPU derive from random
capabilities of a 4,096-capability tree while the boot CPU revokes it, and
then checks that nothing they derived survived.

## Verification

**Unit tests** (`cargo test -p radix-cap`, run by `ob test unit`) cover
each module with hand-written cases: rights algebra, the kind table, each
check, carving, and store scenarios
(`revoke_reaches_every_space_and_survives_deletes`,
`revoke_in_steps_blocks_new_derivations`,
`derivation_depth_is_bounded`, `sealed_capabilities_round_trip_through_the_store`,
`retyped_memory_comes_back`, `stale_names_are_refused`,
`device_ranges_narrow`, and others).

**Property tests** run random sequences of up to 60 operations (create,
make memory, grant ports, derive, seal, unseal, retype, move, delete,
revoke in random budgets, destroy and recreate spaces) over 3 spaces of 6
slots, 400 cases each:

| Test | Property |
| --- | --- |
| `the_store_follows_the_model` | after every operation the store holds exactly the capabilities, parents, depths, objects and counts of a simple model, the audit passes, and refused operations change nothing; tearing everything down releases every object |
| `no_sequence_widens_rights` | at the end, no capability holds a right its kind lacks or one its parent lacks, and none is further from its root than its depth |

**Kani proofs** (`kernel/cap/src/proofs.rs`, run by `ob test kani` and the
CI job of that name) hold for every input, with no bounds besides those
stated:

| Harness | Proves |
| --- | --- |
| `check_is_exact` | a use is allowed exactly when the capability is unsealed, of the right kind, and holds every right needed |
| `derivation_never_widens` | every derivation result is `within` its source, is a valid capability, has rights `source ∩ mask`, keeps the kind and seal, and gets a badge only when the source had none; every refusal is for the reason it gives |
| `sealed_capabilities_stay_sealed` | a sealed capability can be derived only as an exact copy, and cannot be used, sealed again or retyped |
| `sealing_changes_only_the_seal` | sealing succeeds exactly with an unsealed endpoint key holding `seal` and `copy` on an unsealed capability, and changes nothing but the seal |
| `unsealing_needs_the_key` | unsealing succeeds exactly with an unsealed capability to the sealing endpoint holding `unseal`, and gives back the sealed capability |
| `within_is_transitive` | `within` is transitive |
| `retyping_never_widens` | retyping needs an unsealed memory capability with `control` and `copy`, and gives a valid, unbadged, unsealed capability with rights in both the source's and the kind's |
| `carving_stays_above_the_watermark` | for orders up to 12 and counts up to 1,024 (the store's limit), carved blocks start at or above the watermark, are aligned to their size in physical memory, and end inside the memory object |

"Every capability" means every value the store can hold: rights within its
kind's, a badge only on a kind that has them, a range inside its kind's
limit. The harnesses take about ten seconds together.

**The TLA+ model** (`kernel/cap/tla/Capabilities.tla`, run by `ob test
tla` and the CI job "TLA+ models") describes the derivation tree as the
store keeps it: slots holding capabilities of an object, rights and a
badge, with `Create`, `Derive`, `Retype` (a new object inside the
source's), `Delete` (children move up), `Move`, and a revoke split into
`RevokeStart`, single-leaf `RevokeStep`s and `RevokeDone`. Each object
records the object it was retyped from, and lives while a capability
names it or anything retyped from it lives. A ghost variable records each
capability's derivation history. TLC checks:

| Property | Kind | Says |
| --- | --- | --- |
| `TypeOK`, `EmptySlotsAreBlank`, `TreeWellFormed` | invariant | the tree joins only full slots, depths fall towards the roots, so it has no cycles |
| `TreeIsDerivation` | invariant | the tree ancestors of each capability are exactly its derivation history |
| `RightsNeverWiden`, `SameOrInnerObject`, `BadgesFixed`, `CopiedOnlyWithCopy` | invariant | along every derivation history: rights only shrink, the object stays or is one retyped from it, a badge once set stays, and every capability copied from held `copy` |
| `ParentCovers`, `OriginsAcyclic` | invariant | each capability names its tree parent's object or one retyped from it, through any number of retypes (the audit's check on each edge); no object lies inside itself |
| `CapsImmutable` | step | a capability never changes in its slot |
| `NoDerivationUnderRevoke` | step | nothing new is derived from a marked capability or below it |
| `RevokeIsComplete` | step | when a revoke ends, nothing derived from its capability exists |
| `RevokeTerminates` | liveness | every revoke ends |

`Capabilities.cfg` checks the safety properties with 4 slots, 2 objects,
2 rights and derivations 3 deep, using the symmetry of slots and objects:
about 2.8 million distinct states, four minutes on 4 CPUs.
`CapabilitiesRetype.cfg` checks them with 3 slots and 3 objects, so that
memory can be retyped from memory that was itself retyped: about 190,000
states, ten seconds. `CapabilitiesLiveness.cfg` checks everything,
liveness included, with 3 slots, 2 objects and depth 2 and no symmetry
(TLC cannot use symmetry for liveness): about 220,000 states, half a
minute. To check that the properties have teeth, each guard of the model
was removed in turn and TLC found the violation: deriving under a revoke
(`NoDerivationUnderRevoke`), deleting without moving children up
(`TreeIsDerivation`), deriving with the mask alone (`RightsNeverWiden`),
moving a marked capability (`EmptySlotsAreBlank`), and a spec without
fairness (`RevokeTerminates`). An edge check that accepts only the object
a capability was retyped from directly, as the store's audit first did,
fails `ParentCovers` in four steps: retype memory twice, then delete the
capability in the middle. The property tests found that bug in the audit
before retyping was in the model.

The model leaves out spaces, `transfer`, sealing, ranges, kinds and the
memory blocks themselves: those are checks on single capabilities, which
the Kani proofs cover, and the property tests run them inside the store.
It lets any object be retyped, a superset of what the store allows.

**The `caps` boot test** runs the `caps` self-test on 4 CPUs in QEMU
(`kernel/radix/src/captest.rs`; `radix-debug(7)` lists its parts). It is
the exit criterion's "in-kernel test suite": it tries every way of using
or copying a capability not held, every one of the 4,096 rights masks
with every use the mask leaves out, badges, seals, ranges, retyping with
the frame allocator's accounting, revocation across spaces and in steps,
the depth limit, 100,000 random operations with the audit in between, and
revocation racing derivations on all CPUs, and finally checks that nothing
leaked from the store, the heap or the frame allocator. On QEMU without
KVM it takes about a tenth of a second.

## Limits

- Since Phase 5 programs use the store through system calls, and
  threads, address spaces, endpoints, notifications, rings, the random
  number generator and the kernel log have kernel objects behind them
  (`docs/design/ipc.md`); since Phase 6 so do processes
  (`docs/design/processes.md`).
- One store and one lock for the whole kernel. That is simple and
  obviously atomic. Lookups no longer take it when nothing has changed
  (see *In the kernel*), but every change serializes on it and empties
  every CPU's cache. Finer locking (per space, with a tree lock for
  derivation and revocation) can come when real workloads show the
  need; the TLA+ model would have to grow with it.
- The lock is a mutex (since Phase 4), and the store allocates under it
  with `MaySleep`. Retyping frames records them under the lock but zeroes
  them after it is let go (since Phase 6): the retyping thread zeroes
  them in steps of 64 pages before its call returns, a thread that maps
  one sooner zeroes the rest itself, and deleting a frame part-way waits
  only for the step under way, so its memory never goes back while it is
  being written. So a large retype (up to 1,024 frames of up to 2^20
  pages each) no longer holds up other changes to the store.
- Only memory, frames and DMA buffers are retyped. Kernel objects that
  need memory of their own (threads, page tables) will be retyped from
  memory objects too, so that a process pays for its kernel objects.
- The Kani proofs cover the pure checks, not the store; the store is
  covered by its audit under the property tests and the self-test, and its
  tree by the TLA+ model, which is checked on small models only.
