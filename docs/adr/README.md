<!--
SPDX-FileCopyrightText: 2026 The Dukovo contributors
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
| [0002](0002-license-apache-2.0-or-mit.md) | License Dukovo under Apache-2.0 OR MIT | Accepted |
| [0003](0003-github-and-actions.md) | Host on GitHub and run CI on GitHub Actions | Accepted |
| [0004](0004-btrfs-native-filesystem.md) | Btrfs is the native file system | Accepted |
| [0005](0005-static-linking.md) | Statically link the programs in /system/bin | Accepted |
| [0006](0006-default-image-includes-lumen.md) | The default image is the full 4 GiB image with Lumen | Superseded by [0078](0078-server-first-release.md) |
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
| [0043](0043-system-call-fuzzing-and-documented-errors.md) | System calls are fuzzed from user mode on every push, and every error a call returns must be in its documentation | Accepted |
| [0044](0044-objects-found-without-a-shared-lock.md) | Objects are found through a per-CPU cache of weak references, and endpoint calls on both CPUs at once are held to Linux | Accepted |
| [0045](0045-instruction-budgets-in-ci.md) | CI holds every operation on one CPU to an instruction budget, counted by QEMU | Accepted |
| [0046](0046-performance-gate-on-the-fastest-of-three-runs.md) | The performance gate measures Radix and Linux three times in turn and counts each operation's fastest run | Accepted |
| [0047](0047-processes.md) | A process is a capability-named object that its maker builds, that ends with its threads or its maker, and that is reaped by deleting its last capability | Accepted |
| [0048](0048-static-pie-programs-and-user-aslr.md) | Programs are static position-independent executables, placed at random addresses by whoever loads them | Accepted |
| [0049](0049-boot-archive.md) | The programs a machine starts with come in a ustar boot archive that GRUB loads as a module, checked whole before use | Accepted |
| [0050](0050-spark-is-started-by-the-kernel-from-the-boot-archive.md) | The kernel starts one program, Spark, from the boot archive, gives it the archive and a pool of memory, and stops if it ends | Accepted |
| [0051](0051-radix-rt-runtime.md) | radix-rt is the Rust runtime of native programs: a heap, arguments, the boot archive and spawning, on top of libradix | Accepted |
| [0052](0052-message-signalled-interrupts-only.md) | Device interrupts are message-signalled only, and PCI is reached through ECAM | Accepted |
| [0053](0053-modern-virtio-only.md) | Virtio devices are driven through the modern PCI transport only, with split virtqueues | Accepted |
| [0054](0054-files-through-directory-capabilities.md) | Files are reached through directory capabilities, with every path resolved beneath one | Accepted |
| [0055](0055-page-cache-and-writeback.md) | File data lives in per-file page caches, metadata in a block cache per device, written back every five seconds | Accepted |
| [0056](0056-own-fat-implementation.md) | FAT is our own implementation from Microsoft's specification, checked against dosfstools and mtools | Accepted |
| [0057](0057-hierarchy-proc-and-sys.md) | Spark builds the directory hierarchy; /proc and /sys are plain text, with Linux's layout where both have a thing | Accepted |
| [0058](0058-kernel-stack-frame-budget.md) | Every kernel function's stack frame is held to 2 KiB, checked after every release build | Accepted |
| [0059](0059-file-operations-fast-paths.md) | A path with no `..` is walked without a chain, each mount filters its mount points, and a file method looks its capability up once | Accepted |
| [0060](0060-anonymous-memory-in-the-kernel.md) | Anonymous memory is the kernel's: made when touched, charged to the domain, and shared copy-on-write after a fork | Accepted |
| [0061](0061-posix-process-tree-in-the-kernel.md) | The kernel keeps POSIX's process tree, groups and sessions, beside the capabilities | Accepted |
| [0062](0062-signals-in-the-kernel.md) | The kernel delivers signals on the way back to user mode, with Linux's numbers and frame | Accepted |
| [0063](0063-fork-and-exec.md) | fork copies what may be copied, and exec loads a program from a file into a fresh address space | Accepted |
| [0064](0064-file-descriptors-and-paths-for-posix.md) | A file descriptor is a capability slot, and the C library turns paths into paths beneath the root it holds | Accepted |
| [0065](0065-pipes-and-poll.md) | Pipes are kernel file descriptions, and poll is one call over capabilities | Accepted |
| [0066](0066-terminals-and-pseudo-terminals.md) | Terminals are kernel pseudo-terminals with POSIX's line discipline and job control | Accepted |
| [0067](0067-clocks-and-timers.md) | Clocks are read through the process, and one kernel thread runs every timer | Accepted |
| [0068](0068-mlibc-is-the-c-library.md) | mlibc is the C library, with Radix's own system layer and Linux's ABI numbers | Accepted |
| [0069](0069-clang-toolchain-and-ports.md) | Software from elsewhere is built by ports: pinned sources, patches and a script, with the host's Clang wrapped for Radix | Accepted |
| [0070](0070-rust-standard-library-port.md) | Rust's standard library is ported to a radix target on mlibc, built from the toolchain's rust-src | Accepted |
| [0071](0071-dynamic-linker-for-bundles.md) | mlibc's dynamic linker is /system/lib/ld.so, for bundles; system programs stay static | Accepted |
| [0072](0072-exact-numbers-and-times-in-the-c-library.md) | The C library writes and reads numbers and times as glibc does, with musl's code where mlibc's was wrong | Accepted |
| [0073](0073-c-utf-8-locale.md) | Programs start in C.UTF-8, compiled from glibc's locale data; named locales wait for Phase 12 | Accepted |
| [0074](0074-posix-scheduling-and-resource-limits.md) | Nice levels, POSIX scheduling and resource limits follow Linux's rules for a process without privileges | Accepted |
| [0075](0075-advisory-file-locks.md) | Advisory file locks are kept by the kernel in one table: BSD's, POSIX's and Linux's open file description locks | Accepted |
| [0076](0076-file-times-and-device-numbers.md) | Programs set a file's times; reading never changes them, and each mount has a device number | Accepted |
| [0077](0077-one-cache-emptied-only-by-what-it-holds.md) | One per-CPU cache finds a capability and its object, and empties only when something it holds goes | Accepted |
| [0078](0078-server-first-release.md) | Dukovo 1.0 is a server release, and Lumen moves to 1.1 | Accepted |
| [0079](0079-framework-desktop-second-reference-machine.md) | The Framework Desktop is the second reference machine | Accepted |
| [0080](0080-crates-patched-for-radix.md) | Programs in Rust from elsewhere build with crates.io crates patched for Radix | Accepted |
| [0081](0081-symbolic-links.md) | Symbolic links are resolved by the kernel, beneath the directory a path starts from | Accepted |
| [0082](0082-copies-under-the-page-cache-never-wait-for-a-page.md) | Copies under a file's page cache lock never wait for a page | Accepted |
| [0083](0083-nukesh-the-shell.md) | Nukesh is the shell and /bin/sh, written in Rust and checked against bash | Accepted; changed by [0103](0103-the-shell-corpus.md) |
| [0084](0084-dev-fd-in-the-c-library.md) | The C library gives /dev/fd, /dev/stdin, /dev/stdout and /dev/stderr | Accepted |
| [0085](0085-socket-pairs.md) | Socket pairs are two pipes, one each way | Accepted |
| [0086](0086-release-archives-and-autoconf-ports.md) | Ports come from release archives or git, and autoconf packages build through one wrapper | Accepted |
| [0087](0087-gpl-programs-among-the-ports.md) | GPL programs may be ports in /system, each a program of its own | Accepted |
| [0088](0088-archives-and-compressors.md) | libarchive is tar, cpio and unzip; each compressor is its own project's | Accepted |
| [0089](0089-directory-descriptor-paths.md) | The C library notes the path of each directory descriptor, for `..` beneath it and fchdir | Accepted |
| [0090](0090-editors-cxx-and-default-settings.md) | Editors, a C++ runtime, and the settings the system starts with | Accepted |
| [0091](0091-named-sockets-and-passed-descriptors.md) | Named sockets connect through the tree, and descriptors ride on their bytes | Accepted |
| [0092](0092-cpu-time-split-and-load-averages.md) | CPU time is split by the timer's samples, and the load averages are Linux's | Accepted |
| [0093](0093-cpu-affinity-and-io-classes.md) | Threads have Linux's CPU affinity, and processes Linux's I/O classes | Accepted |
| [0094](0094-proc-for-the-native-tools-kmsg-and-opened-paths.md) | /proc tells the native tools what they show, /dev/kmsg gives the log, and files keep the path they were opened by | Accepted |
| [0095](0095-tracing-through-a-pipe-the-kernel-writes.md) | A trace is a pipe the kernel writes a record to for each call | Accepted |
| [0096](0096-native-administration-tools-as-one-program.md) | The native administration tools are one Rust program, written for Radix with Linux's names and options | Accepted |
| [0097](0097-keyboard-events-and-the-framebuffer-for-programs.md) | The kernel hands programs key events through /dev/kbd and the screen through /dev/fb0 | Accepted |
| [0098](0098-resource-domains-from-user-space.md) | Spark makes the services' resource domains through capabilities, and restarts them itself | Accepted |
| [0099](0099-power-capability-and-ordered-shutdown.md) | Turning the machine off is a capability only Spark holds, and Spark stops the services first | Accepted |
| [0100](0100-consoled-virtual-consoles-in-a-program.md) | consoled draws the virtual consoles as a program, a terminal for TERM=linux built on the vte crate | Accepted |
| [0101](0101-spark-supervises-services.md) | Spark supervises services from simple key-value files, in a domain each, and answers a control socket | Accepted |
| [0102](0102-manual-pages-and-completions-in-the-image.md) | Every program in /system/bin has its manual page, indexed, and the shell completes its options from that page | Accepted |
| [0103](0103-the-shell-corpus.md) | Nukesh is checked against POSIX's shell tests and real projects' scripts, run in a sandbox | Accepted |
| [0104](0104-cancellation-points-as-musl-makes-them.md) | Thread cancellation points are system calls made as musl makes them, through a stub the signal handler can see | Accepted |
| [0105](0105-shootdowns-skip-stopped-virtual-cpus.md) | Under KVM, a TLB shootdown sends no interrupt to a virtual CPU the host has stopped | Accepted |
| [0106](0106-posix-benchmarks-one-c-program.md) | The POSIX rows of `ob bench` are one C program, run on Radix and on Linux | Accepted |
