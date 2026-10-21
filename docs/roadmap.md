<!--
SPDX-FileCopyrightText: 2026 The OrthrOS contributors
SPDX-License-Identifier: Apache-2.0 OR MIT
-->

# OrthrOS roadmap: Radix implementation plan

This is the implementation plan for OrthrOS, from the Radix kernel
specification (v1.4) to Release 1.0 and beyond, as agreed on 2026-10-20.
When a phase's scope changes, update this file in the same pull request.
Decisions it refers to are recorded in [`docs/adr/`](adr/README.md).

## Summary

OrthrOS 1.0 is an installable x86-64 operating system on the Radix kernel. It boots from a USB stick, partitions and formats a disk with its own tools, installs itself, and boots into Nukesh with networking, bundles, atomic updates and the Lumen compositor. The plan reaches 1.0 in 17 phases (0 to 16), each ending in something you can boot and test in QEMU, then continues on a post-1.0 track to full spec conformance.

Sizing, stated plainly: projects of this kind (Redox, SerenityOS, Managarm) took small teams several years to reach a comparable point. Phases therefore carry relative sizes (S, M, L, XL), not dates.

### Defaults chosen where the ask left room

| Decision | Choice | Reason |
| --- | --- | --- |
| First architecture | x86-64, UEFI and legacy BIOS | One working system first; AArch64 is the first post-1.0 phase, and CI compiles the arch layer for it from Phase 1 |
| Native filesystem | Btrfs, implemented from the on-disk format in Rust | Spec 7 first recommendation; Linux btrfs-progs becomes a free test oracle |
| Bootloader | GRUB 2 loading Radix through Multiboot2 | Spec 12.3; one entry path covers UEFI and BIOS |
| Kernel language | Rust no\_std, pinned nightly toolchain | Spec 12.1 |
| C compatibility | mlibc with a Radix sysdeps layer | Designed for porting to new kernels, MIT licensed, already runs GCC, coreutils and Wayland ports on other hobby kernels |
| Build host | Linux x86-64, or any OS through the provided Podman/Docker build container | Image creation needs no root |
| Reference hardware | QEMU/KVM (q35 + OVMF), then the reference Dell Inspiron 15 5570 laptop (Core i7-8550U) | Chosen by the project owner; see the Reference machine section |
| Networking in 1.0 | Wired Ethernet only | Wi-Fi drivers and WPA supplicant are their own large effort; the Dell has a built-in 10/100 Ethernet port |
| Graphics in 1.0 | UEFI GOP framebuffer and virtio-gpu with software rendering | Native Intel/AMD GPU drivers are post-1.0 |
| License | Apache-2.0 OR MIT for all OrthrOS code; ported tools keep their own licenses | Matches the Rust crates we reuse; GPL tools ship as separate programs |

### Scope of 1.0

- **In:** Radix kernel, Spark init, Nukesh, core utilities, bundle runtime, users and login, logging, IPv4/IPv6 networking with DHCP and DNS, Btrfs, FAT and exFAT, swap, USB storage and input, atomic base-system updates with rollback, snapshots, the installer with the `rdisk` partitioner, USB install media, Lumen with a test terminal bundle, a man page for every shipped command and file format, and the `ob` build tool.
- **Out until after 1.0:** any desktop environment or shell UI (by request), Wi-Fi, Bluetooth, native GPU drivers, audio, printing, suspend, secure and measured boot, disk encryption, virtualization host, confidential computing, AArch64 and RISC-V.

## Spec review: gaps and decisions

The spec (v1.4) is buildable as written once eleven gaps are settled. Each row below states the decision this plan builds on; all are recorded as ADRs in `docs/adr/` during Phase 0 so they can be revisited.

| # | Spec reference | Gap or conflict | Decision in this plan |
| --- | --- | --- | --- |
| 1 | 1.2 vs 7.x | 1.2 says "kernel only", yet sections 7.1 to 7.16 specify a full userspace | Treat the spec as the OrthrOS system spec; Radix is the kernel inside it |
| 2 | 16.2 | Two steps are numbered 7, and Spark (step 7) must mount filesystems before the VFS exists (the other step 7) | Phase 6 builds a minimal VFS with a read-only boot archive before Spark; the full VFS follows in Phase 7 |
| 3 | 7.1 vs 7.6 | `/system` is immutable and replaced on update, but `/system/etc` and `/system/var` must persist and be writable | `/system` is a read-only Btrfs subvolume per update slot; `/system/etc` and `/system/var` are separate writable subvolumes mounted over it. Updates ship defaults in `/system/lib/defaults/etc` and add new files without overwriting edited ones |
| 4 | 2.1 vs 7.8 | Network stacks are to be compartmentalized, yet the kernel stack must remain the default path | The network stack runs in the kernel as a compartment: a `#![forbid(unsafe_code)]` crate with its own capability set, restartable on panic |
| 5 | 7.2 | `chmod` and `chown` must exist but there are no UID/GID permission bits | Files carry an owner principal and a small rights template as metadata. `chown` needs an admin capability; `chmod` edits the template that governs capabilities derived for other principals. Detailed in `radix-fs-rights(7)` |
| 6 | 7.2, 7.3 | Job control and supervision need POSIX-style signals, which the spec never names | Signals are a kernel notification object. Sending one requires a process capability with the `signal` right; mlibc maps POSIX signal APIs onto it |
| 7 | 4.1, 13 | Porting C tools needs `fork()`; capabilities make naive fork unsafe | `fork()` clones the address space copy-on-write and copies only capabilities marked inheritable. `posix_spawn` is the preferred, faster path |
| 8 | 9 vs 7.6 | Section 9 asks for live patching; 7.6 says it is not needed initially | Live patching is post-1.0. Updates reboot into the new slot |
| 9 | 8, 12.3 | Secure boot with GRUB needs a signed shim, which an independent project cannot get signed by Microsoft | Post-1.0: users enroll the project's own keys (sbctl or MOK). 1.0 boots with Secure Boot disabled |
| 10 | 7.8 | Wi-Fi is not mentioned anywhere | Wired Ethernet only in 1.0 |
| 11 | 7.16 | Notifications are "D-Bus-style" without naming an interface | A small `notifyd` service speaking a capability-based IPC protocol; Lumen and the console both subscribe |

## System architecture

Radix splits into a small trusted core and a ring of restartable compartments; everything above the syscall line is ordinary capability-restricted userspace.

Programs reach the kernel only through capability messages, and compartments reach devices only through capabilities that `lib/hw` hands out.

### Kernel: trusted core

The TCB is the only kernel code allowed to use `unsafe`, and every `unsafe` block carries a `// SAFETY:` argument checked by a CI lint.

- **arch/x86\_64 (HAL):** Multiboot2 entry, long-mode setup, GDT/IDT/TSS, local APIC and I/O APIC, x2APIC, HPET and invariant TSC, SMP bring-up, `SYSCALL`/`SYSRET` entry, PCID, SMEP/SMAP, early serial and framebuffer console. The HAL is a trait set (`PageTable`, `InterruptController`, `Timer`, `CpuLocal`) so AArch64 slots in later.
- **mm:** buddy frame allocator with DMA32 and normal zones and per-NUMA-node pools; 4-level page tables (5-level later); slab caches with per-CPU magazines; explicit allocation context (`Atomic` or `MaySleep`) on every allocator call; KASLR, guard pages, W^X; reclaim and swap-out under pressure.
- **cap:** per-process capability space (CSpace) of opaque slots; each capability is a type tag, rights mask and object reference with an optional badge. A derivation tree supports delegation and revocation.
- **sched:** per-CPU run queues, an EEVDF fair class (the modern successor to CFS), FIFO and round-robin real-time classes with priority inheritance, CPU affinity, and a deadline class post-1.0. RCU (quiescent-state based) for read-mostly kernel data.
- **ipc:** synchronous endpoints with a register-only fast path, asynchronous notifications, shared-memory channels, and `radix-ring` submission and completion queues (io\_uring-style).
- **lib/hw:** the single audited crate that touches MMIO, port I/O, DMA memory and interrupt lines. It hands these out as capability types, never as raw pointers.

