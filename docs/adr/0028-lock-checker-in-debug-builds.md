<!--
SPDX-FileCopyrightText: 2026 The OrthrOS contributors
SPDX-License-Identifier: Apache-2.0 OR MIT
-->

# 0028. The lock checker is on in debug builds, off in release builds, and switchable at boot

- Status: Accepted
- Date: 2026-10-21

## Context

The roadmap's Phase 4 asks for lock-order checking in debug builds, after
Linux's lockdep, and its exit criterion requires it to catch a seeded
deadlock. A checker adds work to every lock taken, and its reports are
only useful if they reach someone: the boot tests mostly run release
builds, because debug builds are several times slower under QEMU.

## Decision

- `radix-lockdep` checks every spin lock and mutex through `radix-sync`'s
  hooks. Lock classes are source locations of lock creation, plus a
  nesting subclass. It reports order cycles, recursion within a class,
  interrupt-unsafe locks and sleeping in atomic context, logs each report
  as error lines starting `Lock checker:`, and turns itself off after the
  first report or when its fixed tables (512 classes, 32 held locks per
  CPU) are full.
- It is on by default in debug builds and off in release builds. The
  kernel flag `lockdep` (or `lockdep=on`) turns it on and `lockdep=off`
  turns it off in any build.
- The boot tests turn it on for `bios-usb` and every Phase 4 self-test,
  and fail any case whose log has a lock checker report, except the
  `lockdep` case, which seeds three and asks the checker to keep going.
  They also fail when the kernel logs a thread sleeping with preemption
  held off.

## Consequences

- Locking bugs show up in CI on every pull request, in release builds,
  where the first run of a bad order fails the test.
- The checker sees only orders that some run exercised; code paths the
  tests never take are unchecked.
- Fixed tables keep the checker allocation-free; a kernel that outgrows
  512 lock classes needs the limit raised, which the checker reports.
