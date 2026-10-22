<!--
SPDX-FileCopyrightText: 2026 The Dukovo contributors
SPDX-License-Identifier: Apache-2.0 OR MIT
-->

# 0010. Phase 0 builds its image with the host's GRUB

- Status: Accepted
- Date: 2026-10-20

## Context

The roadmap pins GRUB by building it from source and, from Phase 14,
assembles the USB image with Dukovo's own `rdisk`, `mkfs-btrfs`,
`mkfs-fat` and `rgrub-setup`. Phase 0 only needs to prove that GRUB loads
the kernel under UEFI and BIOS, from USB and optical media.

## Decision

Until Phase 14, `ob image` builds a hybrid image with `grub-mkrescue`,
using the GRUB that `ob doctor` finds on the host. The pinned version is the
one in the build container (Ubuntu 24.04, GRUB 2.12), which CI also uses.
`ob image` names its output `.img` and `ob image --iso` names it `.iso`; in
Phase 0 the two files have the same contents. The kernel builds for Rust's
built-in `x86_64-unknown-none` target until Phase 1 introduces a custom
target for the higher-half kernel.

## Consequences

- The image boots on UEFI and BIOS machines from a USB stick, hard disk or
  CD, with no Dukovo-specific image tooling yet.
- Hosts with a different GRUB release may produce slightly different
  images. Builds through `./ob --container` use the pinned GRUB.
- Images are not yet bit-for-bit reproducible, because grub-mkrescue writes
  random identifiers. Reproducible images come with the Phase 14 pipeline.
