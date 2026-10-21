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
| [0025](0025-dco-sign-off-and-project-policies.md) | Contributions are signed off under the DCO, with a code of conduct, governance and security policy | Accepted |
| [0026](0026-reuse-and-credits.md) | The repository follows REUSE and credits every upstream project | Accepted |
| [0027](0027-scheduler-design.md) | The scheduler is EEVDF with FIFO and round-robin classes, per-CPU queues and no lock across a switch | Accepted |
| [0028](0028-lock-checker-in-debug-builds.md) | The lock checker is on in debug builds, off in release builds, and switchable at boot | Accepted |
| [0029](0029-resource-domains-and-out-of-memory-policy.md) | Resource domains are capability-named kernel objects, and running out of memory stops whole domains | Accepted |
| [0030](0030-verifying-the-scheduler.md) | The scheduler is verified with a TLA+ model of its switch protocol, Kani, and algebra backed by tests | Accepted |
| [0031](0031-system-call-interface.md) | Programs enter Radix with SYSCALL, through twelve system calls in registers, and every other operation is a capability method | Accepted |
| [0032](0032-endpoints-notifications-and-channels.md) | IPC is synchronous endpoints with a register-only fast path, seL4-style notifications, and channels built on shared memory | Accepted |
| [0033](0033-ridl-interface-definitions.md) | The system call interface is defined once in ridl, and ridlc generates the Rust bindings, C headers and man(2) pages | Accepted |
| [0034](0034-radix-ring.md) | radix-ring batches operations through shared submission and completion queues, after io_uring, and trusts nothing the program writes | Accepted |
| [0035](0035-benchmarks-with-a-linux-baseline.md) | System call and IPC costs are measured on every merge, next to the closest Linux operations on the same machine | Superseded by [0039](0039-linux-in-the-same-virtual-machine-and-a-performance-gate.md) |
| [0036](0036-kernel-started-user-test-programs.md) | Until Phase 6, the kernel starts user-mode test programs from one image the boot loader loads, and the store gains random, ring and kernel_log objects | Accepted |
| [0037](0037-lazy-timer-and-guest-halt-polling.md) | The timer is written only to fire earlier, and idle virtual CPUs poll before they halt | Accepted |
| [0038](0038-channel-wake-ups-per-direction.md) | A channel signals the other end only when it says it may sleep, with one word for each way of waiting | Accepted |
| [0039](0039-linux-in-the-same-virtual-machine-and-a-performance-gate.md) | Radix is held to Linux in the same kind of virtual machine, and CI fails when it is slower | Accepted |
| [0040](0040-hot-paths-without-hidden-costs.md) | The release kernel is built with link-time optimisation, and its hot paths avoid locked instructions, string moves and 128-bit division | Accepted |
| [0041](0041-yield-forfeits-its-slice-and-ipis-end-first.md) | A yield gives up the rest of its slice, and a TLB shootdown's interrupt ends before the flush | Accepted |
| [0042](0042-batched-random-and-lighter-system-calls.md) | Per-CPU random generators work a batch at a time, and system calls and wake-ups skip work they do not need | Accepted |
