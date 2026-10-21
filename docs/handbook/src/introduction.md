<!--
SPDX-FileCopyrightText: 2026 The OrthrOS contributors
SPDX-License-Identifier: Apache-2.0 OR MIT
-->

# Introduction

OrthrOS is an operating system built around **Radix**, a kernel written in
Rust. Radix is a hybrid design: a small trusted core holds the code that
must be trusted, and everything else (drivers, file systems, the network
stack) runs as restartable compartments that cannot use `unsafe` Rust.
Programs reach resources through capabilities, unforgeable references that
carry exactly the rights a program was given.

On top of Radix, OrthrOS provides a complete, installable system:

- **Spark**, the init and service manager, and **Nukesh**, the shell, with
  the usual Unix utilities;
- **Btrfs** as the native file system, with snapshots and atomic,
  roll-back-able system updates;
- wired networking with DHCP and DNS, users and logging;
- **Lumen**, a Wayland compositor, without a desktop environment on top;
- an installer with the `rdisk` partitioner, delivered as a USB image.

## Where the project is

OrthrOS is built in phases, each ending in something that runs. This
handbook describes **Phase 4**: the Radix kernel boots through GRUB under
UEFI and legacy BIOS at a random address, chosen by a cryptographic random
number generator seeded from several sources, manages the machine's memory
with a frame allocator per NUMA node and a kernel heap, protects itself
with W^X page tables, guard pages, SMEP and SMAP, starts every CPU, reads
the firmware's ACPI tables, takes timer interrupts and shows its log on
the screen and the serial port, with crash reports, a kernel debugger and
a crash log for when things go wrong. It also has its capability core, the
store of unforgeable capabilities through which programs will reach every
resource once they arrive (see `radix-capabilities(7)`), and its
scheduler: threads on every CPU, shared fairly or run first by real-time
priority, grouped into resource domains with CPU weights and memory
limits, with an out-of-memory policy that stops apps before services and
never the system (see `radix-scheduling(7)`). Chapters for later phases are listed in the table of
contents and are written as those phases land. The full plan is in
[`docs/roadmap.md`](https://github.com/openlinuxsystems/orthros/blob/main/docs/roadmap.md).

## How this handbook is organised

- **Getting started** covers building OrthrOS, making install media and
  installing it.
- **Using OrthrOS** covers the system itself, from the shell to updates.
- **Reference** lists supported hardware, explains how to debug the
  kernel, and lists the manual pages. Every
  command, file format and interface has a man page; this handbook explains
  how the pieces fit together.
