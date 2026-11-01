<!--
SPDX-FileCopyrightText: 2026 The Dukovo contributors
SPDX-License-Identifier: Apache-2.0 OR MIT
-->

# 0270. A RISC-V smoke boot, and RISC-V stubs of the HAL, keep the kernel portable

- Status: Accepted
- Date: 2026-11-01

## Context

RISC-V is release 1.15's port (the roadmap's phase 37: SBI, PLIC or AIA,
QEMU's `virt` machine, then one board). Since Phase 1, CI has compiled
the architecture-independent kernel against AArch64 stubs of the HAL,
so that x86-64 details cannot reach past the HAL unseen. That catches
what does not compile; it says nothing of a boot path. Chase asked
(2026-10-26) for a RISC-V smoke test alongside Phase 11, so that
release 1.15 stays a port of size M and not a rewrite: a riscv64
target next to the AArch64 stubs,
booting through OpenSBI on QEMU `virt`, printing a banner over the SBI
console, turning Sv39 paging on, taking one SBI timer interrupt and
powering off through SBI, cross-built and booted under TCG by CI on
every push.

OpenSBI (BSD-2-Clause) is the SBI implementation QEMU ships and starts
in machine mode; it jumps to 0x80200000 in supervisor mode with the
hart's ID and the device tree. The *RISC-V Supervisor Binary Interface
Specification* (2.0) and the *RISC-V Privileged Architecture*
(20211203) describe the calls, the CSRs and Sv39. Rust has the
`riscv64gc-unknown-none-elf` target, whose `core` builds from the
toolchain's sources as the kernel's does, so the pinned toolchain needs
nothing more.

## Decision

- `kernel/arch/riscv64` (`radix-arch-riscv64`) holds RISC-V stubs of
  every HAL trait, as `kernel/arch/aarch64` does, and `ob lint` runs
  Clippy on the kernel against them for `riscv64gc-unknown-none-elf`.
- Beside them, its `smoke` module and the program `radix-riscv64-smoke`
  are the smoke boot, its own linker script placing it at 0x80200000.
  `_start` sets up the stack, zeroes the data and calls it; it asks the
  SBI's base extension which SBI it is and prints the banner through
  the Debug Console extension (or the legacy console), with the model
  the device tree names; turns Sv39 on with the first gigabyte of RAM
  mapped where it is and at 0xffffffc080000000, and reads one word
  through both; asks the Timer extension for an interrupt a millisecond
  later and waits for it with its trap handler; and powers off through
  the System Reset extension. Anything unexpected (a trap, a panic,
  paging off) says why and resets as a system failure.
- The crate is `#![deny(unsafe_code)]`, allowing it only in the smoke
  module: its assembly, SBI calls, CSRs and the device tree.
- `ob test riscv` builds the program and boots it on QEMU's `virt`
  machine with `-bios default`, without KVM, and passes when its five
  lines come in order and QEMU powers off by itself within a minute. CI
  runs it after `ob lint` on every push. `ob doctor` notes whether
  `qemu-system-riscv64` is installed.
- Nothing of the common kernel runs on RISC-V yet; the smoke boot is
  the start of the port's boot path, not a kernel.

## Consequences

- A change that breaks the kernel's build for RISC-V, or the boot path a
  port starts from, fails CI the day it is made. The boot takes a
  fraction of a second, the build seconds.
- The HAL's traits now have two sets of stubs to keep in step; both are
  placeholders that only have to compile.
- CI and hosts that run `ob test riscv` need QEMU's RISC-V emulator
  (`qemu-system-misc`), which brings OpenSBI's firmware.
- Phase 37 starts from here: interrupts through the PLIC or AIA, SMP
  through SBI's hart state management, the kernel's own page tables and
  user mode replace the stubs one trait at a time.
