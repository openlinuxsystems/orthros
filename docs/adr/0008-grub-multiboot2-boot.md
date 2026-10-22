<!--
SPDX-FileCopyrightText: 2026 The Dukovo contributors
SPDX-License-Identifier: Apache-2.0 OR MIT
-->

# 0008. Boot through GRUB 2 with the Multiboot2 protocol

- Status: Accepted
- Date: 2026-10-20

## Context

Section 12.3 of the specification makes GRUB 2 the reference boot loader on
UEFI and legacy BIOS. GRUB can start a kernel through the Linux boot
protocol, Multiboot or Multiboot2.

## Decision

Radix is an ELF executable with a Multiboot2 header. GRUB loads it with the
`multiboot2` command on both UEFI and BIOS. The kernel starts in a small
32-bit trampoline (`kernel/arch/x86_64/src/boot.rs`) that enters long mode
and calls Rust code. The kernel is linked at physical address 1 MiB in
Phase 0; Phase 1 moves it to the higher half.

## Consequences

- One kernel image boots under both firmware types, and the boot
  information (memory map, command line, framebuffer, ACPI tables, EFI
  tables) arrives in one documented structure, parsed by
  `kernel/lib/bootinfo`.
- The kernel cannot be started directly as an EFI application; an EFI stub
  remains a possible secondary option, as the specification allows.
- The boot sequence and kernel command line are documented in
  `dukovo-boot(7)`.
