<!--
SPDX-FileCopyrightText: 2026 The OrthrOS contributors
SPDX-License-Identifier: Apache-2.0 OR MIT
-->

# Memory management

This note describes how the Radix kernel manages memory as of Phase 2: the
frame allocator, the kernel heap, the page tables, KASLR, and how all of it
is verified. The user-facing description is the *MEMORY* and *KASLR*
sections of `orthros-boot(7)`; `radix-debug(7)` covers heap corruption
reports, the `mem` and `heap` commands of `kdb`, and the self-tests.

## Crates

```text
kernel/mm               radix-mm: frame allocator, heap, mapping-window ranges,
                        PCID allocator, stress test; host tests and Kani proofs
kernel/lib/relocs       radix-relocs: the kernel relocation table format (KASLR)
kernel/arch/x86_64      memory.rs (layout, boot allocator, handover), paging.rs
                        (kernel page tables, TLB shootdown), space.rs (address
                        spaces), kaslr.rs (moving the image at boot)
tools/xtask             krelocs.rs: writes the relocation table after linking
```

`radix-mm` is architecture-independent and builds for the host, where its
unit and property tests run, and for Kani. It reaches the machine only
through its `Platform` trait (current CPU, the CPU's NUMA node, interrupt
masking, the direct map's address), which the x86-64 layer implements. It
is `#![deny(unsafe_code)]`: the frame allocator, the free lists and the
magazines are safe code, and `unsafe` is allowed only in the few modules
that touch the memory they manage (`raw`, the slab layer's free-object
pattern, the heap's `free`, the `Platform` trait and the stress test),
each with its contract written down. See `docs/coding-standard.md`.

## Boot

Memory comes up in four steps, because each needs the one before:

1. `radix_arch_entry` records the memory map and reserves what must not be
   reused. A bump allocator hands out 4 KiB frames for page tables and
   stacks; it never frees.
2. `memory::choose_layout` picks random places for the direct map and the
   mapping window (see *KASLR*) and installs the direct map's first 4 GiB
   in the boot page tables, so the boot information can be read through it.
3. `paging::build_and_switch` builds the kernel's page tables, checks W^X
   and loads them, and turns on SMEP, SMAP and PCIDs where the CPU has them.
4. After the ACPI tables are read, `memory::start_frame_allocator` hands
   memory over: it reads the NUMA nodes from the SRAT (one node without
   it) and their distances from the SLIT (10 and 20 without it), splits
   each RAM range at 4 GiB and at node boundaries, sets aside a 16-byte
   frame record per frame for each piece (from the bump allocator), gives
   the pieces to the frame allocator, and releases every frame the bump
   allocator has not handed out. Frames it did hand out stay allocated,
   owned by the page tables or per-CPU data. From then on every allocation,
   including the architecture's own (`memory::alloc_pages`), goes through
   the frame allocator. The heap's reclaim hook is registered here too.

## Frame allocator

`radix_mm::Frames` (`frames.rs`) keeps one buddy allocator per zone. Each
NUMA node (at most 8) has two zones: `Dma32`, its RAM below 4 GiB, and
`Normal`, the rest. A zone may hold up to 32 regions of RAM, each with its
array of frame records.

`buddy.rs` hands out blocks of 2^order frames, order 0 to 10 (4 KiB to
4 MiB), naturally aligned. The free lists are doubly linked through the
frame records of the blocks' first frames, so the allocator never reads or
writes the memory it manages. A frame record (`page.rs`) holds the frame's
state (`Reserved`, `Free`, `Allocated` or `Tail`), the block's order, its
owner (an `Owner`: `Kernel`, `PageTable`, `Slab`, `Heap`, `Area`,
`PerCpu` or `Memory`, the last for the capability store's memory objects,
used for accounting and for checking frees), a tag byte and two links; the slab
layer reuses the links and an auxiliary word for its own bookkeeping. A free
names the block's order and owner, and is refused, changing nothing, unless
exactly that block is allocated to that owner.

An allocation takes `Flags` (`context.rs`, spec 3.4):

| Field | Values | Effect |
| --- | --- | --- |
| `Context` | `Atomic`, `MaySleep` | `Atomic` may take a zone's reserve and never reclaims; `MaySleep` leaves the reserve alone and runs reclaim before failing |
| `ZoneLimit` | any, `Dma32` | `Dma32` takes only memory below 4 GiB |
| `NodePolicy` | local, prefer _n_, only _n_ | which node first, and whether others may be used |
| zeroed | yes or no | zero the memory before returning it |

Zones are tried in fallback order: the chosen node's `Normal` zone, then
its `Dma32` zone, then the other nodes nearest first by SLIT distance. Two
floors protect zones: every zone keeps a reserve of 1/128 of its frames,
between 64 KiB and 4 MiB, for `Atomic` callers; and on machines with memory
above 4 GiB, allocations that could use it leave 1/16 of each `Dma32`
zone, at most 64 MiB, for devices that need it. A `MaySleep` allocation
that finds nothing calls the reclaim hook (`Heap::drain`, which empties
every CPU's magazines and frees empty slabs) and tries once more. Failure
is `AllocError`, never a panic.

## Kernel heap

`radix_mm::Heap` (`heap.rs`) serves requests up to 8 KiB from 32 size
classes (`class.rs`), 16 bytes to 8 KiB, a quarter of a power of two
apart. An object's alignment is the largest power of two dividing its size,
up to a page; a request with a stricter alignment moves up to the first
class that provides it. Larger requests get a block of frames of their own
from the frame allocator.

Each class has a slab cache (`slab.rs`). A slab is a block of 1 to 8
frames, the smallest that holds eight objects (for the classes above
4 KiB, 32 KiB slabs holding four to six); its bookkeeping (links into the
cache's list of partly used slabs, the class, the first free object and
the count in use) lives in the frame record of its first frame, so slabs
have no headers. Free objects form a list through the objects: the first
word holds a marker and the next free index, the second a poison pattern.
Both are checked when the object is handed out, and a free checks that the
address is the start of an object that is in use. Damage is reported to
the corruption handler (the kernel panics with a `heap corruption:` message)
and the object or slab is quarantined, never used again.

In front of each slab cache is a magazine layer (`magazine.rs`, after
Bonwick and Adams). Each CPU has two magazines per class, *loaded* and
*previous*, stacks of free objects: up to 30 for classes up to 256 bytes,
15 up to 1 KiB and 7 above, so idle caches hold less memory. Allocations pop and frees push
on the loaded one; when it is empty or full the CPU swaps the two, and only
when both are does it trade a magazine with the class's depot, a locked
list of full and empty magazines (32 of each at most). Magazines are
256-byte objects of the heap's own 256-byte class. A CPU's caches live in a
frame on its own node.

Locks are taken in one order: a CPU's cache, a class's depot, a class's
slab cache, a frame-allocator zone, each with interrupts disabled. A new
slab is allocated with no lock held, and the heap's bookkeeping never
triggers reclaim, so the reclaim hook can take every lock.

### Explicit allocation context

The kernel's `#[global_allocator]` is `ExplicitOnly`, which refuses every
request, and `alloc` is built with `no_global_oom_handling`, which removes
the infallible APIs (`Box::new`, `Vec::push`, `format!`) altogether. Code
that wants an `alloc` collection passes an allocator that carries flags,
`HEAP.with(Flags::MAY_SLEEP)` (a `Kmalloc`), and uses the fallible APIs:
`Vec::try_reserve`, `Box::try_new_in`. So every allocation in the kernel
states its context and handles failure, and the compiler enforces it
(ADR 0017).

## Virtual memory

| Range | Size | Contents |
| --- | --- | --- |
| `0x0000_0000_0000_0000`–`0x0000_7FFF_FFFF_FFFF` | 128 TiB | lower half: address spaces (user programs, from Phase 5) |
| `0xFFFF_8000_0000_0000`–`0xFFFF_BFFF_FFFF_FFFF` | 64 TiB | the direct map, at a random 1 GiB boundary |
| `0xFFFF_C000_0000_0000`–`0xFFFF_DFFF_FFFF_FFFF` | 32 TiB | the 512 GiB mapping window, at a random 1 GiB boundary |
| `0xFFFF_FFFF_8000_0000`–top | 2 GiB | the kernel image, moved by the KASLR slide within the top GiB |

`paging.rs` builds the kernel's half of the page tables once; it never
changes the top-level entries afterwards, so every address space can copy
them. The image is mapped with 4 KiB pages: text read-only and executable,
read-only data and the `.ksyms` and `.krelocs` tables read-only, data and
bss writable and not executable, and the boot CPU's stack guard page left
out. The direct map covers every RAM range of the memory map, writable and
not executable, with 1 GiB pages where the CPU has them and the range
allows, 2 MiB pages next and 4 KiB pages at the edges; the image's text and
read-only data are read-only there too, so they cannot be changed through
their alias. After building the tables, `build_and_switch` walks them and
counts any mapping that is both writable and executable (W^X); the boot log
reports the result.

The mapping window holds stacks, device registers, the framebuffer and
other areas that need contiguous virtual memory. `radix_mm::Ranges`
(`vrange.rs`) hands out pieces of it, first fit over a sorted table of
free extents; every area gets an unmapped guard page below it, so a stack
overflow faults (and is reported as such) instead of running into the next
area. `map_area` maps fresh zeroed frames there; `unmap_area` unmaps them,
shoots the range down and frees the frames.

SMEP (CR4 bit 20) makes the kernel fault if it runs code from a user page,
and SMAP (bit 21) if it touches one without first setting `EFLAGS.AC`
(`stac`/`clac` around deliberate accesses). Page faults that these cause
are reported as such.

### Address spaces and PCIDs

An `AddressSpace` (`space.rs`) has its own top-level table whose upper half
is the kernel's and whose lower half is its own, and a PCID from
`radix_mm::Pcids` (`asid.rs`). With PCIDs, the TLB keeps entries of several
address spaces tagged by identifier, and switching (CR3 with bit 63 set)
flushes nothing. There are only 4096 identifiers, so they are handed out in
generations (the scheme Linux uses for arm64 ASIDs): when a generation's
identifiers run out, the next starts, each CPU flushes its TLB on its next
switch, and spaces get new identifiers as they next run; spaces running at
that moment keep theirs. Identifier 0 is the kernel's. The `nopcid` flag
turns PCIDs off; every switch then flushes.

Each address space counts its changes in a *TLB generation*. A change
bumps the generation and then shoots down the CPUs that are running the
space; a CPU switching to a space marks itself as running it and then
reads the generation, and flushes the space's identifier if the generation
is newer than the one it last loaded. With these two orders, every CPU
either gets the shootdown or sees the new generation.

### TLB shootdown

Removing or changing a mapping that other CPUs may have cached needs a TLB
shootdown (`paging::shootdown`, `shootdown_on`). The initiating CPU takes
the shootdown lock, publishes the range and a new request number, sets a
pending bit for each target CPU, sends them the TLB IPI, flushes its own
TLB and waits for the pending bits to clear; each target flushes the range
(the whole TLB above 32 pages) and clears its bit. Two CPUs may start
shootdowns at once with interrupts disabled, so a CPU waiting for the lock,
or for its targets, serves pending requests itself; the timer tick serves
them too, in case an IPI was missed. A shootdown that is not answered
within 2 seconds panics, naming a CPU that did not answer.

## KASLR

The kernel is linked at one address, `0xFFFF_FFFF_8000_0000` plus its
physical address, with `--emit-relocs`, which keeps the linker's relocation
records in the ELF file. After linking, `ob build` (`krelocs.rs`) reads
them and keeps every location that holds an absolute address into the
higher-half image: 64-bit addresses (`R_X86_64_64`, and the global offset
table) and 32-bit sign-extended ones (`R_X86_64_32S`); relative addresses
move with their code. It writes their physical addresses, sorted, into the
64 KiB `.krelocs` section the image reserves (format in `radix-relocs`),
then removes the relocation sections with `llvm-objcopy`, because GRUB
refuses to load an ELF file that has them. The Phase 2 kernel has about
2,600 locations.

At boot the trampoline, still running at the link address, calls
`radix_kaslr_choose`, which reads the command line for `nokaslr` and picks
a slide: a whole number of 2 MiB pages such that the image and its moved
copy both fit in the top GiB, about 500 choices. The trampoline maps the
image a second time, slide bytes higher, in the same page directory, and
calls `radix_kaslr_relocate`, which adds the slide to every listed location
through the identity map. While it runs some addresses are moved and some
are not yet, but both copies of the image are mapped, so either kind still
works; both functions keep no pointers and cannot panic. The trampoline then
enters `radix_arch_entry` at the new address with the slide, which the
symbol lookups subtract (`radix/src/symbols.rs`). The direct map and the
mapping window are placed later, by `memory::choose_layout`, on 1 GiB
boundaries anywhere in their areas.

The places are drawn from the kernel's random number generator, which
`radix_kaslr_choose` seeds before anything else from `RDSEED`, `RDRAND` and
timing jitter (`random.md`, ADR 0022). The boot log prints the layout,
which is acceptable while the kernel has no unprivileged users; ADR 0018
records when that must change.

## Verification

The memory manager is checked at four levels, from cheapest to closest to
the real thing.

**Unit tests** (`cargo test`, run by `ob test unit`) cover each module on
the host with hand-written cases, including multi-threaded heap tests in
which each host thread plays a CPU.

**Property tests** use proptest (ADR 0019) to run random operation
sequences against a model:

| Test | Property |
| --- | --- |
| `buddy::behaves_like_a_set_of_disjoint_blocks` | random allocations and frees never hand out overlapping or misaligned blocks, never lose a frame, and keep every free-list invariant |
| `class::a_class_fits_its_requests` | each request gets the smallest class that holds it with its alignment |
| `magazine::loaded_and_previous_hold_what_was_pushed` | two magazines behave like one stack of twice the capacity |
| `heap::live_objects_are_disjoint` | live heap objects never overlap and keep their alignment, and nothing is left over after everything is freed |
| `vrange::ranges_never_overlap` | ranges handed out never overlap, and every byte is free or handed out |
| `asid::running_spaces_never_share_an_identifier` | two spaces never run with one identifier in one generation, and a CPU flushes before reusing an identifier from an earlier generation |

**Kani proofs** (`kernel/mm/src/proofs.rs`, run by `ob test kani` and the
CI job of that name) check a property for *every* input within stated
bounds, not a sample:

| Harness | Proves, within its bounds |
| --- | --- |
| `buddy_keeps_its_invariants` | on a 4-frame zone, every sequence of up to 3 operations, each an allocation of order 0 to 2 or a free of a live block, keeps `Buddy::check` true, conserves frames, and hands out aligned blocks that never overlap; an allocation fails only when no large enough block is free |
| `buddy_refuses_bad_frees` | after one allocation, a free is accepted for any frame number, order and owner exactly when they name the allocated block and its owner (`Buddy::free` checks this before changing anything) |
| `classes_fit_requests` | for every size up to 8 KiB + 1 and alignment up to 8 KiB, the chosen class is the smallest that fits, and every class holds 4 to 65,534 objects per slab |
| `large_orders_fit_requests` | for every size and alignment, the order chosen for a large request is the smallest block of frames that fits |
| `magazines_hold_what_was_pushed` | for every capacity up to 30 and every sequence of 4 pushes and pops, a CPU's two magazines never lose or duplicate an object, and a push fails only when both are full |
| `ranges_never_overlap` | on a 16-page window with 4 extent slots, every sequence of 3 operations, each an allocation (1 to 8 pages, alignment up to 4 pages) or a release of a live range, keeps the table sorted and disjoint, never hands out overlapping ranges, and accounts for every byte |
| `running_spaces_never_share_an_identifier` | with 3 identifiers, 2 CPUs and 4 spaces, every sequence of 4 switches leaves the two CPUs' spaces with different identifiers, through any rollover |

The bounds keep each harness within minutes; the property tests run the
same operations at larger sizes. In the Kani build `MAX_ORDER` is 2, the
largest order a 4-frame zone can hold, so the buddy proofs do not spend
their time on free lists that must stay empty, and
`buddy_keeps_its_invariants` checks the free lists once, at the end of its
sequence: every state a longer sequence passes through is the end of a
shorter one. `Buddy::check` bounds its walk of each list by the zone's
size rather than by the list, which keeps the model small too. Kani treats
atomics as plain memory accesses, so the proofs say nothing about
concurrency; the stress test covers that.

**Boot tests** run the real thing in QEMU (`ob test`): `mm-stress` has 4
CPUs on 2 NUMA nodes allocate and free about 24 million heap objects of
random sizes and contexts, and map and unmap 800 areas with shootdowns, then
checks that every frame is back and nothing was damaged; `tlb` checks
shootdowns and address-space switches across CPUs; `stack-overflow`, `smep`
and `smap` check that the protections fault and are reported correctly;
and every normal boot checks the KASLR layout, W^X and the zones.

## Limits

- Phase 2 has no user programs; address spaces are exercised only by the
  `tlb` self-test. The capability core (Phase 3, `docs/design/capabilities.md`)
  is the heap's first user besides the stress test, and takes the pages of
  its memory objects from the frame allocator.
- Reclaim only drains the heap's caches. Waiting for memory, and swapping,
  come with the scheduler (Phase 4) and Phase 10.
- NUMA has been tested only on QEMU's emulated nodes. Memory hot-plug is
  not supported; the frame records are sized for the memory map at boot.
- The kernel image's slide has about 9 bits of entropy, which is typical
  for x86-64 kernels but would not stop an attacker who can try often.
