<!--
SPDX-FileCopyrightText: 2026 The OrthrOS contributors
SPDX-License-Identifier: Apache-2.0 OR MIT
-->

# 0020. Physical memory is managed in per-node DMA32 and Normal zones with per-frame records

- Status: Accepted
- Date: 2026-10-20

## Context

The roadmap's Phase 2 asks for a buddy frame allocator "with zones and
per-NUMA-node pools", a kernel heap of slab caches with per-CPU magazines,
and a page-table manager with PCIDs. The spec leaves open which zones,
where per-frame metadata lives, how large blocks get, and how the kernel's
half of the address space is laid out. The reference laptop has one NUMA
node and 8 GiB or more of RAM; servers that may run OrthrOS later have
several nodes and devices that can only address 32 bits.

## Decision

- Each NUMA node (from the ACPI SRAT, at most 8) has two zones: `Dma32`,
  its RAM below 4 GiB, and `Normal`, the rest. There is no 16 MiB ISA zone.
  Allocations fall back from the local node's `Normal` zone to its `Dma32`
  zone to the other nodes by SLIT distance. Each zone keeps a reserve,
  1/128 of its frames between 64 KiB and 4 MiB, for atomic callers, and
  allocations that could use memory above 4 GiB leave 1/16 of each `Dma32`
  zone (at most 64 MiB) alone.
- Every managed frame has a 16-byte record in an array per region of RAM,
  set aside when memory is handed over at boot (0.4% of RAM). The buddy
  allocator's free lists and the slab layer's bookkeeping live in these
  records, so neither writes to the memory it manages except slab objects'
  free pattern.
- Buddy blocks go up to order 10 (4 MiB). Heap objects come from 32 size
  classes up to 8 KiB; larger requests take frames directly.
- The kernel's higher half is laid out as: the direct map of all RAM in a
  64 TiB area, a 512 GiB mapping window for stacks and device memory in a
  32 TiB area, and the kernel image in the top 2 GiB (ADR 0018 randomizes
  all three). The lower 128 TiB belongs to address spaces.
- Address spaces get PCIDs from a generation-based allocator (as Linux does
  for arm64 ASIDs); the kernel uses PCID 0. TLB shootdowns are synchronous
  IPIs: the initiator waits until every target has flushed.

## Consequences

- One code path serves single-node laptops and multi-node servers; a laptop
  simply has one node with two zones.
- Frame records cost 4 MiB per GiB of RAM, and hot-plugged memory would
  need records allocated later, which is not supported.
- Synchronous shootdowns keep unmapping simple and correct but cost a
  round trip to every CPU running the address space; batching and lazy
  flushing can come later if benchmarks show the need.
- Revisit the zone split if a device class needs memory below 1 MiB or
  16 MiB, and the 8-node limit if larger machines become targets.