### Kernel: compartments

Compartments are kernel crates compiled with `#![forbid(unsafe_code)]`. They reach hardware only through capabilities from `lib/hw`, run behind the IOMMU (Intel VT-d) once Phase 11 lands, and are restarted if they panic: the panic unwinds to the compartment boundary, its state is dropped and the device is re-probed.

- VFS, page cache and block layer
- Filesystems: Btrfs, FAT12/16/32, exFAT, ext4 (read-only), tmpfs, devfs, procfs, sysfs, bootfs
- Storage drivers: virtio-blk, AHCI (SATA), NVMe, USB mass storage (BOT, then UAS)
- Network stack and NIC drivers: virtio-net, Intel e1000/e1000e, Intel igc, Realtek r8169 family (including the Dell's Fast Ethernet chip), USB Ethernet adapters
- USB core: xHCI, hubs, HID keyboard and mouse
- TTY and PTY layer, input, and the display layer (GOP framebuffer, virtio-gpu)

The same driver crates also build as userspace driver processes against the `radix-driver` API. Release 1.0 runs the e1000 driver in userspace to meet the spec 15 requirement of isolating a non-trivial driver.

### System call and IPC interface

- About a dozen syscalls: `cap_call`, `send`, `recv`, `reply_recv`, `notify`, `wait`, `yield`, `map`, `unmap`, `ring_setup`, `ring_enter`, `debug_log`. Every other operation is a message on a capability, for example `open` is a message on a directory capability.
- Interfaces are written once in an interface description language (`abi/*.ridl`). The `ridlc` compiler generates Rust bindings for kernel and userspace, C headers for mlibc, and man(2) page skeletons, so code and documentation cannot drift.
- Each interface carries a version number and stable method IDs; old versions stay callable.
- Errors are structured (`domain`, `code`, `detail`) and map to `errno` in mlibc.

### Userspace stack

| Layer | Components | Origin and license |
| --- | --- | --- |
| Runtimes | `radix-rt` (no\_std Rust runtime for early programs), `libradix` (capability API), Rust std port (`x86_64-unknown-radix`), mlibc | New code; mlibc MIT |
| Init and services | Spark, logd, logtrim, netd (DHCPv4/v6 client and server, static config), resolved, timed, authd, consoled, volumed, inputd, notifyd | New, Rust |
| Shell | Nukesh, using reedline for line editing | New, Rust; reedline MIT |
| Core utilities | uutils coreutils, findutils and diffutils | Rust, MIT |
| Ported tools | GNU grep and sed, one-true-awk, less, libarchive (bsdtar, bsdunzip), gzip, bzip2, xz, Info-ZIP zip, curl, nano with ncurses, mandoc | GPL, BSD, MIT and ISC; shipped as separate programs |
| Radix-native tools | ps, top, kill, pgrep, pkill, who, w, uptime, free, vmstat, df, ip, ss, ping, clear, which, chmod, chown, bundle, user, snap, sysupdate, rdisk | New, Rust |
| Network libraries | smoltcp as the base of the in-kernel stack, hickory-dns for the resolver (DNS-over-TLS and HTTPS), rustls with the Mozilla CA bundle | 0BSD, MIT/Apache |
| Hardware discovery | Own `radix-bootinfo` and `radix-acpi` parsers for the Multiboot2 boot information and static ACPI tables (ADR 0013); the AML interpreter (rust-osdev `aml` crate, or uACPI in a compartment) is chosen when first needed | New code; MIT/Apache or MIT |
| Graphics | Lumen built on Smithay with the pixman software renderer, wayland-rs, libxkbcommon and xkeyboard-config | MIT |
| Boot | GRUB 2, built from pinned source | GPL-3.0 |

nano is an addition to the spec's utility list: a console-first system needs an editor for its configuration files.

## Repository, toolchain and the `ob` build tool

Everything lives in one repository and is built by one command, `./ob`, which needs no root access except for writing a USB stick.

### Repository layout

```
orthros/
├── ob                    # build entry point (shell wrapper around tools/xtask)
├── orthros.toml          # build configuration: arch, profile, features, image options
├── rust-toolchain.toml   # pinned nightly Rust
├── abi/                  # .ridl interface definitions (syscalls, IPC protocols)
├── kernel/
│   ├── arch/x86_64/      # HAL implementation (aarch64/ and riscv64/ later)
│   ├── mm/  cap/  sched/  ipc/
│   ├── fs/               # VFS, page cache, block layer, filesystems
│   ├── net/              # network stack compartment
│   ├── drivers/          # driver compartments
│   └── lib/              # shared kernel library, lib/hw
├── userspace/
│   ├── init/             # Spark
│   ├── nukesh/
│   ├── coreutils/        # Radix-native tools
│   ├── bundle-runtime/
│   ├── services/         # logd, netd, resolved, timed, authd, volumed, ...
│   ├── installer/        # orthros-install, rdisk, mkfs tools, rgrub-setup
│   ├── sysupdate/
│   └── lumen/
├── libs/                 # radix-rt, libradix, mlibc sysdeps, Rust std patches
├── ports/                # one recipe per third-party package
├── image/                # rootfs skeleton, GRUB configs, image definitions
├── docs/
│   ├── man/              # man pages, scdoc sources
│   ├── handbook/         # user and admin handbook (mdBook)
│   ├── design/           # subsystem design documents and safety arguments
│   └── adr/              # architecture decision records
├── tests/                # QEMU integration and install tests
└── tools/xtask/          # the Rust program behind ./ob
```

### Toolchain

- **Rust:** a pinned nightly in `rust-toolchain.toml`, with `-Zbuild-std` for the kernel target (`x86_64-unknown-none` style custom JSON) and for userspace (`x86_64-unknown-radix`). Patches to Rust's `std` and the `libc` crate live in `libs/rust-std/` until they can go upstream, as Redox did.
- **C ports:** Clang and LLD (the same LLVM that Rust uses) through wrapper scripts `x86_64-radix-cc` and `x86_64-radix-ld` that point at the mlibc sysroot. Ports get a small `config.sub` patch and a Meson cross file.
- **Ports system:** each `ports/<name>/recipe.toml` pins a source URL, SHA-256, patches and build steps, similar to Redox's cookbook. Sources are cached in `sources/` so a second build works offline.
- **Host tools built from pinned source:** GRUB 2, and OrthrOS's own `rdisk`, `mkfs-btrfs`, `mkfs-fat` and `rgrub-setup`, which are compiled for Linux too and used to assemble images. The image pipeline dogfoods the installer's tools.

### The `ob` command

| Command | What it does | Output |
| --- | --- | --- |
| `./ob setup` | Installs the pinned Rust toolchain and checks host packages | Ready build environment |
| `./ob doctor` | Lists anything missing, with the exact install command for Debian, Ubuntu, Fedora or Arch | Report |
| `./ob build` | Builds kernel, userspace, ports and docs incrementally | `out/sysroot/` |
| `./ob image` | Builds the USB installer image (hybrid GPT, boots UEFI and BIOS) | `out/orthros-<version>-x86_64.img` |
| `./ob image --iso` | Same system as a hybrid ISO for VMs and optical media | `out/orthros-<version>-x86_64.iso` |
| `./ob run` | Boots the image in QEMU with KVM, OVMF and a serial console | Running VM |
| `./ob run --install` | Boots the installer with a blank 32 GiB virtual disk attached | Running VM |
| `./ob write-usb` | Lists removable drives with model and size, asks for typed confirmation, writes and verifies the image | Bootable USB stick |
| `./ob test` | Unit tests, then QEMU boot and install tests | Test report |
| `./ob docs` | Builds man pages and the handbook; `--check` fails on any command without a man page | `out/docs/` |
| `./ob clean` | Removes build output, keeping the source cache |  |

Every command accepts `--container`, which runs it inside the pinned Podman or Docker build image, so a macOS or Windows host can build too. `./ob write-usb` refuses non-removable disks unless given `--force`, and users who prefer can write the image with balenaEtcher, GNOME Disks, Rufus (DD mode) or `dd`.

`orthros.toml` holds the few choices a builder makes:

```toml
[build]
arch = "x86_64"
profile = "release"        # or "debug"

[features]
lumen = true               # false builds a console-only image
userspace-e1000 = true

[image]
size = "4GiB"
live-user = "live"
```

Host requirements: a Linux x86-64 machine with about 40 GB free disk space, QEMU, OVMF, xorriso, mtools, Clang/LLD, btrfs-progs and dosfstools; `./ob doctor` names anything missing.

## Phased roadmap to 1.0

Seventeen phases lead to 1.0, with three milestones on the way: a console system after Phase 9, the spec's "first usable system" (16.7) once Phases 10 to 13 are done, and Release 1.0 once Phases 14 to 16 are done. Phases 0 to 9 are strictly sequential; after Phase 9 storage, USB, networking and users can run in parallel, and Lumen can start once USB input exists.

Phases 10 to 13 run side by side once the console system works; the phase numbers give the simplest schedule, and Lumen work can start early, as soon as Phase 11 delivers USB input.

Every phase has the same definition of done: code merged with CI green, tests added to `./ob test`, a man page for every new command, file format and interface, the handbook updated, and an ADR for any decision the spec left open.

### Phase 0: Foundations (S)

- Repository, license files, ADR template, contribution guide and coding standard (Clippy `undocumented_unsafe_blocks` set to deny).
- `./ob` skeleton with `setup`, `doctor`, `build`, `run`; build container image; pinned Rust and GRUB.
- CI pipeline; docs pipeline (scdoc and mandoc built for the host, mdBook).
- **Exit:** `./ob run` boots GRUB in QEMU (UEFI and BIOS), which loads a stub kernel that prints `Radix` on the serial port, and CI is green.

**Status:** done, in [pull request #1](https://github.com/openlinuxsystems/orthros/pull/1). Until the Phase 14 image pipeline, GRUB comes from the build container (Ubuntu 24.04, GRUB 2.12) instead of being built from source (ADR 0010).

### Phase 1: Boot and early hardware (M)

- Multiboot2 header, 32-to-64-bit trampoline, higher-half kernel, early page tables.
- 16550 serial and framebuffer console (PSF font), early log ring buffer (`klog`), panic handler with symbolized backtraces. The Dell has no serial port, so the panic report fills the framebuffer and the log is kept in reserved memory across a warm reboot for the next boot to print.
- GDT, IDT and exception handlers; ACPI tables (RSDP, MADT, HPET, MCFG); local APIC, I/O APIC and timer calibration; SMP bring-up of all CPUs; early CPU microcode loading when the optional firmware package is installed.
- QEMU GDB stub workflow (`./ob run --gdb`) and a minimal serial kernel debugger (`kdb`); trace points available from the first instruction of Rust code.
- CI compiles the HAL trait set for AArch64 (stubs only) so x86 assumptions cannot leak into common code.
- **Exit:** on QEMU with 4 CPUs, under UEFI and BIOS, the kernel prints its memory map and CPU list, receives timer interrupts on every CPU, and turns a deliberate page fault into a readable backtrace. The same image, booted from USB, shows its boot log on the Dell's screen.

**Status:** implemented. `./ob test` checks the QEMU exit criteria under UEFI and BIOS, from USB and CD-ROM, and reads the boot log and the panic report back from the framebuffer; the Dell smoke test and its result are in the handbook's Supported hardware chapter. Changes from this plan: the boot information and ACPI tables are parsed by the kernel's own safe crates (ADR 0013); microcode comes from a boot module that `ob image --microcode` adds, until the firmware package exists (ADR 0014). Decisions made in this phase: ADRs 0011 to 0016.

### Phase 2: Memory management (L)

- Buddy frame allocator with zones and per-NUMA-node pools; page-table manager with PCID and TLB shootdown IPIs.
- Kernel heap: slab caches with per-CPU magazines, explicit allocation context, graceful failure everywhere.
- KASLR (relocatable kernel), guard pages, W^X, SMEP and SMAP. A kernel CSPRNG (ChaCha20) seeded from RDSEED and RDRAND plus timing jitter, never trusting a single source, ready before KASLR picks its offset.
- Kani proof harnesses for allocator invariants; host-side property tests with proptest.
- **Exit:** a multi-CPU stress test allocates and frees tens of millions of objects with no leaks or corruption, and the Kani harnesses pass in CI.

**Status:** implemented. The `mm-stress` boot test has 4 CPUs on 2 emulated NUMA nodes allocate and free about 24 million heap objects and map and unmap 800 areas, then checks that no frame leaked and no object was damaged; seven Kani harnesses run in their own CI job (`./ob test kani`), and proptest properties run with the unit tests. Design notes: `docs/design/memory.md`. Changes from this plan: the kernel builds `alloc` without its infallible APIs, so the compiler enforces the explicit allocation context (ADR 0017); KASLR uses a relocation table that `ob build` writes into the image after linking, and also randomizes the direct map and the mapping window (ADR 0018). The CSPRNG, added to this phase on 2026-10-20, hashes all three sources with BLAKE2s and reports their health tests in the boot log (ADR 0022, `docs/design/random.md`). Decisions made in this phase: ADRs 0017 to 0020 and 0022.

### Phase 3: Capability core (M)

- Capability spaces, object types (memory, frame, address space, thread, process, endpoint, notification, IRQ, I/O port, MMIO, DMA), rights masks, badges, sealing.
- Derivation tree with delegation and revocation.
- TLA+ model of derivation and revocation, Kani proofs of the rights checks, and a written semi-formal argument in `docs/design/capabilities.md` (spec 15).
- **Exit:** the in-kernel test suite shows no capability can be forged or its rights widened, and the model checks pass.

**Status:** implemented. The `caps` boot test runs the in-kernel suite on 4 CPUs: it tries every way of using or copying a capability not held, derives with each of the 4,096 rights masks and checks every use left out, checks badges, sealing, ranges and retyping, revokes across spaces and in steps, runs 100,000 random operations under the store's audit, races a revoke against derivations on the other CPUs, and checks that nothing leaked. Eight Kani harnesses prove the rights checks for every capability (`./ob test kani`), and TLC checks the TLA+ model of derivation and revocation, liveness included (`./ob test tla`, in its own CI job). The semi-formal argument is in `docs/design/capabilities.md`, and `radix-capabilities(7)` describes the model. Changes from this plan: rights include `copy` and `transfer`, sealing uses an endpoint as its key, deleting a capability moves its children up to its parent, and revocation works in budgeted steps under a mark that blocks new derivations (ADR 0023); the model is checked with the pinned TLA+ tools 1.7.4 (ADR 0024). Decisions made in this phase: ADRs 0023 and 0024.

### Phase 4: Threads and scheduling (L)

- Context switch with eager XSAVE, kernel threads, per-CPU run queues, idle with MWAIT or HLT.
- EEVDF fair class, FIFO and round-robin real-time classes, priority inheritance, affinity, load balancing that respects cache and NUMA distance.
- RCU, per-CPU data, lock-order checking in debug builds (lockdep-style).
- Resource domains (cgroup-style CPU weight and memory limit objects) as first-class kernel objects (spec 4.1).
- **Exit:** fairness, real-time preemption and priority-inversion test suites pass, and lock-order checking catches a seeded deadlock.

### Phase 5: System calls and IPC (L)

- `SYSCALL` entry and exit with safe user-copy primitives; the dozen core syscalls; a random capability (`getrandom`) that every process receives at start, backed by the kernel CSPRNG, for ASLR, TLS, SSH keys, password salts and signing.
- `ridlc` IDL compiler generating Rust, C and man(2) output.
- Endpoints with a register-only fast path, notifications, shared-memory channels, `radix-ring`.
- Benchmark harness for syscall and IPC round trips, recording Linux numbers on the same machine as a baseline.
- **Exit:** user-mode test programs ping-pong over IPC and rings, and CI publishes the benchmark numbers on every merge.

### Phase 6: First userspace (M)

- ELF64 loader (static and position-independent), user ASLR, process objects.
- Boot archive (`bootfs`, a tar loaded by GRUB as a module) with a minimal read-only VFS.
- `radix-rt` runtime and Spark v0: start one program, reap children, log over the console.
- **Exit:** Spark starts from the boot archive, launches a test program that prints to the console, and reaps it when it exits.

### Phase 7: VFS and basic storage (L)

- Full VFS with capability-based handles, mount table and page cache.
- tmpfs, devfs, procfs (plain-text files), sysfs; block layer driven by rings; virtio-blk; FAT12/16/32 read-write.
- The spec 7.1 directory layout created by Spark at boot.
- **Exit:** Spark mounts a FAT virtual disk, files written are read back after reboot, and `/proc` lists running processes.

### Phase 8: POSIX layer and process model (XL)

- mlibc Radix sysdeps; Rust `std` port; Clang wrappers; ports system.
- `posix_spawn`, `fork` (copy-on-write, inheritable capabilities only), `exec`, `waitpid`.
- Signals, process groups, sessions, controlling terminals; TTY line discipline and PTYs; pipes; `poll` and `select` on top of rings; clocks and timers.
- Static linking for system programs; mlibc's dynamic loader for bundles that ship private libraries.
- **Exit:** a ported C program, a Rust `std` program and the relevant libc-test cases pass, and fork, exec, signal and pipe tests pass.

### Phase 9: Nukesh, core utilities and man pages (L)

- Nukesh: parser, pipes, all redirections, here-documents, `;` `&&` `||`, grouping, subshells, variables and export, globbing, `if`/`for`/`while`/`case`, functions, shebang scripts, job control (`&`, `fg`, `bg`, `jobs`, Ctrl-Z, Ctrl-C), history and completion.
- uutils coreutils, findutils and diffutils ports; GNU grep and sed; one-true-awk; less; libarchive; gzip, bzip2, xz, zip; nano; mandoc.
- Radix-native tools from the architecture table; `consoled` with virtual terminals, and the i8042 PS/2 keyboard driver (QEMU's default keyboard, and how the Dell's built-in keyboard is connected).
- **Exit (milestone: console system):** the machine boots to a Nukesh prompt in QEMU and on the Dell, a scripted suite exercising every utility in spec 7.2 passes, and `man` works for every command in `/system/bin`.

### Phase 10: Native storage: Btrfs, NVMe, AHCI and swap (XL)

- AHCI and NVMe drivers, AHCI first because both of the Dell's drives are SATA.
- Btrfs in four steps: read-only (superblock, chunk, root and filesystem trees, CRC32C checksums); read-write copy-on-write with transaction commits; subvolumes and snapshots; zstd compression and scrub. Single-device profiles (single and DUP) only in 1.0.
- exFAT read-write, ext4 read-only; swap partitions in the standard swap format; page reclaim and swap-out.
- Linux `btrfs check` runs in CI on every image Radix writes; images made by Linux `mkfs.btrfs` must mount in Radix; power-cut tests kill QEMU at random points and check consistency.
- **Exit:** the root filesystem is Btrfs, a long filesystem stress run passes `btrfs check` clean, and the system survives memory pressure by swapping.

### Phase 11: USB, input and removable media (L)

- xHCI, hubs, HID keyboard and mouse, USB mass storage (BOT, then UAS), console keymaps, a console over the xHCI Debug Capability for machines without a serial port, and the ACPI power button with clean power-off.
- IOMMU (Intel VT-d) programming so device DMA is confined to granted buffers.
- `volumed`: mounts inserted volumes at `/volumes/<label>`, grants access to the logged-in user, safe eject.
- **Exit:** a FAT or exFAT USB stick appears under `/volumes` in QEMU and on the Dell, ejecting removes it, a USB keyboard works alongside the built-in one, and the power button shuts the laptop down cleanly.

### Phase 12: Networking (XL)

- NIC drivers in priority order: virtio-net and e1000 for QEMU; the Realtek r8169 family, including the Dell's RTL810x Fast Ethernet chip (PCI ID 10ec:8136); USB Ethernet (CDC-ECM, CDC-NCM and RTL8153 gigabit adapters); then Intel e1000e and igc. The e1000 driver also runs as a userspace process.
- Stack built from smoltcp and extended for production use: TCP window scaling, SACK, timestamps, CUBIC; IPv6 with NDP and SLAAC; ICMPv4 and v6; bridging and VLANs; multiple interfaces; a small ordered packet-filter ruleset.
- BSD sockets in mlibc, gated by network capabilities (outbound, listen, per-port rights).
- `netd` (DHCPv4 and v6 client, optional DHCP server, static configuration), `resolved` (caching, multiple resolvers, DNS-over-TLS and HTTPS), `timed` (SNTP, writes the RTC); `ip`, `ss`, `ping`, `curl`; iperf3 port for measurement; netlog streams kernel and service logs over UDP to another machine for debugging on the Dell.
- **Exit:** DHCP works on QEMU and on the Dell's built-in Ethernet, `curl https://` succeeds, a bundle without network permission cannot open a socket, and iperf3 throughput is recorded against Linux on the same hardware, using a USB 3 gigabit adapter because the built-in port tops out at 100 Mbps.

### Phase 13: Users, bundles and logging (L)

- `authd` with Argon2id password hashes, `login` on every console, the `user` tool, home directory skeleton, per-user and admin capability sets.
- Bundle runtime: manifest JSON schema and validation, keyboard-only permission prompt, grant storage, launch with exactly the granted capabilities and a filesystem view built from them.
- Bundle signing with Ed25519 (minisign-compatible) over a hashed file list; `bundle` CLI (`install`, `list`, `info`, `grant`, `revoke`, `run`, `remove`, `sign`, `verify`).
- `logd` and `logtrim` writing JSON lines under `/system/var/log`; audit log of security events; `notifyd` with console delivery.
- **Exit (milestone, with Phases 10 to 12: spec 16.7 first usable system):** a user installs a sample bundle into `~/applications`, approves `documents: read-write`, launches it, and it can write `~/documents` but not `~/downloads`; unsigned bundles are refused under the default policy; only the admin can read logs.

### Phase 14: Installer, rdisk and USB media (L)

- Live image, `orthros-install`, `rdisk`, `mkfs-btrfs`, `mkfs-fat`, `mkswap`, `rgrub-setup` and answer-file installs, as specified in the next section.
- **Exit:** an automated QEMU test installs to a blank disk under UEFI and under BIOS, reboots and logs in, and a manual install from a USB stick succeeds on the Dell.

### Phase 15: Atomic updates and snapshots (M)

- `sysupdate`: signed update images, A/B system subvolumes, GRUB boot counting through `grubenv` on the EFI partition, a `boot-complete` health check in Spark, automatic rollback.
- `snap`: create, list, restore and delete snapshots of `/system`, `/users` or any subvolume; optional inclusion of `/volumes` targets.
- **Exit:** an update applies in QEMU; a deliberately broken update rolls back on its own; a deleted file in `/users` is restored from a snapshot.

### Phase 16: Lumen compositor (L)

- Display layer with modes, framebuffers and page flips for GOP and virtio-gpu; `inputd` event devices behind capabilities; the Dell's touchpad, which is most likely HID over I2C and needs Intel LPSS I2C, Intel GPIO and I2C-HID drivers (a USB mouse works until then).
- Lumen as specified in its own section below, plus the `terminal.bundle` test client.
- **Exit (milestone: Release 1.0):** the full acceptance list in the Release 1.0 section passes.

## Installer, rdisk and USB install media

One USB image boots on UEFI and legacy BIOS machines, offers a live console, and installs OrthrOS with a keyboard-only installer whose disk tool, `rdisk`, also works on its own like fdisk.

### USB install image

`./ob image` produces a hybrid GPT disk image (default 4 GiB) that is written raw to a USB stick:

| Region | GPT type | Size | Contents |
| --- | --- | --- | --- |
| Protective MBR | none | sector 0 | GRUB `boot.img` for BIOS boot |
| BIOS boot | EF02 | 1 MiB | GRUB `core.img` |
| EFI System | EF00, FAT32 | 256 MiB | `EFI/BOOT/BOOTX64.EFI` (GRUB), `grub.cfg`, the `radix` kernel, `bootfs.tar`, optional `install.json` |
| ORTHROS\_LIVE | OrthrOS data, Btrfs | rest | Read-only live system and the signed system image the installer writes to disk, plus the separate optional firmware package |

The system image is the same signed format that `sysupdate` uses, so installation and updates share one code path and one verification step.

The GRUB menu offers: **Install OrthrOS**, **Try OrthrOS (live console)**, **Install with serial console**, and **Boot from first disk**. The live system mounts tmpfs over `/system/etc`, `/system/var`, `/users` and `/tmp`, starts the installer on the first virtual terminal and a rescue Nukesh on the second.

### Installer (`orthros-install`)

A text interface built with ratatui. It works entirely from the keyboard and over a serial console (spec 7.14). Steps:

1. Keyboard layout.
2. Disk choice: every disk with model, size, bus and current partitions. The USB stick the system booted from is marked and cannot be chosen.
3. Partitioning: **Erase disk and use the guided layout**, **Use free space**, or **Manual** (opens `rdisk`).
4. Review: an exact list of what will be created, erased and formatted. Destructive changes require typing the disk's name.
5. Install: partition, format, create subvolumes, verify and unpack the signed system image, install GRUB for both UEFI and BIOS (with a Windows Boot Manager menu entry when one is found), register a UEFI boot entry and the fallback `EFI/BOOT/BOOTX64.EFI` path.
6. Hostname, time zone, and network (DHCP by default, or static addresses). Then the optional firmware package (decided; ADR 0007): closed-source but redistributable files, Intel CPU microcode now and Wi-Fi firmware after Phase 20, installed only if chosen and kept outside /system.
7. Administrator account (spec 7.7), plus optional ordinary users.
8. Summary, eject prompt, reboot.

The install log goes to `/tmp/install.log` and is copied to `/system/var/log/install.log` on the new system. An answer file (`install.json`, on the EFI partition or passed with `--answers`) runs the same steps unattended, which is how CI tests every install.

### Guided disk layout

| Partition | GPT type | Size | Filesystem | Purpose |
| --- | --- | --- | --- | --- |
| BIOS boot | EF02 | 1 MiB | none | GRUB `core.img` for BIOS machines |
| EFI System | EF00 | 512 MiB | FAT32 | GRUB, kernels and boot archives for both update slots, `grubenv` |
| Swap | Linux swap GUID | Equal to RAM, capped at 8 GiB (installer default, adjustable, 0 disables) | swap | Paging and future suspend-to-disk |
| OrthrOS root | OrthrOS root GUID (minted in Phase 0) | Remaining space | Btrfs | Everything else |

Btrfs subvolumes on the root partition:

| Subvolume | Mounted at | Notes |
| --- | --- | --- |
| `@root` | `/` | Only mount-point directories |
| `@system.a`, `@system.b` | `/system` | Read-only; one active slot, one for the next update |
| `@etc` | `/system/etc` | Writable, survives updates |
| `@var` | `/system/var` | Writable; logs, caches, package and grant state |
| `@applications` | `/applications` | System-wide bundles |
| `@users` | `/users` | Home directories |
| `@data` | `/data` | Shared data |
| `@snapshots` | not mounted | Managed by `snap` |

`/tmp` and `/run` are tmpfs; `/dev`, `/proc` and `/sys` are virtual; `/volumes` holds removable media. The EFI partition is mounted at `/system/boot`.

### `rdisk` partitioner

`rdisk` is an fdisk-style tool for GPT and MBR disks, written in Rust and documented in `rdisk(8)`.

- **Interactive mode** keeps fdisk's single-letter commands so the habit carries over: `p` print, `n` new, `d` delete, `t` type, `l` list types, `g` new GPT, `o` new MBR, `c` name, `u` units, `v` verify, `w` write, `q` quit, `m` help.
- **Script mode** (`rdisk --script layout.json /dev/disk0`) applies a layout file; the installer and `./ob image` both use it.
- GPT primary and backup headers with CRC32 checks and repair, protective MBR, 1 MiB alignment, partition names, readable type aliases (`efi`, `bios-boot`, `swap`, `orthros-root`, `linux`, `windows-data`).
- Nothing is written before `w`; `--dry-run` prints the result; mounted disks are refused.
- Disks appear as `/dev/disk0`, partitions as `/dev/disk0p1`, with `/dev/disk/by-label/` and `/dev/disk/by-uuid/` links.
- Builds for Linux too, so `./ob image` uses the same code that runs on target machines.

### Formatting and boot tools

| Tool | Purpose | Notes |
| --- | --- | --- |
| `mkfs-btrfs` | Create Btrfs filesystems and subvolumes | Shares the on-disk structure crate with the kernel driver; output verified with Linux `btrfs check` in CI. Fallback: port btrfs-progs |
| `fsck-btrfs` | Check Btrfs (read-only in 1.0) | Same crate |
| `mkfs-fat`, `fsck-fat` | FAT12/16/32 for EFI partitions and USB sticks |  |
| `mkfs-exfat` | exFAT for large USB media |  |
| `mkswap`, `swapon`, `swapoff` | Swap setup | Standard swap format |
| `mkfs` | Front end: `mkfs -t btrfs /dev/disk0p4` |  |
| `rgrub-setup` | Installs GRUB: EFI binary to the EFI partition; for BIOS writes `boot.img` and `core.img` and patches their sector pointers as `grub-bios-setup` does | GRUB images prebuilt by `./ob` from pinned source with only the modules needed (gpt, msdos, fat, btrfs, multiboot2, loadenv, search, gfxterm, serial) |

### Writing the stick

`./ob write-usb` is the recommended path. Alternatives that also work: balenaEtcher, GNOME Disks "Restore Disk Image", Rufus in DD mode, or `sudo dd if=out/orthros-<version>-x86_64.img of=/dev/sdX bs=4M conv=fsync status=progress`.

## Lumen compositor

Lumen 1.0 is a complete Wayland compositor with no desktop environment: it shows windows, title bars and notifications, and nothing that resembles a panel, launcher, dock or settings app.

### In scope

- **Foundation:** Smithay (Rust Wayland compositor library) with a Radix backend written for the display layer and `inputd`, and the pixman software renderer. wayland-rs uses its pure-Rust server, so libwayland is not required.
- **Protocols:** `wl_compositor`, `wl_shm`, `wl_seat`, `wl_output`, `xdg-shell`, `xdg-decoration`, `wl_data_device` (clipboard), `viewporter`, `fractional-scale`, `presentation-time`, `linux-dmabuf` equivalent once virtio-gpu buffers are shareable.
- **Title bars (spec 7.4):** Lumen draws server-side title bars with minimize, maximize/restore and close on every window. A global setting and per-application overrides force server-side decorations; client requests to disable them are ignored when forced, and the client is told through `xdg-decoration`.
- **Window management:** stacking with click-to-focus, keyboard focus cycling (Alt-Tab), move and resize by title bar and edges, maximize, minimize to a keyboard-reachable list (Super key), and a fallback key to close a window. These are compositor behaviors, not a desktop shell.
- **Capabilities:** Lumen is a Spark-supervised service and the only holder of display and input device capabilities. A bundle connects only if its manifest declares `"display": true` and the grant was approved; otherwise its connection fails cleanly. Clipboard read, screen capture and input injection are separate grants.
- **Notifications:** Lumen subscribes to `notifyd` and shows plain-text notifications with position, timeout and do-not-disturb settings.
- **Accessibility:** a Lumen accessibility protocol exposes window titles, focus changes and basic roles to a screen-reader bundle; high-contrast and large-title-bar themes; everything usable from the keyboard.
- **Configuration:** `/system/etc/lumen.json` for system defaults and `~/.config/lumen.json` per user, both documented in `lumen.json(5)`.
- **Starting it:** `lumen` from Nukesh, or enable `lumen.service` in Spark to start it after login. Leaving Lumen out of `orthros.toml` builds a console-only image (spec 7.4, 16.6).
- **Test client:** `terminal.bundle`, a small Rust terminal emulator (alacritty\_terminal for the VT parser, fontdue for text) that proves input, rendering, decorations and permissions end to end.

### Out of scope for 1.0

- A desktop shell, panel, launcher, wallpaper manager or settings application.
- Hardware-accelerated rendering on native GPUs (virtio-gpu 2D and software rendering only).
- XWayland, GTK and Qt ports. These come after 1.0 so that existing toolkits can run on Lumen.
- Multi-monitor layout beyond mirroring or a single extended output on virtio-gpu.

## Documentation and man pages

Every shipped command, service, file format, system call and library interface gets a man page, and `./ob docs --check` fails the build when one is missing.

### Formats and tools

- **Man pages** are written in scdoc (a small Markdown-like syntax, MIT) and compiled to roff. On the system, `man` is mandoc (ISC), which also renders HTML for the project website. Pages install to `/system/share/man`.
- **Handbook** is written in mdBook (MPL-2.0) and installs as plain text and HTML under `/system/share/doc/handbook`, readable from the console with `less` (spec 7.12).
- **Design documents** live in `docs/design/` as Markdown, one per subsystem, each with its `unsafe` inventory and safety arguments.
- **API reference** comes from rustdoc for every crate and from `ridlc` for every interface.
- Ported tools ship their upstream man pages; uutils pages are generated from its argument definitions. Where OrthrOS behaves differently (for example permissions), the page gets an "OrthrOS notes" section.
- Every tool supports `--help`, and Nukesh has a `help` builtin.

### Man page sections

| Section | Contents | Examples |
| --- | --- | --- |
| 1 | User commands | `nukesh(1)`, `bundle(1)`, `snap(1)`, `lumen(1)`, `ps(1)` |
| 2 | System calls, generated from `.ridl` files | `cap_call(2)`, `ring_setup(2)`, `map(2)` |
| 3 | Library interfaces | `libradix(3)`, `radix_ring(3)` |
| 4 | Devices and drivers | `nvme(4)`, `xhci(4)`, `e1000(4)`, `virtio-gpu(4)` |
| 5 | File formats | `manifest.json(5)`, `spark.service(5)`, `install.json(5)`, `lumen.json(5)`, `network.json(5)`, `grants(5)` |
| 7 | Overviews and concepts | `radix-capabilities(7)`, `orthros-hier(7)`, `radix-fs-rights(7)`, `bundles(7)`, `orthros-boot(7)`, `orthros-update(7)` |
| 8 | Administration | `rdisk(8)`, `orthros-install(8)`, `mkfs-btrfs(8)`, `sysupdate(8)`, `spark(8)`, `netd(8)`, `logd(8)`, `user(8)`, `rgrub-setup(8)` |

### Handbook chapters

1. Introduction to OrthrOS and Radix
2. Building OrthrOS with `ob`
3. Creating USB install media
4. Installing, including manual partitioning with `rdisk`
5. First boot and login
6. Nukesh guide and scripting
7. Files, the directory hierarchy and `/volumes`
8. Bundles and permissions
9. Users and administration
10. Networking
11. Updates, snapshots and recovery
12. Lumen
13. Logs and troubleshooting
14. Supported hardware

Developer documentation adds a porting guide (writing `ports/` recipes), a driver-writing guide, the ABI reference, the contributor guide and the ADR log (spec 13).

### How completeness is enforced

- `./ob docs --check` confirms that every binary in `/system/bin`, every service, every `.ridl` interface and every JSON schema has its page, and runs `mandoc -T lint` on all of them.
- Tables of manifest and service-file fields are generated from the JSON schemas, so the docs cannot drift from the parser.
- Handbook code blocks tagged as tests run inside QEMU during `./ob test`.

## Testing, CI and reproducibility

Every push runs unit tests, model checks and QEMU boot tests; every merge to main also runs full install tests; nightly runs add fuzzing, filesystem torture and performance comparisons against Linux.

### Test layers

| Layer | Tools | When |
| --- | --- | --- |
| Unit and property tests | `cargo test` on the host, proptest. Kernel logic crates (mm, cap, sched policy, Btrfs structures, netstack, parsers) are written to compile for the host | Every push |
| Model checking and proofs | Kani for allocators, capability rights and ring buffers; TLA+ (TLC) for capability revocation, IPC, scheduler and the update state machine | Every push; long runs nightly |
| Undefined-behavior checks | Miri on the host-testable parts of `lib/hw`, allocators and ring code; debug kernels with red-zone allocators and lock-order checking | Nightly |
| In-kernel tests | A test runner booted in QEMU (`./ob test kernel`) | Every push |
| Boot tests | QEMU boots under UEFI and BIOS to a Nukesh prompt, driven by serial expect scripts | Every push |
| Install tests | Unattended install from the USB image to a blank virtual disk under UEFI and BIOS, then boot and log in | Every merge to main |
| System suites | Every spec 7.2 utility, bundle permissions, users, logging, two-VM network tests, update and rollback, Lumen screenshots compared with reference images | Nightly |
| Filesystem torture | Linux `btrfs check` on images Radix wrote; images from Linux mounted by Radix; random power-cut tests; stress workloads | Nightly |
| Fuzzing | cargo-fuzz (libFuzzer) on ELF, manifests, JSON configs, GPT and MBR, Btrfs, FAT and exFAT metadata, network packets and IPC decoders; a syscall fuzzer running in QEMU with KVM | Continuous on a dedicated runner |
| Performance | Microbenchmarks for syscall, context switch, IPC, page fault and memory bandwidth; storage and iperf3 throughput; each compared with Linux on the same hardware, with regressions failing the nightly run | Nightly in QEMU; per release on the Dell |
| Real hardware | Install and acceptance checklist on the Dell, with remote power control and the xHCI debug console once automated | Every release |
| Dependency checks | cargo-deny (licenses and advisories), cargo-audit, and an `unsafe` inventory report | Every push |

### CI setup

CI runs on GitHub Actions or Forgejo Actions, both free for open source projects. QEMU tests need runners with KVM access; fuzzing and nightly torture runs fit best on one self-hosted machine. Every pipeline step is just an `./ob` command, so anything CI does can be run locally the same way.

### Reproducible builds

- Pinned Rust, Clang and GRUB versions; `Cargo.lock` plus vendored crates; SHA-256-pinned port sources.
- `SOURCE_DATE_EPOCH` everywhere; filesystem UUIDs, timestamps and file order in images derived from the version, not the clock.
- CI builds each release twice in different directories and compares the outputs with diffoscope.
- Release images ship with SHA-256 checksums and Ed25519 (minisign) signatures.

## Reference machine: Dell Inspiron 15 5570

The Dell Inspiron 15 5570 with a Core i7-8550U is the real-hardware target for 1.0. The table comes from Dell's factory configuration for the project's unit. The Ethernet chip is not in that list; it comes from published 5570 specs ([LaptopMedia](https://laptopmedia.com/laptop-specs/dell-inspiron-15-5570-667/review/), [Ubuntu certification for the sister 5580](https://ubuntu.com/certified/platforms/9412)) and still needs a check with `lspci -nn` from any Linux live USB.

| Component | This unit | Work in the plan | Phase |
| --- | --- | --- | --- |
| CPU and memory | Core i7-8550U, 4 cores and 8 threads, VT-x and VT-d; 8 GB DDR4-2400 in one slot | SMP, PCID, SMEP and SMAP. No CET, so Phase 18 needs a newer test machine for that | 1, 2 |
| Firmware | Dell UEFI, factory-set to UEFI boot with GPT; likely offers a Legacy boot option and Intel PTT (TPM 2.0) | GRUB UEFI and BIOS paths; TPM in Phase 18 | 0, 1, 18 |
| Storage | 128 GB SK hynix SC311 M.2 SATA SSD holding Windows 10, plus a 1 TB 5400 rpm 2.5-inch hard drive; shipped in Intel RST "RAID On" mode | AHCI driver first; NVMe is exercised in QEMU only | 10 |
| Ethernet | Realtek RTL810x 10/100 Fast Ethernet, PCI ID 10ec:8136 (from published specs, to confirm) | Realtek r8169-family driver; a USB 3 gigabit adapter for speed tests | 12 |
| Wi-Fi and Bluetooth | Qualcomm QCA9377 (Dell Wireless 1810) on an M.2 card | Post-1.0 | 20 |
| Graphics and display | Intel UHD 620 only, no discrete GPU; 15.6-inch 1920x1080 AU Optronics eDP panel, non-touch | UEFI GOP framebuffer at 1920x1080 in 1.0; native Intel driver post-1.0 | 16, 20 |
| Keyboard and hotkeys | US keyboard on the i8042 PS/2 controller. Dell's driver list includes the Intel HID event filter, so the power button and hotkeys probably arrive through it | i8042 driver; Intel HID ACPI device (INT33D5) handled with the power button | 9, 11 |
| Touchpad | Dell's driver list includes Intel Serial IO, the I2C controller driver, so HID over I2C is very likely | Intel LPSS I2C, Intel GPIO and I2C-HID drivers | 16 |
| USB | Intel xHCI with USB 3.1 Gen 1 and USB 2.0 ports | xHCI, hubs, HID, mass storage, debug console | 11 |
| Other devices | Realtek ALC3246 audio, Realtek USB card reader, 720p USB camera, Goodix fingerprint reader, 42 Wh battery | Post-1.0 | 19, 20 |

### Firmware setup before the first boot

The handbook's "Creating USB install media" chapter gets a page for this laptop with these steps:

1. Turn off Secure Boot (signed boot returns in Phase 18).
2. Set **SATA Operation** to **AHCI** instead of **RAID On**. Dell ships Intel RST RAID mode, which hides the SSD from other operating systems. If Windows is installed and should keep booting, switch it to AHCI first by booting once into Safe Mode.
3. If the setup offers a Legacy boot list, enable it to test the BIOS path on the same laptop; otherwise BIOS boot is tested in QEMU only.
4. Boot the USB stick from the F12 one-time boot menu.

### Debugging without a serial port

The laptop has no serial port, so three tools are folded into the phases above:

- **Phase 1:** a full-screen framebuffer panic report, with the log kept in reserved memory across a warm reboot so the next boot can print it.
- **Phase 11:** a console over the xHCI Debug Capability, using a USB 3 A-to-A debug cable to a second computer.
- **Phase 12:** `netlog`, which streams kernel and service logs over UDP to another machine on the network.

### Test rhythm and laptop scope

- The latest USB image is smoke-booted on the Dell at the end of Phases 1, 2, 6 and 9, then at the end of every later phase. Results go in `docs/hardware/dell-inspiron-15-5570.md`, which becomes the first entry of the supported-hardware list.
- In 1.0 the laptop gets the power button and a clean power-off. Battery status, the lid switch, brightness keys and suspend wait for Phase 19.

Decided: OrthrOS installs onto the 128 GB SSD with the guided layout, erasing Windows 10. The guided install leaves the 1 TB hard drive untouched; it can be formatted later with rdisk and used under /data or /volumes. Anything worth keeping on the SSD needs backing up before the first Phase 14 install.

## Release 1.0 acceptance: a working system

OrthrOS 1.0 ships when every item below passes, automated in QEMU where possible and by hand on the Dell reference laptop.

**Build and media**

- [ ] On a clean Linux host, `./ob setup && ./ob image` produces `orthros-1.0-x86_64.img` with no manual steps.
- [ ] Two builds of the same commit are bit-for-bit identical.
- [ ] `./ob write-usb` writes and verifies a stick that boots the Dell in UEFI mode, and in legacy BIOS mode if its setup offers Legacy boot (otherwise BIOS boot is checked in QEMU).

**Installation**

- [ ] The guided install erases a disk and installs; the system reboots into the installed copy without the stick.
- [ ] A manual install with `rdisk` (custom partition sizes, existing partitions kept) succeeds.
- [ ] An unattended install from `install.json` succeeds in CI under UEFI and BIOS.

**Spec 16.7 first usable system**

- [ ] Boots to a Nukesh login; the administrator logs in.
- [ ] The user creates files under `~/development` and `~/documents`.
- [ ] A bundle copied into `/applications` or `~/applications` asks for its permissions, receives only what was approved, and launches.
- [ ] Pipes, redirection and job control work in Nukesh.
- [ ] A USB drive appears under `/volumes`, accessible according to capabilities, and ejects safely.
- [ ] All TCB code is memory-safe or has documented, reviewed `unsafe` sections.

**System services**

- [ ] DHCP over wired Ethernet, DNS resolution, `curl https://` and time synchronization all work.
- [ ] A bundle without network permission cannot open a socket.
- [ ] Logs are written as text or JSON lines under `/system/var/log` and readable only with the log capability.
- [ ] Swap is active and the system survives memory pressure.

**Updates and recovery**

- [ ] A signed update installs atomically; an unsigned one is refused.
- [ ] A deliberately broken update rolls back automatically.
- [ ] A snapshot of `/users` is created, listed and restored.

**Lumen**

- [ ] Lumen starts on the Dell's UEFI framebuffer and on virtio-gpu, and the Dell's built-in keyboard and touchpad work in it.
- [ ] `terminal.bundle` opens with Lumen's title bar, and minimize, maximize and close work with mouse and keyboard.
- [ ] Forcing server-side decorations per application works.
- [ ] A bundle without the display grant fails cleanly; the console stays usable with Lumen absent.

**Documentation**

- [ ] `./ob docs --check` passes: every command, service, file format and interface has a man page.
- [ ] The handbook covers building, media creation, installation, administration and recovery.

## Post-1.0 roadmap to full spec conformance

After 1.0, ten more phases close every remaining spec requirement; Phase 17 comes first because spec 15 requires booting on two architectures. The order of Phases 18 to 26 can follow what users need most.

| Phase | Theme | Main work | Spec |
| --- | --- | --- | --- |
| 17 | AArch64 | HAL for AArch64, GICv3, generic timer, device tree and ACPI; QEMU `virt` first, then one real board; GRUB arm64 EFI images and an AArch64 USB image from `./ob image --arch aarch64` | 1.2, 6.2, 15 |
| 18 | Platform security | Secure Boot with user-enrolled keys (sbctl or MOK), measured boot with TPM 2.0, remote attestation, root-filesystem encryption unlocked by passphrase or TPM, Intel CET shadow stacks and indirect-branch tracking (needs an 11th gen or newer Intel test machine; the Dell's 8th gen CPU lacks CET), wider audit logging | 8 |
| 19 | Audio, printing, power | HDA, virtio-snd and USB audio with `audiod`; CUPS port behind a capability wrapper; ACPI S3 suspend, suspend-to-disk to swap, runtime device power management, battery and thermal reporting | 7.9, 7.10, 7.15 |
| 20 | Hardware breadth | Wi-Fi (starting with the Dell's Qualcomm QCA9377, which needs Qualcomm's redistributable but closed firmware file) with a WPA supplicant, Bluetooth, native Intel and AMD display drivers, Mesa for GPU acceleration, PCIe and memory hotplug | 3.1, 6.1, 6.2 |
| 21 | Toolkits and applications | GTK and Qt on Lumen, optional XWayland, OpenSSH, more application bundles | 7.4, 13 |
| 22 | Virtualization and containers | VT-x and AMD-V host support with a Radix VMM API and a VMM built from rust-vmm crates; container tooling on resource domains and capability dropping | 4.1, 6.2, 13 |
| 23 | Confidential computing and CXL | Intel TDX and AMD SEV-SNP guest support, Arm CCA, CXL memory tiering | 6.2, 8, 14 |
| 24 | Scale and real time | Scaling work to 128 to 256 cores, deadline scheduling class, tail-latency tuning, programmable packet path (XDP-style), RDMA, zero-copy networking, persistent memory | 4.2, 6.3, 11 |
| 25 | Observability and live patching | Dynamic probes with a verified safe bytecode, hardware performance counters, crash dumps to reserved memory with a post-mortem tool, kernel live patching | 9, 10, 15 |
| 26 | RISC-V | HAL for RISC-V (SBI, PLIC or AIA), QEMU `virt`, then one real board | 1.2, 6.2 |

A Linux system-call compatibility layer (spec 13) stays optional and outside the TCB; it is worth starting only once the native ABI is stable.

## Spec traceability matrix

Every section of spec v1.4 maps to at least one phase; Phase 0's CI keeps this table as `docs/traceability.md` so a requirement cannot be silently dropped.

| Spec section | Requirement | Phases |
| --- | --- | --- |
| 2.1, 2.2 | Hybrid structure, minimal TCB | 1 to 5 (TCB), 7 and 10 to 12 (compartments) |
| 2.3 | Capability security, no ambient authority | 3, enforced in every later phase |
| 3.1 | Physical memory, zones, NUMA | 2; hotplug in 20 |
| 3.2 | Virtual memory, demand paging, copy-on-write, ASLR | 2, 7, 8 |
| 3.3 | Memory protection, IOMMU, W^X, speculative-execution mitigations | 2, 11; CET in 18; memory tagging on Arm in 17 |
| 3.4 | Allocation interfaces | 2 |
| 4.1 | Processes, threads, namespaces, resource controllers | 4, 6, 13, 22 |
| 4.2 | Scheduling | 4; energy-aware and heterogeneous CPUs in 17; deadline class in 24 |
| 4.3 | Synchronization, RCU | 4 |
| 5.1, 5.2 | System calls, async I/O, IPC | 5 |
| 6.1 | Driver model, userspace drivers | 7, 10 to 12; power management in 19 |
| 6.2 | HAL, architectures, virtualization, confidential computing | 1, 17, 26; 22, 23 |
| 6.3 | Storage and networking | 7, 10, 12; high-performance paths in 24 |
| 7 | Filesystems, Btrfs, swap, FAT and exFAT, ext4 | 7, 10, 11 |
| 7.1 | Hierarchy, bundles, manifests, `/volumes` | 7, 11, 13 |
| 7.2 | Nukesh and core utilities | 9 |
| 7.3 | Spark init and service management | 6, 9, 13 |
| 7.4 | Lumen compositor | 16; toolkits in 21 |
| 7.5 | Logging | 1 (early log), 13 |
| 7.6 | Atomic base-system updates | 15 |
| 7.7 | Users and accounts | 13, 14 |
| 7.8 | Networking | 12 |
| 7.9 | Audio | 19 |
| 7.10 | Power management | 19 |
| 7.11 | Time synchronization | 1 (hardware clock), 12 |
| 7.12 | Documentation | 0 and every phase's definition of done |
| 7.13 | Snapshots and backups | 15 |
| 7.14 | Accessibility baseline | 9, 14, 16 |
| 7.15 | Printing | 19 |
| 7.16 | Notifications | 13, 16 |
| 8 | Security features | 2, 13, 18, 23 |
| 9 | Reliability and fault isolation | Compartment restart from 7; Spark health checks in 15; live patching in 25 |
| 10 | Observability and debugging | 1, 5; completed in 25 |
| 11 | Performance | Benchmarks from 5; tuning in 24 |
| 12.1, 12.2 | Tooling, CI, coding standards | 0, ongoing |
| 12.3 | Boot, GRUB 2, UEFI and BIOS | 0, 1, 14 |
| 13 | Stable ABI, compatibility, containers | 5, 8, 21, 22 |
| 14 | Extensibility | Versioned interfaces from 5; new architectures in 17, 26 |
| 15 | Acceptance criteria | Release 1.0 plus Phase 17 (second architecture) |

## Risks and open decisions

The biggest risk is the Btrfs implementation; the reference machine is now settled, so the remaining open decisions are small.

### Risks

| Risk | Impact | Mitigation |
| --- | --- | --- |
| Btrfs written from scratch is the largest single component | Data loss, or Phase 10 running long | Read-only first; single-device profiles only; Linux `btrfs check` and power-cut tests in CI from day one. Fallback allowed by spec 7: a simpler purpose-built copy-on-write filesystem |
| Real-hardware quirks in ACPI, USB and NICs | Works in QEMU, fails on real machines | Smoke boots on the Dell from Phase 1 onward, a published supported-hardware list, uACPI isolated in a compartment as a fallback AML interpreter |
| Rust `std` port drifts with nightly Rust | Broken userspace builds | Pinned toolchain, deliberate quarterly upgrades, upstream the target as tier 3 |
| smoltcp is not yet production-grade for servers | Spec 7.8 requires a production-quality stack at first release | Extend it (SACK, window scaling, CUBIC), fuzz it, and run interoperability tests against Linux peers every night |
| IPC overhead for userspace drivers | Slow I/O | Drivers run as in-kernel compartments by default; only the e1000 demonstration runs in userspace until benchmarks justify more |
| C tools assume Linux (procfs, ioctl, UID/GID) | Port breakage and patch load | Compatibility shims in mlibc; prefer Rust tools where a good one exists |
| GPL code leaking into the kernel | License conflict | cargo-deny license policy on kernel crates; GPL ports confined to `ports/` |
| Secure Boot must be disabled for 1.0 | Install friction on new PCs | Handbook chapter with firmware steps; signed boot path in Phase 18 |
| Scope versus team size | Long time to 1.0 | Each milestone is usable on its own; the console system after Phase 9 is already shareable for contributors |

### Decisions

Each row records a decision the plan depends on and the alternatives considered. The architecture decision records in `docs/adr/` hold the reasoning.

| Decision | Default | Alternatives |
| --- | --- | --- |
| Reference machine | Decided: Dell Inspiron 15 5570 with a Core i7-8550U | Model confirmed from Dell's factory configuration; Ethernet chip still to confirm with lspci |
| Code hosting and CI | Decided: GitHub with GitHub Actions | Codeberg or self-hosted Forgejo |
| License for OrthrOS code | Decided: Apache-2.0 OR MIT | MPL-2.0, GPL-2.0 |
| Native filesystem | Decided: Btrfs | Purpose-built copy-on-write filesystem |
| Linking of system programs | Decided: Static | Dynamic with mlibc's loader |
| Default install image | Decided: full 4 GiB image with Lumen | Smaller console-only image |
