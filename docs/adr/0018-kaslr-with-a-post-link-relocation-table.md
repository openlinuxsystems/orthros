<!--
SPDX-FileCopyrightText: 2026 The OrthrOS contributors
SPDX-License-Identifier: Apache-2.0 OR MIT
-->

# 0018. KASLR moves the kernel at boot using a relocation table written after linking

- Status: Accepted
- Date: 2026-10-20

## Context

The roadmap's Phase 2 asks for kernel address space layout randomization
with a relocatable kernel. The kernel is an `ET_EXEC` ELF linked at a fixed
address in the top 2 GiB with the `kernel` code model (ADR 0011), which
GRUB's Multiboot2 loader places at its physical addresses; GRUB cannot
relocate it, and refuses ELF files that carry relocation sections.

The usual alternatives each have a cost:

- building the kernel position-independent (`-fPIE` style, `R_X86_64_RELATIVE`
  relocations) gives up the `kernel` code model's cheap 32-bit absolute
  addresses, needs a dynamic-relocation pass anyway, and changes how
  `core` is compiled;
- a separate decompressor or loader stage, as Linux has, adds a second
  boot program and its own build.

Linux's x86 kernel uses a third way: it is linked at a fixed address with
`--emit-relocs`, a build tool extracts the relocations that matter into a
table, and early boot code applies it.

## Decision

- The kernel is linked with `--emit-relocs`. After linking, `ob build`
  reads the relocation records, keeps every location that holds an
  absolute address into the higher-half image (64-bit `R_X86_64_64`
  values, slots of the global offset table, and 32-bit sign-extended
  `R_X86_64_32S` values), and writes their physical addresses into the
  image's 64 KiB `.krelocs` section in the `radix-relocs` format. It then
  removes the relocation sections with the toolchain's `llvm-objcopy`.
  Relocation types the kernel does not expect fail the build.
- The 32-bit boot trampoline, running at the link address, picks a slide
  in 2 MiB steps such that the image and its moved copy fit in the top
  GiB, maps the image a second time at the slide, adds the slide to every
  listed location, and enters the kernel at its new address.
- The direct map and the mapping window are randomized too, on 1 GiB
  boundaries, in a 64 TiB and a 32 TiB area of the higher half.
- Random numbers come from `RDSEED` or `RDRAND`, mixed with the TSC.
  (Replaced by the kernel's random number generator, ADR 0022.)
- The `nokaslr` kernel flag turns all of it off; `ob run --gdb` adds it.
- The boot log and `kdb` show the layout; panic reports show run-time
  addresses, and symbol lookups subtract the slide.

## Consequences

- The kernel keeps the `kernel` code model and one ELF file; the relocation
  pass is about 30 lines of boot code and a build step like the symbol
  table's (ADR 0016).
- The image slide has about 9 bits of entropy (509 places with a 4 MiB
  image), the direct map about 16 and the mapping window 15. That stops
  attacks that hard-code addresses, not one that can probe repeatedly.
- The layout is printed in the boot log. That is fine while the kernel has
  no unprivileged users; when user programs arrive (Phase 6), the log must
  be readable only by privileged users, or the addresses left out of it.
- The build depends on `llvm-objcopy` from the `llvm-tools` component,
  which the pinned toolchain already lists.
- A kernel built without `ob` has an empty table and runs at its link
  address; the boot log says so.
- Revisit if the kernel grows past what fits twice in 1 GiB, or to
  randomize at a finer granularity than 2 MiB, which would need the
  boot page tables to map the image with 4 KiB pages.
