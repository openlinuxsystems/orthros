<!--
SPDX-FileCopyrightText: 2026 The Dukovo contributors
SPDX-License-Identifier: Apache-2.0 OR MIT
-->

# 0200. uACPI runs the firmware's AML in the kernel

- Status: Accepted
- Date: 2026-10-30

## Context

Phase 11c, its first part. Much of a PC is described only in AML, the
bytecode of the DSDT and SSDTs, which an operating system runs: a
laptop's battery (`_BIF`, `_BST`), its AC adapter (`_PSR`) and lid
(`_LID`), its thermal zones and their trip points, a power button that is
a device of the namespace, the embedded controller's events, the
general-purpose events' methods, and what the firmware does before a
sleep state (`_PTS`). ADR 0013 left the interpreter to the phase that
first needs one, between the rust-osdev `aml` crate and uACPI in a
compartment, and ADR 0142 heard the fixed power button without one.
Chase chose uACPI on 2026-10-27: laptop firmware, the Dell's and the
Framework's among it, is written for Windows and tested against it, and
needs an interpreter that behaves as Windows does where the
specification leaves room.

uACPI, by Daniil Tatianin and its contributors (MIT), is some 26,000
lines of C11 that a kernel builds into itself. It follows the NT
interpreter's behavior where AML relies on it, its authors fuzzed it and
run their own test suite of AML against it, and several kernels use it,
Managarm and Ironclad among them. It needs about forty functions from the
kernel (`uacpi_kernel_*`): mapping physical memory, I/O ports, PCI
configuration space, time, sleeping, mutexes and events, spin locks,
deferred work, and installing the System Control Interrupt's handler.
Version 7.0.0 came out on 2026-10-16; commit c0edece after it fixes a
table lock left held when a table's checksum is wrong.

A compartment, a crate without `unsafe`, cannot hold C. Running AML in a
process of its own would not isolate much either: AML reads and writes
whatever I/O ports, physical memory and PCI configuration space the
firmware names, by design, so that process would need all of them, and
the SCI. Linux, the BSDs and Windows run their interpreters in the
kernel, and the firmware that provides the AML is trusted already: it
runs in System Management Mode, beneath the kernel.

## Decision

- **uACPI 7.0.0, at commit c0edece, is in `kernel/lib/uacpi/vendor`,
  unmodified** but for its build files, left out. The crate
  `radix-uacpi`'s build script compiles it with the Clang the ports are
  built with (20 or later) for the kernel's target, as the kernel's Rust
  is compiled: the `kernel`
  code model, no red zone, general-purpose registers only, frame pointers
  kept, and each function's frame in `.stack_sizes`, so `ob build` holds
  the C to the 2 KiB frame budget of ADR 0058 too (`-Wframe-larger-than`
  already fails a frame over it). Frees say their size and allocations
  come zeroed from the heap (`UACPI_SIZED_FREES`,
  `UACPI_NATIVE_ALLOC_ZEROED`). Building the kernel now needs Clang and
  LLVM's archiver too, which `ob doctor` already checks for the ports.
- **The crate is part of the trusted computing base.** It is
  `#![deny(unsafe_code)]`, and allows it only in `ffi`, where the C is
  called and calls back. The callbacks are built on a safe `Host` trait
  the kernel implements:
  - uACPI's one blocking primitive, a counting semaphore, which its
    mutexes and events are made of, is an atomic count whose waiters
    sleep in the kernel's wait table; giving a unit never sleeps, so it
    works in interrupt handlers;
  - its spin locks are atomic flags held with interrupts masked, outside
    the lock checker, which sees only the kernel's locks; uACPI takes no
    kernel lock while it holds one;
  - its memory comes from the kernel's heap, as an allocation that may
    sleep or not by where it is called from;
  - its deferred work goes to two kernel threads: `acpi-events`, pinned
    to the boot CPU as uACPI asks, runs general-purpose events' methods,
    since some firmware's System Management Mode code expects them there,
    and `acpi-notify` runs `Notify`'s handlers. Each item's room in the
    queues is kept when uACPI creates it, so queuing never allocates in
    an interrupt handler.
- **Physical memory**: RAM is reached through the direct map; anything
  else, device registers or firmware memory, is mapped uncached in the
  mapping window and unmapped when uACPI is done with it. A mapping over
  256 MiB is refused, so that a region the firmware declares too large
  fails alone instead of costing the kernel its memory for page tables.
  **PCI configuration space** is reached through ECAM, as the PCI scan
  does (ADR 0052). **The RSDP** uACPI starts from is the copy GRUB put
  in the boot information, which the kernel keeps, or the one it found
  in the BIOS areas.
- **At boot, after the PCI scan, uACPI starts** in the kernel's main
  thread: it reads the tables, saying only what goes wrong, as the kernel
  lists them itself; takes the machine into ACPI mode; routes the SCI
  through the I/O APIC, as ADR 0142 does; loads the DSDT's and SSDTs'
  AML and runs the devices' `_STA` and `_INI`. The kernel then hears the
  fixed power button through uACPI's fixed event, and turns on each
  general-purpose event that has a method, once its own handlers are in.
  `ACPI: uACPI 7.0.0 runs the firmware's AML` says it is done, after
  uACPI's own lines on what it loaded.
- **Powering off goes through uACPI**: once Spark asks and the disks are
  written back, uACPI runs `_PTS(5)` and enters S5 with the sleep types
  of `\_S5`. If it cannot, the kernel's own S5 of ADR 0099 is tried, as
  before.
- **Without uACPI** (`aml=off`, a new boot flag, or when uACPI cannot
  start), no AML runs, and the kernel hears the fixed power button by
  itself (ADR 0142). The flag is for firmware whose AML misbehaves.

## Consequences

- The crate's host tests run uACPI on a simulated PC, with tables
  compiled by ACPICA's `iasl` from `src/tests/dsdt.asl` and kept beside
  it: from the RSDP to S5, through a device's `_INI`, an AML mutex and
  `Sleep`, a `PCI_Config` and a `SystemMemory` region, the power button,
  a general-purpose event's method run from its queue, and `_PTS`.
- Every boot test now runs uACPI on QEMU's DSDT, under BIOS and UEFI;
  `boot_report` looks for its line, an `ACPI:` error fails the test, and
  `power-button` presses the button through it. The `aml-off` case boots
  with `aml=off` and powers off through the kernel's own path.
- The kernel's trusted computing base grows by uACPI's C. A bug in it is
  a bug in the kernel; `aml=off` is the way around one until it is fixed
  upstream and vendored again.
- The machine stays in ACPI mode from boot, as since ADR 0142, but now
  the general-purpose events whose methods the firmware has are handled:
  on laptops, the embedded controller's, the lid's and the battery's.
- Not yet: the battery, the AC adapter, the lid and a power button of the
  namespace, which come next in 11c (ADR 0201); thermal zones and CPU
  frequency (ADR 0202); sleep states other than S5; `_PRT` routing of
  legacy interrupts, which devices on MSI and MSI-X do not need; and
  uACPI's own test suite run in CI.
