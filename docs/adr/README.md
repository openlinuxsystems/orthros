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
| [0021](0021-license-manifest-in-every-image.md) | Every image carries a checked license manifest | Accepted; changed by [0114](0114-plain-names-inside-system.md) |
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
| [0050](0050-spark-is-started-by-the-kernel-from-the-boot-archive.md) | The kernel starts one program, Spark, from the boot archive, gives it the archive and a pool of memory, and stops if it ends | Accepted; changed by [0114](0114-plain-names-inside-system.md) |
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
| [0071](0071-dynamic-linker-for-bundles.md) | mlibc's dynamic linker is /system/lib/ld.so, for bundles; system programs stay static | Accepted; changed by [0114](0114-plain-names-inside-system.md) |
| [0072](0072-exact-numbers-and-times-in-the-c-library.md) | The C library writes and reads numbers and times as glibc does, with musl's code where mlibc's was wrong | Accepted |
| [0073](0073-c-utf-8-locale.md) | Programs start in C.UTF-8, compiled from glibc's locale data; named locales wait for Phase 12 | Accepted; changed by [0114](0114-plain-names-inside-system.md) |
| [0074](0074-posix-scheduling-and-resource-limits.md) | Nice levels, POSIX scheduling and resource limits follow Linux's rules for a process without privileges | Accepted |
| [0075](0075-advisory-file-locks.md) | Advisory file locks are kept by the kernel in one table: BSD's, POSIX's and Linux's open file description locks | Accepted |
| [0076](0076-file-times-and-device-numbers.md) | Programs set a file's times; reading never changes them, and each mount has a device number | Accepted |
| [0077](0077-one-cache-emptied-only-by-what-it-holds.md) | One per-CPU cache finds a capability and its object, and empties only when something it holds goes | Accepted |
| [0078](0078-server-first-release.md) | Dukovo 1.0 is a server release, and Lumen moves to 1.1 | Accepted |
| [0079](0079-framework-desktop-second-reference-machine.md) | The Framework Desktop is the second reference machine | Accepted |
| [0080](0080-crates-patched-for-radix.md) | Programs in Rust from elsewhere build with crates.io crates patched for Radix | Accepted |
| [0081](0081-symbolic-links.md) | Symbolic links are resolved by the kernel, beneath the directory a path starts from | Accepted; changed by [0113](0113-plain-names-for-the-root-folders.md) and [0114](0114-plain-names-inside-system.md) |
| [0082](0082-copies-under-the-page-cache-never-wait-for-a-page.md) | Copies under a file's page cache lock never wait for a page | Accepted |
| [0083](0083-nukesh-the-shell.md) | Nukesh is the shell and /bin/sh, written in Rust and checked against bash | Accepted; changed by [0103](0103-the-shell-corpus.md) |
| [0084](0084-dev-fd-in-the-c-library.md) | The C library gives /dev/fd, /dev/stdin, /dev/stdout and /dev/stderr | Accepted; changed by [0113](0113-plain-names-for-the-root-folders.md) |
| [0085](0085-socket-pairs.md) | Socket pairs are two pipes, one each way | Accepted |
| [0086](0086-release-archives-and-autoconf-ports.md) | Ports come from release archives or git, and autoconf packages build through one wrapper | Accepted |
| [0087](0087-gpl-programs-among-the-ports.md) | GPL programs may be ports in /system, each a program of its own | Accepted |
| [0088](0088-archives-and-compressors.md) | libarchive is tar, cpio and unzip; each compressor is its own project's | Accepted |
| [0089](0089-directory-descriptor-paths.md) | The C library notes the path of each directory descriptor, for `..` beneath it and fchdir | Accepted |
| [0090](0090-editors-cxx-and-default-settings.md) | Editors, a C++ runtime, and the settings the system starts with | Accepted; changed by [0114](0114-plain-names-inside-system.md) |
| [0091](0091-named-sockets-and-passed-descriptors.md) | Named sockets connect through the tree, and descriptors ride on their bytes | Accepted |
| [0092](0092-cpu-time-split-and-load-averages.md) | CPU time is split by the timer's samples, and the load averages are Linux's | Accepted |
| [0093](0093-cpu-affinity-and-io-classes.md) | Threads have Linux's CPU affinity, and processes Linux's I/O classes | Accepted |
| [0094](0094-proc-for-the-native-tools-kmsg-and-opened-paths.md) | /proc tells the native tools what they show, /dev/kmsg gives the log, and files keep the path they were opened by | Accepted; changed by [0113](0113-plain-names-for-the-root-folders.md) |
| [0095](0095-tracing-through-a-pipe-the-kernel-writes.md) | A trace is a pipe the kernel writes a record to for each call | Accepted |
| [0096](0096-native-administration-tools-as-one-program.md) | The native administration tools are one Rust program, written for Radix with Linux's names and options | Accepted |
| [0097](0097-keyboard-events-and-the-framebuffer-for-programs.md) | The kernel hands programs key events through /dev/kbd and the screen through /dev/fb0 | Accepted |
| [0098](0098-resource-domains-from-user-space.md) | Spark makes the services' resource domains through capabilities, and restarts them itself | Accepted |
| [0099](0099-power-capability-and-ordered-shutdown.md) | Turning the machine off is a capability only Spark holds, and Spark stops the services first | Accepted |
| [0100](0100-consoled-virtual-consoles-in-a-program.md) | consoled draws the virtual consoles as a program, a terminal for TERM=linux built on the vte crate | Accepted |
| [0101](0101-spark-supervises-services.md) | Spark supervises services from simple key-value files, in a domain each, and answers a control socket | Accepted |
| [0102](0102-manual-pages-and-completions-in-the-image.md) | Every program in /system/bin has its manual page, indexed, and the shell completes its options from that page | Accepted; changed by [0114](0114-plain-names-inside-system.md) |
| [0103](0103-the-shell-corpus.md) | Nukesh is checked against POSIX's shell tests and real projects' scripts, run in a sandbox | Accepted |
| [0104](0104-cancellation-points-as-musl-makes-them.md) | Thread cancellation points are system calls made as musl makes them, through a stub the signal handler can see | Accepted |
| [0105](0105-shootdowns-skip-stopped-virtual-cpus.md) | Under KVM, a TLB shootdown sends no interrupt to a virtual CPU the host has stopped | Accepted |
| [0106](0106-posix-benchmarks-one-c-program.md) | The POSIX rows of `ob bench` are one C program, run on Radix and on Linux | Accepted |
| [0107](0107-the-gate-is-parity-and-measures-again.md) | The performance gate is parity with Linux, and an operation over it is measured again on both systems | Accepted |
| [0108](0108-the-registry-under-the-store-lock.md) | The object registry is guarded by the capability store's lock | Accepted |
| [0109](0109-a-doorbell-for-late-shootdown-answers.md) | Under KVM, a CPU that answers a TLB shootdown late gets a faster timer while it runs | Accepted |
| [0110](0110-shared-mappings-keep-their-pages.md) | A shared mapping keeps its pages in one place, which also names the futex words in it | Accepted |
| [0111](0111-one-queue-for-every-disk.md) | Every disk's requests go through one queue, and block devices are read and written raw | Accepted |
| [0112](0112-ahci-from-the-specification.md) | SATA disks through AHCI, written from Intel's specification, in a crate without unsafe code | Accepted |
| [0113](0113-plain-names-for-the-root-folders.md) | The root's folders have plain names, and Unix's names are hidden links to them | Accepted |
| [0114](0114-plain-names-inside-system.md) | The folders inside /system have plain names, and Unix's names are hidden links to them | Accepted |
| [0115](0115-nvme-from-the-specification.md) | NVMe disks, written from the specification, with a queue for each CPU | Accepted |
| [0116](0116-idle-polling-learns-from-other-cpus.md) | Idle polling learns only from wake-ups that other CPUs send | Accepted |
| [0117](0117-nvme-shadow-doorbells.md) | NVMe shadow doorbells, and virtio requests of 254 pages | Accepted; changed by [0136](0136-nvme-completion-heads-go-to-the-register-at-the-next-completion.md) |
| [0118](0118-frames-in-order-for-memory-written-in-order.md) | Memory written in order gets frames in order, and virtio merges buffers that follow each other | Accepted |
| [0119](0119-nvme-scatter-gather-lists-for-long-runs.md) | NVMe data in long runs of memory go as scatter gather lists | Accepted |
| [0120](0120-extended-state-matches-the-host.md) | Extended state enables what the host's does, with AMX held off | Accepted |
| [0121](0121-large-pages-where-frames-line-up.md) | A frame's pages get 2 MiB pages where they line up | Accepted |
| [0122](0122-disk-checks-on-real-machines.md) | Real machines' disks are checked by serial number, and Linux checks what Dukovo wrote | Accepted |
| [0123](0123-disks-of-4k-sectors.md) | Disks of 4 KiB sectors keep 512-byte addresses and move whole sectors | Accepted |
| [0124](0124-the-disk-check-measures-the-benchmarks.md) | The disk check also measures the benchmarks on the machine | Accepted |
| [0125](0125-btrfs-blocks-are-checked-before-use.md) | Btrfs is read by checking every block before use | Accepted |
| [0126](0126-no-age-or-identity-verification.md) | Dukovo never verifies anyone's age or identity | Accepted |
| [0127](0127-btrfs-files-are-read-through-the-page-cache.md) | Btrfs files are read through the page cache and checked as Linux checks them | Accepted |
| [0128](0128-btrfs-compressed-extents-have-their-own-decoders.md) | Btrfs's compressed extents are read by decoders of its own, written from the formats' specifications | Accepted |
| [0129](0129-btrfs-commits-copy-on-write-and-flush-before-the-superblock.md) | Btrfs writes by copying on write, and a commit flushes its blocks before its superblock | Accepted |
| [0130](0130-btrfs-files-change-under-one-transaction-lock-and-data-is-copied-at-write-back.md) | Btrfs files change under one transaction lock, and their data is copied on write at write-back | Accepted |
| [0131](0131-btrfs-makes-chunks-as-it-fills-and-reserves-room-before-a-change.md) | Btrfs makes chunks as it fills, and reserves room before a change | Accepted |
| [0132](0132-btrfs-reads-ahead-in-runs-and-is-benched-against-linuxs-btrfs.md) | Btrfs reads ahead in runs, and is benched against Linux's Btrfs | Accepted |
| [0133](0133-btrfs-uncompresses-a-runs-extents-on-two-cpus-through-the-kernels-join.md) | Btrfs uncompresses a run's extents on two CPUs, through the kernel's `join` | Accepted |
| [0134](0134-btrfs-compresses-writes-with-zstd-level-1-on-the-disks-that-ask-for-it.md) | Btrfs compresses writes with zstd level 1 on the disks that ask for it | Accepted |
| [0135](0135-hard-links-named-pipes-device-nodes-and-a-change-time.md) | Hard links, named pipes, device nodes, socket names on disk, and a change time | Accepted |
| [0136](0136-nvme-completion-heads-go-to-the-register-at-the-next-completion.md) | An NVMe completion head the controller asks for goes to its register at the next completion | Accepted |
| [0137](0137-the-root-is-the-dukovo-root-partitions-btrfs-subvolumes.md) | The root is the Btrfs subvolumes of a Dukovo root partition | Accepted |
| [0138](0138-btrfs-snapshots-and-subvolumes-with-linuxs-back-references.md) | Btrfs snapshots and subvolumes, with Linux's back references | Accepted |
| [0139](0139-btrfs-clones-files-extents-as-linuxs-ficlone-does.md) | Btrfs clones files' extents as Linux's `FICLONE` does | Accepted |
| [0140](0140-exfat-from-the-specification.md) | exFAT is our own implementation from Microsoft's specification, and ships though its patents run to 2027 | Accepted |
| [0141](0141-ext4-from-its-documentation-with-a-journal-of-whole-blocks.md) | ext4 is our own implementation from its documentation, journaling whole blocks at each sync | Accepted |
| [0142](0142-the-kernel-hears-the-fixed-acpi-power-button-and-spark-powers-off.md) | The kernel hears ACPI's fixed power button, and pressing it asks Spark to power off | Accepted |
| [0143](0143-spark-pets-the-chipsets-hardware-watchdog.md) | Spark pets the chipset's hardware watchdog, which restarts a machine whose Spark stops responding | Accepted |
| [0144](0144-lspci-lscpu-and-lsmem-with-the-pci-id-database.md) | lspci, lscpu and lsmem are systools, with the PCI ID database as a port | Accepted |
| [0145](0145-machine-checks-are-reported-and-the-unrecoverable-ones-panic.md) | Machine checks are reported, and the ones the CPU cannot go on after panic | Accepted |
| [0146](0146-xhci-controllers-are-driven-by-a-kernel-thread-each.md) | xHCI controllers are driven by a kernel thread each, from Intel's specification | Accepted |
| [0147](0147-usb-hubs-and-every-device-configured-by-its-controllers-thread.md) | USB hubs are driven by their controller's thread, which configures every device | Accepted |
| [0148](0148-usb-keyboards-and-mice-are-read-through-their-hid-reports.md) | USB keyboards and mice are read through their HID reports, and their keys repeat in software | Accepted |
| [0149](0149-the-consoles-type-in-layouts-made-from-xkeyboard-configs.md) | The consoles type in keyboard layouts made from xkeyboard-config's | Accepted |
| [0150](0150-btrfs-scrubs-the-last-commit-a-run-of-extents-at-a-time.md) | Btrfs scrubs the last commit, a run of extents at a time | Accepted |
| [0151](0151-btrfs-replays-the-log-of-fsyncs-linux-leaves-at-mount.md) | Btrfs replays the log of fsyncs Linux leaves, at mount | Accepted |
| [0152](0152-swap-to-linux-swap-areas-from-a-thread-of-its-own.md) | Swap to Linux's swap areas, from a thread of its own | Accepted |
| [0153](0153-btrfs-on-several-disks-mounts-with-one-missing-and-warns.md) | Btrfs on several disks mounts with one missing, and warns | Accepted |
| [0154](0154-btrfs-writes-a-log-of-fsyncs-as-linux-does.md) | Btrfs writes a log of fsyncs, as Linux does | Accepted |
| [0155](0155-btrfs-replaces-a-disk-by-rebuilding-onto-it-with-a-scrub.md) | Btrfs replaces a disk by rebuilding onto it with a scrub | Accepted |
| [0156](0156-a-disk-requests-thread-is-woken-from-its-slot.md) | A disk request's thread is woken from its slot, and a lending takes nothing from the heap | Accepted |
| [0157](0157-a-disks-health-is-read-through-a-request-of-its-own.md) | A disk's health is read through a request of its own | Accepted |
| [0158](0158-btrfs-is-trimmed-under-the-transaction-a-batch-at-a-time.md) | Btrfs is trimmed under the transaction, a batch at a time | Accepted |
| [0159](0159-btrfs-balances-a-chunk-at-a-time-by-copying-paths.md) | Btrfs balances a chunk at a time, copying the paths to what it moves | Accepted |
| [0160](0160-the-console-over-the-xhci-debug-capability.md) | The console goes over the xHCI Debug Capability to a second computer, with the `dbc` flag | Accepted |
| [0161](0161-usb-sticks-are-disks-over-the-bulk-only-transport.md) | USB sticks are disks over the Bulk-Only Transport, served by a thread each | Accepted |
| [0162](0162-usb-attached-scsi-queues-commands-on-usb-3-disks.md) | USB Attached SCSI queues commands on USB 3 disks | Accepted |
| [0163](0163-volumed-mounts-volumes-plugged-in-after-boot-by-label.md) | volumed mounts the volumes plugged in after boot by their labels, and eject unmounts one | Accepted |
| [0164](0164-each-device-reaches-only-its-own-memory-behind-vt-d.md) | Each device reaches only its own memory behind Intel VT-d | Accepted |
| [0165](0165-each-device-reaches-only-its-own-memory-behind-amd-vi.md) | Each device reaches only its own memory behind AMD-Vi | Accepted |
| [0166](0166-each-device-raises-only-its-own-interrupts.md) | Each device raises only its own interrupts | Accepted |
| [0167](0167-newer-intel-chipsets-and-amds-fch-have-their-watchdog-too.md) | Newer Intel chipsets and AMD's FCH have their watchdog too | Accepted |
| [0168](0168-bad-memory-ends-only-the-program-that-read-it.md) | Bad memory ends only the program that read it, and its page leaves use | Accepted |
| [0169](0169-corrected-errors-interrupt-and-memory-errors-are-counted.md) | Corrected errors interrupt where the CPU can, and memory errors are counted | Accepted |
| [0170](0170-btrfs-keeps-simple-quotas.md) | Btrfs keeps simple quotas, each extent counted to the subvolume that wrote it | Accepted |
| [0180](0180-the-tcp-ip-stack-runs-in-the-kernel-on-smoltcp.md) | The TCP/IP stack runs in the kernel, on smoltcp | Accepted |
| [0181](0181-virtio-net-copies-frames-through-slots-of-its-own.md) | The virtio network card copies frames through slots of its own | Accepted |
| [0182](0182-the-kernel-patches-smoltcp-so-socket-memory-comes-back.md) | The kernel patches smoltcp, so that a socket's memory comes back | Accepted |
| [0183](0183-programs-reach-the-network-through-capabilities-of-its-ports.md) | Programs reach the network through capabilities of its ports | Accepted |
| [0184](0184-services-and-the-c-library-use-the-network-capabilities-they-were-given.md) | Services and the C library use the network capabilities they were given | Accepted |
| [0185](0185-loopback-runs-in-the-senders-thread-and-the-network-joins-the-gate.md) | Loopback runs in the sender's thread, and the network joins the gate | Accepted |
| [0186](0186-a-loopback-connection-passes-its-bytes-straight-across.md) | A loopback connection passes its bytes straight across | Accepted |
| [0187](0187-tcp-keeps-quiet-connections-and-controls-congestion-as-linux-does.md) | TCP keeps quiet connections, and controls congestion, as Linux does | Accepted |
| [0188](0188-tcp-recovers-from-losses-with-sack-and-an-interface-holds-thousands-of-sockets.md) | TCP recovers from losses with SACK, and an interface holds thousands of sockets | Accepted |
| [0200](0200-uacpi-runs-the-firmwares-aml-in-the-kernel.md) | uACPI runs the firmware's AML in the kernel | Accepted |
| [0201](0201-batteries-adapters-and-lids-through-uacpi.md) | Batteries, AC adapters and lids through uACPI, and the embedded controller | Accepted |
| [0202](0202-cpu-frequency-through-hwp-and-cppc.md) | CPU frequency through Intel's HWP and AMD's CPPC | Accepted |
| [0203](0203-thermal-zones-through-uacpi.md) | Thermal zones through uACPI | Accepted |
| [0204](0204-zens-scalable-machine-check-banks.md) | Zen's scalable machine-check banks | Accepted |
| [0205](0205-the-cpus-temperature-sensors.md) | The CPUs' temperature sensors | Accepted |
| [0206](0206-the-chipsets-smbus-controller.md) | The chipset's SMBus controller | Accepted |
| [0207](0207-framework-embedded-controller.md) | Framework's embedded controller | Accepted |
| [0208](0208-programs-set-the-fans-leds-and-argb-header-through-the-platform-capability.md) | Programs set the fans, LEDs and ARGB header through the platform capability | Accepted |
| [0210](0210-logd-keeps-the-kernel-log-as-the-system-log.md) | logd keeps the kernel log as the system's log, and the kernel says which process wrote each record | Accepted |
| [0211](0211-the-audit-log-and-trimming-the-log.md) | Security events of the kernel and Spark go to an audit log, and logd trims the log by age and size | Accepted |
| [0212](0212-notifications-and-the-privacy-promise.md) | notifyd passes notifications to the console's bottom line, naming the sender as the kernel does, and the privacy promise | Accepted |
| [0213](0213-files-have-owners-groups-and-modes.md) | Files have owners, groups and modes, which Btrfs, ext4 and tmpfs keep | Accepted |
| [0214](0214-names-and-the-second-gate.md) | Processes and capabilities act for names, which the names capability gives, and owners and modes narrow what a capability reaches | Accepted |
| [0215](0215-views-directories-made-of-attached-capabilities.md) | Views: directories made of attached capabilities, which a walk crosses onto as onto mounts | Accepted |
| [0216](0216-service-files-in-toml.md) | Service files are TOML, read by one crate, and a file that does not read never takes a service down | Accepted |
| [0230](0230-sensors-and-lsusb-with-the-usb-id-database.md) | sensors and lsusb are systools, with the USB ID database as a port | Accepted |
| [0231](0231-the-firmwares-smbios-tables-and-hwinfo.md) | The firmware's SMBIOS tables show what the machine is, and hwinfo shows them | Accepted |
