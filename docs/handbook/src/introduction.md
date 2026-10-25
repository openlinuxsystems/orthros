<!--
SPDX-FileCopyrightText: 2026 The Dukovo contributors
SPDX-License-Identifier: Apache-2.0 OR MIT
-->

# Introduction

Dukovo is an operating system built around **Radix**, a kernel written in
Rust. Radix is a hybrid design: a small trusted core holds the code that
must be trusted, and everything else (drivers, file systems, the network
stack) runs as restartable compartments that cannot use `unsafe` Rust.
Programs reach resources through capabilities, unforgeable references that
carry exactly the rights a program was given.

On top of Radix, Dukovo 1.0 is a headless server system that installs
itself and is run from the shell or from a browser:

- **Spark**, the init and service manager, which supervises services and
  restarts them when they fail;
- **Nukesh**, the shell, which is also the POSIX `/bin/sh` and runs
  bash's scripts as `/bin/bash`, with the commands an administrator
  expects on a Linux server;
- **Btrfs** as the native file system, with snapshots, mirroring across
  disks, and atomic system updates that can be rolled back;
- wired networking with DHCP and DNS, WireGuard, a firewall, users,
  logging and SSH;
- bundles, the signed packages that apps and services come in;
- **Dukovo Admin**, a web console that runs the whole machine from any
  browser;
- an installer with the `rdisk` partitioner, delivered as a USB image,
  and images for clouds and virtual machines.

The **Lumen** compositor, and the graphical side that builds on it, follow
in Dukovo 1.1.

## Where the project is

Dukovo is built in phases, each ending in something that runs. This
handbook describes **Phase 9**.

- **Boot and hardware.** The Radix kernel boots through GRUB under UEFI
  and legacy BIOS at a random address, starts every CPU and reads the
  firmware's ACPI tables.
- **Memory and protection.** It manages memory with a frame allocator per
  NUMA node and a kernel heap, and protects itself with W^X page tables,
  guard pages, SMEP and SMAP.
- **Capabilities.** Programs reach every resource through unforgeable
  capabilities (see `radix-capabilities(7)`).
- **Scheduling.** Threads are shared fairly or run first by real-time
  priority, grouped into resource domains (see `radix-scheduling(7)`).
- **IPC.** Programs talk over endpoints, notifications, shared-memory
  channels and rings (see `radix-ipc(7)`).
- **Processes.** Each program runs in a process of its own. Spark starts
  them from a boot archive (see `radix-processes(7)` and `spark(8)`).
- **Files.** Files are reached through directory capabilities: tmpfs,
  FAT on virtual disks, `/proc` and `/sys` (see `radix-files(7)` and the
  Files chapter).
- **POSIX.** Programs written for Unix run on a POSIX layer with fork,
  exec, signals, pipes, terminals and job control (see `radix-posix(7)`),
  through the mlibc C library and a port of Rust's standard library.
- **Shell and tools.** It boots to the Nukesh shell on six virtual
  consoles, with the commands of a Linux server and a manual page for
  each (see the Nukesh guide chapter, `nukesh(1)` and `consoled(8)`).
  Spark supervises the services it starts (see `service(8)`).
- **Debugging.** Crash reports, a kernel debugger and a crash log help
  when things go wrong.

Native storage, Btrfs on NVMe and SATA disks, arrives in Phase 10.
Chapters for later phases are listed in the table of contents and are
written as those phases land. The full plan is in
[`docs/roadmap.md`](https://github.com/openlinuxsystems/orthros/blob/main/docs/roadmap.md).

## How this handbook is organised

- **Getting started** covers building Dukovo, making install media and
  installing it.
- **Using Dukovo** covers the system itself, from the shell to updates.
- **Reference** lists supported hardware, explains how to debug the
  kernel, and lists the manual pages. Every
  command, file format and interface has a man page; this handbook explains
  how the pieces fit together.
