<!--
SPDX-FileCopyrightText: 2026 The OrthrOS contributors
SPDX-License-Identifier: Apache-2.0 OR MIT
-->

# Architecture decision records

Each file records one decision: its context, what was decided and what
follows from it. See [0001](0001-record-architecture-decisions.md) for why,
and [CONTRIBUTING.md](../../CONTRIBUTING.md) for how to add one.

| Number | Decision | Status |
| --- | --- | --- |
| [0000](0000-template.md) | Template | |
| [0001](0001-record-architecture-decisions.md) | Record architecture decisions | Accepted |
| [0002](0002-license-apache-2.0-or-mit.md) | License OrthrOS under Apache-2.0 OR MIT | Accepted |
| [0003](0003-github-and-actions.md) | Host on GitHub and run CI on GitHub Actions | Accepted |
| [0004](0004-btrfs-native-filesystem.md) | Btrfs is the native file system | Accepted |
| [0005](0005-static-linking.md) | Statically link the programs in /system/bin | Accepted |
| [0006](0006-default-image-includes-lumen.md) | The default image is the full 4 GiB image with Lumen | Accepted |
| [0007](0007-closed-firmware-separate-package.md) | Closed firmware ships only as a separate optional package | Accepted |
| [0008](0008-grub-multiboot2-boot.md) | Boot through GRUB 2 with the Multiboot2 protocol | Accepted |
| [0009](0009-reference-hardware.md) | The Dell Inspiron 15 5570 is the reference machine | Accepted |
| [0010](0010-phase-0-boot-image.md) | Phase 0 builds its image with the host's GRUB | Accepted |
| [0011](0011-kernel-target-and-build-std.md) | The kernel has its own target and builds core with -Zbuild-std | Accepted |
| [0012](0012-crash-log-in-reserved-memory.md) | Keep the console output in reserved memory across warm restarts | Accepted |
| [0013](0013-own-parsers-for-boot-information-and-acpi.md) | Parse the boot information and static ACPI tables in our own crates | Accepted |
| [0014](0014-early-microcode-from-an-image-module.md) | Early microcode comes from a boot module added at image build time | Accepted |
| [0015](0015-console-font-spleen.md) | The framebuffer console uses the Spleen fonts | Accepted |
| [0016](0016-kernel-symbol-table-in-the-image.md) | The kernel carries its own symbol table, written in after linking | Accepted |
| [0017](0017-explicit-allocation-context.md) | Every kernel allocation states its context, and the global allocator refuses | Accepted |
| [0018](0018-kaslr-with-a-post-link-relocation-table.md) | KASLR moves the kernel at boot using a relocation table written after linking | Accepted |
| [0019](0019-verifying-the-memory-manager.md) | The memory manager is verified with Kani proofs and proptest properties | Accepted |
| [0020](0020-zones-nodes-and-address-space-layout.md) | Physical memory is managed in per-node DMA32 and Normal zones with per-frame records | Accepted |
| [0021](0021-license-manifest-in-every-image.md) | Every image carries a checked license manifest | Accepted |
| [0022](0022-kernel-random-number-generator.md) | The kernel's random numbers come from ChaCha20 seeded through BLAKE2s from every source | Accepted |
| [0023](0023-capability-model.md) | Capabilities carry copy and transfer rights, seal with endpoints, and revoke in marked steps | Accepted |
| [0024](0024-tla-plus-tools.md) | The TLA+ models are checked with TLC from the pinned TLA+ tools 1.7.4 | Accepted |
| [0025](0025-reuse-and-credits.md) | The repository follows REUSE and credits every upstream project | Accepted |
