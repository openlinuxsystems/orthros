<!--
SPDX-FileCopyrightText: 2026 The Dukovo contributors
SPDX-License-Identifier: Apache-2.0 OR MIT
-->

# Dukovo roadmap: Radix implementation plan

This is the implementation plan for Dukovo, from the Radix kernel
specification (v1.4) to Release 1.0 and beyond, as agreed on 2026-10-20
and reordered server first on 2026-10-23 (ADR 0078). When a phase's scope
changes, update this file in the same pull request. Decisions it refers to
are recorded in [`docs/adr/`](adr/README.md).

## Summary

Dukovo 1.0 is a rock-solid, installable x86-64 **server operating system** on the Radix kernel, in the spirit of Ubuntu Server (decided on 2026-10-23). It boots from a USB stick or a cloud image, partitions and formats disks with its own tools, installs itself, and runs headless: Nukesh and a full administration command set, networking, bundles and service bundles, atomic updates, Btrfs with mirroring, and **Dukovo Admin**, a built-in web console in the spirit of Fedora's Cockpit that runs the whole machine from any browser. It serves websites and databases out of the box (nginx, PostgreSQL, SQLite, Valkey, automatic HTTPS certificates). The plan reaches 1.0 in 17 phases (0 to 16), each ending in something you can boot and test in QEMU. The graphical side starts right after 1.0: the Lumen compositor is Phase 17, the first 1.x release, followed by the UI kit and Store, containers through Podman, developer tools, the WebAssembly app platform, the optional personality shell, gaming, a local AI assistant and a full set of first-party apps. Post-1.0 phases are numbered in the order we plan to build them.

Why server first: a server release is judged on uptime, data safety and security, which is exactly the foundation the desktop needs, and most servers run as virtual machines whose devices Radix already supports, which eases the driver problem. The web console gives Dukovo a polished, first-class way to administer it from day one.

Sizing, stated plainly: projects of this kind (Redox, SerenityOS, Managarm) took small teams several years to reach a comparable point. Phases therefore carry relative sizes (S, M, L, XL), not dates.

### Guiding principle: favor the user

When a decision reaches a fork in the road, Dukovo favors the user. It is a users-first system, not a platform run for a company's benefit. Freedom is at the core of its philosophy: the freedom to use, study, change and share the system, to run any software the user chooses, and to decide everything about one's own machine. In practice:

- The user owns the machine. Updates and restarts happen only when the user chooses, and nothing is forced.
- Warn and confirm, never forbid. Risky choices, such as a weak password or sideloading an unsigned app, get a plain warning and a confirmation, and then the user decides.
- No telemetry, no ads, no required online accounts, and no data leaves the machine unless the user sends it.
- Secure defaults, all of which the owner can change.
- Legal and fair to others: every license is honored, and every project we build on is credited.

### Guiding principle: a responsive UI

These are guidelines, not hard rules, and they are reviewed once the UI is well underway (see Decisions).

- Measure the delay from input to screen. Track it and report it in CI, and make it a hard gate only once the measurements are stable. Aim for less than one frame of system delay on top of the display, and under 200 ms to open any first-party app.
- Lumen composites as late as safely possible before each refresh, adapting to recent frame times, and shows a full-screen app directly when it can.
- The UI kit never blocks its UI thread on disk, network or IPC. System UI is native Rust with no garbage collector and no web views.
- Animations are short, can be interrupted and can be turned off.
- Local UI never waits on the network.
- Interactive threads get scheduler priority, and the CPU and GPU boost on input and then drop back to save battery.

### Defaults chosen where the ask left room

| Decision | Choice | Reason |
| --- | --- | --- |
| First architecture | x86-64, UEFI and legacy BIOS | One working system first; AArch64 is Phase 34, and CI compiles the arch layer for it from Phase 1 |
| Native filesystem | Btrfs, implemented from the on-disk format in Rust | Spec 7 first recommendation; Linux btrfs-progs becomes a free test oracle |
| Bootloader | GRUB 2 loading Radix through Multiboot2 | Spec 12.3; one entry path covers UEFI and BIOS |
| Kernel language | Rust no\_std, pinned nightly toolchain | Spec 12.1 |
| C compatibility | mlibc with a Radix sysdeps layer | Designed for porting to new kernels, MIT licensed, already runs GCC, coreutils and Wayland ports on other hobby kernels |
| Build host | Linux x86-64, or any OS through the provided Podman/Docker build container | Image creation needs no root |
| Reference hardware | QEMU/KVM (q35 + OVMF), then two reference machines: the Dell Inspiron 15 5570 laptop (Core i7-8550U) and the Framework Desktop (Ryzen AI Max+ 395, 128 GB), with every driver for the Framework planned | Chosen; see the Reference machines section |
| Networking in 1.0 | Wired Ethernet up to 10 GbE, USB Ethernet, and WireGuard | A server needs wired networking; Wi-Fi drivers and the WPA supplicant are their own large effort, in Phase 24 |
| Graphics in 1.0 | Text console on the UEFI GOP framebuffer or a serial line; administration through the web console | 1.0 is a headless server release. Lumen follows in Phase 17 and native GPU drivers in Phase 24 |
| License | Apache-2.0 OR MIT for all Dukovo code; ported tools keep their own licenses | Matches the Rust crates we reuse; GPL tools ship as separate programs |

### Scope of 1.0

- **In:** Radix kernel, Spark init with supervised services, resource limits and timers, Nukesh with server-grade scripting, the administration command set, bundles and service bundles, users and login, logging with remote forwarding, IPv4/IPv6 networking with DHCP and DNS up to 10 GbE, WireGuard, the packet filter, Btrfs (with RAID1 and RAID10 across disks), FAT and exFAT, swap, USB storage and input, atomic base-system updates with rollback and optional unattended security updates, snapshots and backups, the installer with the `rdisk` partitioner and a server profile, USB install media, cloud and VM images with first-boot setup, the Dukovo Admin web console, the 1.0 services (nginx, certd, PostgreSQL, SQLite, Valkey, PHP-FPM, the metrics exporter), a hardware watchdog, a screen reader with speech and braille, laptop and desktop power basics (power button, battery status, CPU frequency, thermal shutdown), SSH (off by default), elevate for administration, the software catalog and the unified update command, a recovery boot entry, a man page for every shipped command and file format, and the `ob` build tool.
- **Out until after 1.0:** the Lumen compositor and anything graphical (Phase 17 onward), Wi-Fi, Bluetooth, native GPU drivers, audio beyond screen-reader speech, printing, suspend, containers and Podman (Phase 20), Go-based software (Phase 18), secure and measured boot, disk encryption, virtualization host, confidential computing, AArch64 and RISC-V.

## Spec review: gaps and decisions

The spec (v1.4) is buildable as written once eleven gaps are settled. Each row below states the decision this plan builds on; all are recorded as ADRs in `docs/adr/` during Phase 0 so they can be revisited.

| # | Spec reference | Gap or conflict | Decision in this plan |
| --- | --- | --- | --- |
| 1 | 1.2 vs 7.x | 1.2 says "kernel only", yet sections 7.1 to 7.16 specify a full userspace | Treat the spec as the Dukovo system spec; Radix is the kernel inside it |
| 2 | 16.2 | Two steps are numbered 7, and Spark (step 7) must mount filesystems before the VFS exists (the other step 7) | Phase 6 builds a minimal VFS with a read-only boot archive before Spark; the full VFS follows in Phase 7 |
| 3 | 7.1 vs 7.6 | `/system` is immutable and replaced on update, but `/system/settings` and `/system/state` must persist and be writable | `/system` is a read-only Btrfs subvolume per update slot; `/system/settings` and `/system/state` are separate writable subvolumes mounted over it. Updates ship defaults in `/system/resources/default-settings` and add new files without overwriting edited ones |
| 4 | 2.1 vs 7.8 | Network stacks are to be compartmentalized, yet the kernel stack must remain the default path | The network stack runs in the kernel as a compartment: a `#![forbid(unsafe_code)]` crate with its own capability set, restartable on panic |
| 5 | 7.2 | `chmod` and `chown` must exist but there are no UID/GID permission bits | Files carry an owner principal and a small rights template as metadata. `chown` needs an admin capability; `chmod` edits the template that governs capabilities derived for other principals. Detailed in `radix-fs-rights(7)` |
| 6 | 7.2, 7.3 | Job control and supervision need POSIX-style signals, which the spec never names | Signals are a kernel notification object. Sending one requires a process capability with the `signal` right; mlibc maps POSIX signal APIs onto it |
| 7 | 4.1, 13 | Porting C tools needs `fork()`; capabilities make naive fork unsafe | `fork()` clones the address space copy-on-write and copies only capabilities marked inheritable. `posix_spawn` is the preferred, faster path |
| 8 | 9 vs 7.6 | Section 9 asks for live patching; 7.6 says it is not needed initially | Live patching is post-1.0. Updates reboot into the new slot |
| 9 | 8, 12.3 | Secure boot with GRUB needs a signed shim, which an independent project cannot get signed by Microsoft | Post-1.0: users enroll the project's own keys (sbctl or MOK). 1.0 boots with Secure Boot disabled |
| 10 | 7.8 | Wi-Fi is not mentioned anywhere | Wired and USB Ethernet in 1.0 |
| 11 | 7.16 | Notifications are "D-Bus-style" without naming an interface | A small `notifyd` service speaking a capability-based IPC protocol; Lumen and the console both subscribe |

## System architecture

Radix splits into a small trusted core and a ring of restartable compartments; everything above the syscall line is ordinary capability-restricted userspace.

Only the trusted core touches hardware; everything above holds capabilities.

```text
+----------------------------------------------------------------------+
| Userspace (capability-restricted processes)                          |
|   Bundles (self-contained apps)    admind (web console)              |
|   Nukesh (shell and utilities)     Spark + services (netd, logd,     |
|                                    authd)                            |
|   Runtimes: mlibc, Rust std, libradix                                |
+ - - - - - - - - - - - - - - - - - - - - - - - - - - - - - - - - - - -+
|   about a dozen syscalls: messages on capabilities                   |
+ - - - - - - - - - - - - - - - - - - - - - - - - - - - - - - - - - - -+
| Kernel compartments (no unsafe code, restartable)                    |
|   VFS + filesystems (Btrfs, FAT, exFAT)                              |
|   Network stack (TCP/IP, NIC drivers)                                |
|   Storage drivers (NVMe, AHCI, virtio)                               |
|   Devices (USB, TTY, display)                                        |
+----------------------------------------------------------------------+
| Trusted computing base (audited unsafe code, model-checked)          |
|   cap (capabilities)   mm (memory)        sched (scheduler)          |
|   ipc (IPC, rings)     arch (x86-64 HAL)  lib/hw (MMIO, DMA)         |
+----------------------------------------------------------------------+
                                  |
                                  | lib/hw only
                                  v
          Hardware: CPUs, memory, IOMMU, PCIe and USB devices
```

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
| Init and services | Spark, logd, logtrim, netd (DHCPv4/v6 client and server, static config), resolved, timed, authd, consoled, volumed, inputd, notifyd, sshd, updated; for the server platform `admind` (web console and its API), `certd` (ACME certificates) and `metricsd` (Prometheus-format exporter) | New, Rust |
| Shell | Nukesh, using reedline for line editing; also `/bin/sh` in its POSIX mode | New, Rust; reedline MIT |
| Core utilities | uutils coreutils, findutils and diffutils | Rust, MIT |
| Ported tools | GNU grep and sed, one-true-awk, less, libarchive (bsdtar, bsdunzip), gzip, bzip2, xz, zstd, lz4, 7-Zip (7zz, built without its non-free unRAR code), Info-ZIP zip, curl, nano with ncurses, mandoc, plus the ported tools in the administration command set below | GPL, BSD, MIT and ISC; shipped as separate programs |
| Radix-native tools | ps, top, kill, pgrep, pkill, who, w, uptime, free, vmstat, df, ip, ss, ping, clear, which, chmod, chown, bundle, user, snap, sysupdate, rdisk, elevate, update, backup, shutdown, reboot, poweroff, service, plus the native tools in the administration command set below | New, Rust |
| Network libraries | smoltcp as the base of the in-kernel stack, hickory-dns for the resolver (DNS-over-TLS and HTTPS), rustls with the Mozilla CA bundle, russh for the SSH server and client, boringtun for WireGuard, instant-acme for certificates | 0BSD, MIT/Apache, BSD-3 |
| Web console | `admind` on axum, tokio and rustls with a plain HTML, CSS and TypeScript front end (or Rust compiled to Wasm with Leptos; settled by ADR in Phase 16) | MIT, Apache-2.0 |
| Hardware discovery | Own `radix-bootinfo` and `radix-acpi` parsers for the Multiboot2 boot information and static ACPI tables (ADR 0013); the AML interpreter (rust-osdev `aml` crate, or uACPI in a compartment) is chosen when first needed | New code; MIT/Apache or MIT |
| Graphics (1.1, Phase 17) | Lumen built on Smithay with the pixman software renderer, wayland-rs, libxkbcommon and xkeyboard-config | MIT |
| Boot | GRUB 2, built from pinned source | GPL-3.0 |
| App runtime (post-1.0, Phase 22) | Wasmtime running WebAssembly components (WASI 0.2, Component Model), compiled ahead of time with Cranelift when a bundle is installed | Rust, Bytecode Alliance; Apache-2.0 WITH LLVM-exception |

nano is an addition to the spec's utility list: a console-first system needs an editor for its configuration files. Windows-style (CUA) shortcuts are the system default everywhere: Ctrl+C, X and V for copy, cut and paste, Ctrl+Z and Y for undo and redo, Ctrl+S save, Ctrl+F find, Ctrl+A select all. nano ships with its modern bindings (nano 8 or later) switched on in the system nanorc. In terminals, Ctrl+C copies when text is selected and otherwise interrupts, as in Windows Terminal; Ctrl+Z keeps its job-control meaning at the Nukesh prompt. The default editor is chosen through EDITOR and VISUAL (system default in /system/settings, per-user override) behind a neutral editor command. POSIX requires vi, and server admins expect it, so nvi (BSD-3-Clause) ships as `vi`; Vim and micro come from the catalog.

### Administration command set

A server is run from the shell, so Dukovo ships the commands an administrator expects on Ubuntu Server or Fedora Server, each with a man page. Where a familiar Linux tool depends on Linux internals, a native Rust tool takes its name and common options. Thin front ends for muscle memory (`systemctl`, `journalctl`, `sudo`, `crontab`, `hostnamectl`, `timedatectl`) cover their everyday verbs, map them onto the native tools, and name the native command when a verb is not supported.

| Area | Commands | Source and license | Phase |
| --- | --- | --- | --- |
| Shell and sessions | Nukesh (also `/bin/sh`), tmux, script | New, Rust; tmux ISC with libevent BSD-3-Clause | 9 |
| Text and data | vi (nvi), jq, file, bc, patch, xxd, hexdump, column, tree | nvi BSD-3-Clause; jq MIT; file BSD-2-Clause; bc by Gavin Howard BSD-2-Clause; OpenBSD patch BSD; the rest new Rust | 9 |
| Processes and debugging | watch, renice, ionice, taskset, nohup, timeout, lsof (open handles and held capabilities per process), rtrace (traces a process's system calls and capability messages, as strace does), dmesg, pidstat | uutils for nohup and timeout; the rest new Rust | 9 |
| Storage | lsblk, blkid, mount, umount, findmnt, fstrim, iostat, btrfs (device add and remove, balance, scrub, quotas, RAID conversion), diskhealth (SMART and NVMe health logs), nvme, quota | New, Rust; output checked against Linux btrfs-progs in CI | 10 |
| Hardware | lspci, lsusb, lscpu, lsmem, sensors, hwinfo (DMI and firmware details), watchdog | New, Rust; PCI and USB ID databases (BSD-3-Clause or GPL-2.0, data files) | 11 |
| Network | dig and host, traceroute, mtr, nc, tcpdump, openssl, nic (link speed, offloads and statistics, as ethtool), whois, iperf3 | hickory-dns MIT/Apache; mtr GPL-2.0 as a separate program; OpenBSD netcat BSD; tcpdump and libpcap BSD-3-Clause; OpenSSL Apache-2.0; iperf3 BSD-3-Clause; the rest new Rust | 12 |
| Remote access and transfer | ssh, scp, sftp, ssh-keygen, ssh-agent, ssh-copy-id, rsync, curl, wget | russh Apache-2.0; rsync and wget GPL-3.0 as separate programs; curl MIT-style | 12, 13 |
| Users and access | passwd, id, groups, who, w, last, lastlog, elevate, and `sudo` as a front end to elevate | New, Rust | 13 |
| Services, logs and scheduling | service, log (query, follow and filter logd, as journalctl does), logger, crontab and at (both write Spark timers), sendmail (a small relay so jobs can mail alerts, built on lettre, MIT), and the systemctl, journalctl, hostnamectl and timedatectl front ends | New, Rust | 16 |
| Health and automation | dukovo-check (security and health report), admin (command-line client for the admind API) | New, Rust | 16 |

Bigger or GPL tools that some admins want, such as GNU bash, zsh, fish, Vim, Perl, GnuPG and GNU make, come from the catalog as separate bundles, never into /system.

## Repository, toolchain and the `ob` build tool

Everything lives in one repository and is built by one command, `./ob`, which needs no root access except for writing a USB stick.

### Repository layout

```
dukovo/
├── ob                    # build entry point (shell wrapper around tools/xtask)
├── dukovo.toml          # build configuration: arch, profile, features, image options
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
│   ├── spark/            # Spark, the init
│   ├── programs/         # Rust programs on std: Nukesh, systools (the Radix-native tools)
│   ├── bundle-runtime/
│   ├── services/         # logd, netd, resolved, timed, authd, volumed, ...
│   ├── installer/        # dukovo-install, rdisk, mkfs tools, rgrub-setup
│   ├── sysupdate/
│   ├── admin/            # admind, the Dukovo Admin web console
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
- **Host tools built from pinned source:** GRUB 2, and Dukovo's own `rdisk`, `mkfs-btrfs`, `mkfs-fat` and `rgrub-setup`, which are compiled for Linux too and used to assemble images. The image pipeline dogfoods the installer's tools.

### The `ob` command

| Command | What it does | Output |
| --- | --- | --- |
| `./ob setup` | Installs the pinned Rust toolchain and checks host packages | Ready build environment |
| `./ob doctor` | Lists anything missing, with the exact install command for Debian, Ubuntu, Fedora or Arch | Report |
| `./ob build` | Builds kernel, userspace, ports and docs incrementally | `out/sysroot/` |
| `./ob image` | Builds the USB installer image (hybrid GPT, boots UEFI and BIOS) | `out/dukovo-<version>-x86_64.img` |
| `./ob image --iso` | Same system as a hybrid ISO for VMs and optical media | `out/dukovo-<version>-x86_64.iso` |
| `./ob run` | Boots the image in QEMU with KVM, OVMF and a serial console | Running VM |
| `./ob run --install` | Boots the installer with a blank 32 GiB virtual disk attached | Running VM |
| `./ob write-usb` | Lists removable drives with model and size, asks for typed confirmation, writes and verifies the image | Bootable USB stick |
| `./ob test` | Unit tests, then QEMU boot and install tests | Test report |
| `./ob docs` | Builds man pages and the handbook; `--check` fails on any command without a man page | `out/docs/` |
| `./ob clean` | Removes build output, keeping the source cache |  |
| `./ob lint` | Runs clippy, cargo-deny and the doc checks | Lint report |
| `./ob fmt` | Formats all Rust code; `--check` fails on any difference |  |
| `./ob test kani` | Runs the Kani model-checking proofs | Proof report |
| `./ob release` | Builds and signs the release images and the GPL and LGPL source tarballs, then publishes them | Signed images and source tarballs |

Every command accepts `--container`, which runs it inside the pinned Podman or Docker build image, so a macOS or Windows host can build too. `./ob write-usb` refuses non-removable disks unless given `--force`, and users who prefer can write the image with balenaEtcher, GNOME Disks, Rufus (DD mode) or `dd`.

`dukovo.toml` holds the few choices a builder makes:

```toml
[build]
arch = "x86_64"
profile = "release"        # or "debug"

[features]
lumen = false              # Lumen joins the default image in 1.1
admin-console = true       # the Dukovo Admin web console
userspace-e1000 = true

[image]
size = "4GiB"
live-user = "live"
```

Host requirements: a Linux x86-64 machine with about 40 GB free disk space, QEMU, OVMF, xorriso, mtools, Clang/LLD, btrfs-progs and dosfstools; `./ob doctor` names anything missing.

## Phased roadmap to 1.0

Seventeen phases lead to 1.0, a headless server release, with three milestones on the way: a console system after Phase 9, the spec's "first usable system" (16.7) once Phases 10 to 13 are done, and Release 1.0 once Phases 14 to 16 are done. Phases 0 to 9 are strictly sequential; after Phase 9 storage, USB, networking and users can run in parallel. The order was changed to server first on 2026-10-23: Phase 16 is now the server platform and web console, and the Lumen compositor moved to Phase 17, the first 1.x release.

Phases 10 to 13 run side by side once the console system works; the drawing shows the simplest schedule. Lumen design work can start any time, because it does not depend on code.

Sub-phases (decided on 2026-10-23): large phases are split into lettered sub-phases such as 10a and 10b, ordered by dependency so the base comes first and features build on top. Each sub-phase is a self-contained step with its own pull requests, CI, check and boot video, and carries its own size. A phase is done when all its sub-phases are and its exit check passes. The go-ahead to start work is still given per phase and covers all its sub-phases. Inside a phase the sub-phases run in order, while Phases 10 to 13 can still run side by side.

Every phase has the same definition of done: code merged with CI green, tests added to `./ob test`, a man page for every new command, file format and interface, the handbook updated, and an ADR for any decision the spec left open. Audit checkpoints: a code and performance audit runs after Phase 5 (once the yield and IPC performance fix lands, before Phase 6), after Phase 9, after Phase 12 and before Phase 14. Each covers every unsafe block, lock ordering, capability checks and input validation on every system call, fuzzing, profiling under KVM with benchmark budgets enforced in CI, and consistency between code, docs, ADRs and this plan. Performance gate: spec section 11 asks for performance competitive with or better than contemporary production kernels, so every kernel microbenchmark (system call, context switch, IPC, page fault, TLB shootdown, memory bandwidth and multi-core scaling) must match or beat Linux measured in the same CI run on the same machine before the next phase starts, and CI fails on any regression past that ratio.

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
- Resource domains (cgroup-style CPU weight and memory limit objects) as first-class kernel objects (spec 4.1). An out-of-memory policy uses them: when memory truly runs out, user apps are stopped first and system services last, and Spark restarts what it supervises.
- **Exit:** fairness, real-time preemption and priority-inversion test suites pass, and lock-order checking catches a seeded deadlock.

**Status:** implemented. Eight boot tests run on 4 CPUs with the lock checker on: `sched-fair` measures shares by nice level and by domain CPU weight, `sched-rt` checks that wake-ups preempt by priority and that a timer wakes a FIFO thread on a busy CPU within a millisecond, `sched-pi` ends a priority inversion through one mutex and through a chain of two, `sched-balance` spreads threads from one CPU over two NUMA nodes, `lockdep` catches seeded AB-BA deadlocks between spin locks and between mutexes and a mutex taken under a spin lock, `rcu` checks that readers on three CPUs never see an object freed too early, `fpu` checks the vector registers through thousands of switches, and `oom` checks domain limits and that running out of memory stops an app before a service, which its supervisor restarts, and never a `system` domain. Every other boot test fails on any lock checker report. TLC checks a TLA+ model of blocking, waking and the switch handoff, liveness included, and Kani proves the weight table, CPU sets and the real-time pick. The semi-formal argument is in `docs/design/scheduler.md`, and `radix-scheduling(7)` describes the scheduler. Changes from this plan: no lock is held across a context switch, which hands over through an `on_cpu` word in each context; domain CPU weights scale threads' weights in each CPU's fair queue instead of nesting a queue per domain; the kernel's workers run in the fair class because mutex handoffs favour real-time waiters (ADR 0027); the lock checker is on by default in debug builds and switchable with the `lockdep` flag (ADR 0028); domains are capability-named kernel objects, and until Spark exists a kernel `supervisor` thread restarts stopped domains (ADR 0029); the fair queue's arithmetic is argued algebraically and checked by property tests rather than Kani (ADR 0030). Decisions made in this phase: ADRs 0027 to 0030.

### Phase 5: System calls and IPC (L)

- `SYSCALL` entry and exit with safe user-copy primitives; the dozen core syscalls; a random capability (getrandom) that every process receives at start, backed by the kernel CSPRNG, for ASLR, TLS, SSH keys, password salts and signing.
- `ridlc` IDL compiler generating Rust, C and man(2) output.
- Endpoints with a register-only fast path, notifications, shared-memory channels, `radix-ring`.
- Benchmark harness for syscall and IPC round trips, recording Linux numbers on the same machine as a baseline.
- **Exit:** user-mode test programs ping-pong over IPC and rings, and CI publishes the benchmark numbers on every merge.

**Status:** implemented. Programs run in user mode and reach the kernel through twelve system calls, defined with every object's methods and the kernel's errors in `abi/*.ridl`; `ridlc` generates the Rust bindings, C headers and 23 man(2) pages from them. Two boot tests run user-mode programs on 2 CPUs with the lock checker on: in `user` a probe calls every system call and method with good and bad arguments, including a check that registers are preserved and failed calls return nothing, and eight kinds of fault each end only their program; in `ipc` five pairs of programs ping-pong over an endpoint, pass capabilities in messages, signal notifications, talk through a shared-memory channel and exchange `msg_ring` completions between two rings, on one CPU and across two. Both check that the capability store holds nothing more afterwards. TLC checks a TLA+ model of calls, replies, cancellation and closing, liveness included, and five Kani harnesses prove the info words, the user-range checks and the ring accounting against anything a program writes. `ob bench` measures every operation of the performance gate (system calls, a capability method, context switches, IPC round trips on one CPU and across two, page faults, TLB shootdowns, memory bandwidth, and scaling over both CPUs) next to the closest Linux operations, run by Linux booted in the same kind of virtual machine; CI's `bench` job fails when any of them is more than 5 % slower than Linux, and publishes the report in its summary on every pull request and merge. The semi-formal argument is in `docs/design/ipc.md`, and `radix-ipc(7)` describes the mechanisms. Changes from this plan: the random capability is an object with `read` and `words` methods in slot 4 of every program, and rings have a `random` operation too (ADR 0031); channels are a library over a shared frame and two notifications rather than a kernel object (ADR 0032); until Phase 6's loader, the kernel starts the test programs from a `usertests` boot module, and the store gains `random`, `ring` and `kernel_log` objects (ADR 0036); the benchmark first only reported numbers against the host's Linux (ADR 0035), and now fails CI when Radix is slower than Linux in the same kind of virtual machine (ADR 0039); meeting that took a timer written only to fire earlier and halt polling for idle virtual CPUs (ADR 0037), channel wake-ups only for an end that said it may sleep (ADR 0038), link-time optimisation, hot paths without locked instructions, string moves or 128-bit division, and KVM's paravirtual EOI (ADR 0040), and a yield that gives up the rest of its slice as Linux's does since 6.18, with a TLB shootdown's interrupt ended before the flush (ADR 0041); every row now passes, most of them faster than Linux, and per-CPU random generators that work a batch at a time and lighter system calls and wake-ups widened the thinnest margins (ADR 0042). The user-mode tests found two bugs of earlier phases, fixed here: a TLB shootdown with no other CPU to ask still took a request number, so its CPU skipped the next real request under that number and that request's initiator panicked waiting for it; and idle CPUs halted with HLT claimed to poll for wake-ups, so a wake-up from another CPU waited for the next timer tick. An audit closed the phase (see Audits under Testing). It added the `fuzz` boot test, in which groups of programs make random system calls, methods and ring entries from logged seeds, and every error must be one the call's documentation lists (ADR 0043). It found and fixed a failed reply that left its caller waiting for good, a thread exit that could deadlock its CPU on the lock checker's per-thread lock, errors missing from the documentation of eleven of the twelve system calls, an unsound `RcuCell::replace`, an endpoint queue that took quadratic time, a retype count checked only after the heap was asked for room for it, interrupt and idle instructions the compiler was free to move memory accesses across, page tables reached through mutable references while the MMU writes them too (their entries are now atomics), a CPU that started late after a failed start-up, which would have run on the next CPU's stack, and boot reservations beyond the table's room, dropped without a word. Retyping still zeroes frames under the store's lock, which is recorded in `docs/design/capabilities.md` with its fix, due before Phase 6's init retypes large amounts of memory. Objects are now found through a per-CPU cache with no shared lock, which a new benchmark of endpoint calls on both CPUs at once holds to Linux (ADR 0044), and CI's `instructions` job holds every one-CPU operation to an instruction budget counted by QEMU (ADR 0045). The gate itself failed on main on a different row each time, from the shared runners' noise, so it now measures Radix and Linux three times in turn and counts each operation's fastest run (ADR 0046). Decisions made in this phase: ADRs 0031 to 0046.

### Phase 6: First userspace (M)

- ELF64 loader (static and position-independent), user ASLR, process objects.
- Boot archive (`bootfs`, a tar loaded by GRUB as a module) with a minimal read-only VFS.
- `radix-rt` runtime and Spark v0: start one program, reap children, log over the console.
- **Exit:** Spark starts from the boot archive, launches a test program that prints to the console, and reaps it when it exits.

**Status:** implemented. A process is a capability-named kernel object with a capability space, an address space, a hidden space for what its maker keeps for it, and its threads; whoever holds its capability with the control right builds it from outside with ten methods, and it ends with its last thread, a fault, `exit`, its last capability or the process it was made for, with an exit code that `notify_exit` announces, until deleting its last capability reaps it (ADR 0047, `radix-process(2)`). Every object a program makes is charged to its process's resource domain. Programs are static position-independent executables, which the kernel's loader and radix-rt's `spawn` check whole and place at random addresses: 28 bits of entropy for the image and 24 for the stack (ADR 0048). `ob build` writes the boot archive, a ustar file with Spark and the programs it starts, which GRUB loads as a module and which is checked whole before use (ADR 0049); at the end of boot the kernel starts `init=` (`/system/bin/spark` by default) from it with the words after `--` as its arguments, and panics if it ever ends (ADR 0050). radix-rt gives native programs a heap, their arguments, the boot archive as a read-only file system and `spawn` (ADR 0051). Spark v0 starts `/system/tests/hello` with its arguments, waits for it, reaps it and checks that all of its memory came back, which the new `spark` boot test checks under UEFI, together with both programs' random placement. The `user` boot test's probe checks every process method, and the `fuzz` boot test's programs now make processes and call every process method at random, with the store and the registry checked afterwards. The design and its semi-formal argument are in `docs/design/processes.md`, and `radix-processes(7)` and `spark(8)` describe it. Changes from this plan: a process made with `create` ends with the process it was made for, so nothing a program makes outlives it, even a process that holds the only capability to itself (ADR 0047); the self-tests' `usertests` image stays a fixed executable so that its addresses stay put; and Spark's role as the supervisor of resource domains and their restarts moves to Phase 9, when there are services to supervise, so the kernel's stand-in keeps applying the out-of-memory policy until then. Retyping now zeroes frames outside the capability store's lock, as the Phase 5 audit asked before init retypes large amounts of memory. Decisions made in this phase: ADRs 0047 to 0051.

### Phase 7: VFS and basic storage (L)

- Full VFS with capability-based handles, mount table and page cache.
- tmpfs, devfs, procfs (plain-text files), sysfs; block layer driven by rings; virtio-blk; FAT12/16/32 read-write.
- The spec 7.1 directory layout created by Spark at boot.
- **Exit:** Spark mounts a FAT virtual disk, files written are read back after reboot, and `/proc` lists running processes.

**Status:** implemented. Programs reach files only through directory capabilities: every path is resolved beneath the directory it comes with, `..` never climbs above it and absolute paths are refused, as with WASI and Capsicum (ADR 0054, `radix-directory(2)`, `radix-file(2)`). The VFS joins filesystems in a mount table; each file's data lives in a page cache indexed like Linux's XArray, each device's metadata in a block cache, and a writeback thread syncs every filesystem every five seconds and gives clean pages back when memory runs low (ADR 0055). The kernel scans PCI through ECAM, drives virtio block devices through the modern transport with MSI-X interrupts (ADRs 0052 and 0053), reads MBR and GPT partition tables and names disks as Linux does. `radix-fat` is our own FAT12/16/32 with long names, read-write, from Microsoft's specification (ADR 0056); tmpfs, `/dev`, `/proc` and `/sys` complete the filesystems, with `/proc` as plain text and `/sys` in Linux's layout (ADR 0057). Spark v1 builds the spec 7.1 hierarchy at every boot and mounts each disk's FAT filesystem at `/volumes/<name>`. Every storage crate but the one that touches device registers forbids `unsafe` code. The new `storage` boot test gives QEMU four disks (FAT12, FAT16, FAT32 and one partitioned), has a program write files with short and long names, many clusters, many entries, renames, removals, appends and cuts to each, checks them on the host with `fsck.fat` and mtools, writes one more from the host, reboots, and checks that every byte reads back, and that `/proc` lists the running processes and `/sys` the disks; it passes, which is the exit criterion. The `fuzz` boot test's programs now call every directory and file method at random in a tmpfs of their own, and TLC checks a TLA+ model of cluster reuse between files and directories, which finds the stale-sector bug the design avoids when its discard is taken out. `ob bench` measures opening, `stat`, and 4 KiB reads and writes of a file, next to Linux's `openat`, `fstatat`, `pread` and `pwrite` on tmpfs, with instruction budgets; the first measurements had opening, `stat` and reads behind Linux, and a path walk that allocates nothing, a filter of mount points in each mount and one capability lookup per call bring all four within the gate (ADR 0059). The design and its semi-formal arguments are in `docs/design/storage.md`, and `radix-files(7)`, `radix-proc(5)`, `radix-sysfs(5)` and `spark(8)` describe it. Changes from this plan: block requests block their thread, with up to 64 in flight per disk, and rings carry file reads, writes and syncs rather than driving the block layer themselves; devices use MSI or MSI-X only, so no AML interpreter is needed for interrupts (ADR 0052). A mount whose function took 6 KiB of a 16 KiB kernel stack ended in a double fault while this phase was built, so every release build now refuses a kernel with any stack frame over 2 KiB, outside a short list of boot-time functions (ADR 0058). The lock checker gained subclasses for mutexes, for tmpfs's nested directory locks, and its graph searches now use bit sets. Decisions made in this phase: ADRs 0052 to 0059.

### Phase 8: POSIX layer and process model (XL)

- mlibc Radix sysdeps; Rust `std` port; Clang wrappers; ports system.
- `posix_spawn`, `fork` (copy-on-write, inheritable capabilities only), `exec`, `waitpid`.
- Signals, process groups, sessions, controlling terminals; TTY line discipline and PTYs; pipes; `poll` and `select` on top of rings; clocks and timers.
- Static linking for system programs; mlibc's dynamic loader for bundles that ship private libraries.
- **Exit:** a ported C program, a Rust `std` program and the relevant libc-test cases pass, and fork, exec, signal and pipe tests pass.

**Status:** implemented. The kernel keeps what POSIX needs across processes: anonymous memory made when touched and shared copy-on-write after `fork` (ADR 0060), one process tree with groups and sessions (ADR 0061), signals with Linux's numbers and frame delivered on the way back to user mode (ADR 0062), `fork` of the capabilities that may be copied and `exec` of a program from a file into a fresh address space, with interpreters and `#!` scripts (ADR 0063), pipes and `poll` (ADR 0065), pseudo-terminals with POSIX's line discipline and job control (ADR 0066), clocks, interval timers and POSIX timers (ADR 0067), futexes, scheduling and resource limits by Linux's rules for a process without privileges (ADR 0074), advisory file locks of all three kinds (ADR 0075), and file times that programs set and reads never change, with `st_dev` naming a file's mount (ADR 0076); the rules live in the host-tested `radix-posix` and `radix-tty` crates, and `radix-signal(2)` and `radix-terminal(2)` are new. A file descriptor is a capability slot, and the C library resolves paths against its working directory to paths beneath the root it holds (ADR 0064). mlibc 7.0.0 is the C library, with our own Radix system layer and Linux's ABI numbers (ADR 0068), and 22 patches, among them musl 1.2.5's exact floating-point conversions, `strftime`, calendar code, `scanf` and Internet address conversions (ADR 0072); programs start in C.UTF-8, compiled from glibc's locale data (ADR 0073). Software from elsewhere is built by the new ports system with the host's Clang 20 wrapped for Radix (ADR 0069), and Rust's standard library is ported to an `x86_64-unknown-radix` target, with the `libc` crate checked against mlibc's headers after every build (ADR 0070). System programs stay static; mlibc's dynamic linker is `/system/lib/ld.so`, for bundles (ADR 0071). The new `posix` boot test runs, on a pseudo-terminal Spark gives it, 15 C test programs (fork, exec, signals, pipes, poll, terminals, job control, time, files, locks, threads, spawn, limits, and 1,428 `printf`, `strtod`, `scanf` and `strftime` answers generated with glibc), a dynamically linked program with a private library and a plugin, `std-test` on Rust's standard library, every one of mlibc's own 132 tests that apply to Radix, 110 tests of libc-test, the musl project's tests of C libraries (the ones left out wait for a shell, users or later phases, each with its reason in `ports/libc-test/build.sh`), and Lua 5.4.7's own test suite, the first ported program; all 20 pass, which is the exit criterion. The design and its semi-formal arguments are in `docs/design/posix.md`, and `radix-posix(7)` describes the layer. Changes from this plan: ports pin a git commit rather than an archive's SHA-256, and are kept in `target/ports`, and C is compiled by the host's Clang 20 rather than the LLVM inside Rust's toolchain, which ships no C front end (ADR 0069); `poll` is one method of the process rather than ring operations, since a ring completes operations on one capability each (ADR 0065); `posix_spawn` is the C library's, on `fork` and `exec`, while native programs can start a POSIX program in a process they built with `exec` (ADR 0063). The tests found and fixed a timer race in which setting a timer while the timer thread took the process off its list lost the timer for good, and a race in which two session leaders could both take one terminal. 26 of libc-test's tests failed at first, from bugs in mlibc that patches 0013 to 0022 fix, each meant for upstream: among them a `raise` that signalled the whole process rather than the calling thread, `scanf` fields that ignored their width, and mutexes that made a system call on every unlock; tmpfs also gained Linux's limit on files, so that empty files cannot use up the kernel's heap. To keep file operations on par with Linux on newer CPUs, each CPU's capability and object caches became one, which empties only when something it holds goes (ADR 0077). What is left for later, from shared futexes to POSIX rows in `ob bench`, is listed in the design note's known limits. Decisions made in this phase: ADRs 0060 to 0077.

### Phase 9: Nukesh, core utilities and man pages (XL)

- Nukesh: parser, pipes, all redirections, here-documents, `;` `&&` `||`, grouping, subshells, variables and export, globbing, `if`/`for`/`while`/`case`, functions, shebang scripts, job control (`&`, `fg`, `bg`, `jobs`, Ctrl-Z, Ctrl-C), history and completion.
- Server-grade Nukesh: a POSIX mode that serves as `/bin/sh` and passes a POSIX shell conformance suite; the bash features real admin scripts use (`[[ ]]`, indexed and associative arrays, `local`, `$'...'`, here-strings, brace expansion, process substitution, `mapfile`, `printf -v`, `set -euo pipefail`, `trap` on EXIT, ERR and signals, `getopts`, `read -r -t`, `wait -n`); `nukesh -n` syntax checks, `-x` tracing with file and line numbers, and errors that name the line; history with timestamps, safe across concurrent sessions; completion for every shipped command, generated from its argument definitions; a restricted mode for limited accounts; hang-up handling (`disown`, `nohup`, SIGHUP to jobs) so a dropped SSH session behaves as on Linux; and clean output on a serial line or dumb terminal (`TERM=dumb`, `NO_COLOR`). The parser and expander are fuzzed, and a corpus of a few hundred real-world scripts (distribution install scripts, container entrypoints, CI scripts) must give the same results as under bash or dash.
- uutils coreutils, findutils and diffutils ports; GNU grep and sed; one-true-awk; less; libarchive; gzip, bzip2, xz, zstd, lz4, 7-Zip, zip; nano; mandoc; and the Phase 9 rows of the administration command set (tmux, vi, jq, file, bc, patch, lsof, rtrace and the rest).
- Radix-native tools from the architecture table, plus shutdown, reboot and poweroff; `consoled` with virtual terminals, and the i8042 PS/2 keyboard driver (QEMU's default keyboard, and how the Dell's built-in keyboard is connected).
- Spark supervises the services it starts: each in a resource domain of its own, restarted when it fails, with Spark taking over the out-of-memory policy's supervisor role from the kernel's stand-in, and the kernel interface to make domains and set priorities that this needs (moved here from Phase 6).
- **Exit (milestone: console system):** the machine boots to a Nukesh prompt in QEMU and on the Dell, a scripted suite exercising every utility in spec 7.2 and every Phase 9 administration command passes, the POSIX conformance suite and the script corpus pass, and `man` works for every command in `/system/bin`.

**Status:** implemented, with the Dell's smoke test of Phase 9 (handbook, Supported hardware) still to run. Landed: symbolic links, resolved by the kernel beneath the directory a path starts from, with Spark's `/bin`, `/sbin`, `/usr`, `/etc` and `/var` links into `/system` (ADR 0081); Rust programs from elsewhere build with crates.io crates patched for Radix (ADR 0080); uutils coreutils 0.9.0, as one program with a link for each utility, and uutils findutils 0.9.1 (`find`, `xargs`), with manual pages; and the posix boot test's new `utilities` part, which runs them as scripts do. Two kernel bugs that the new programs exposed are fixed. A read or write between a file and memory not touched yet could deadlock against a page fault on a mapped file, so copies under a file's cache lock no longer wait for a page (ADR 0082). The crash log, in VMs with less than 4 GiB of RAM, was placed over the 30 MiB boot archive and wrote over the programs in it; it now keeps clear of the boot modules. Change from this plan: uutils diffutils 0.5.0 lacks `-r`, `-w` and `-N`, which scripts use, so GNU diffutils comes with GNU grep and sed instead. Nukesh has landed (ADR 0083): written in Rust for Dukovo, it is `/system/bin/nukesh`, `/bin/sh` in its POSIX mode and `rnukesh`, restricted; it has the bash features scripts use, `-n` and `-x` with file and line, a history safe across sessions, programmable completion, job control and hang-up handling, and reedline as its line editor, with a plain reader for `TERM=dumb`. Its scripts' output is checked against bash 5.2 and dash on the build host and again at boot, along with a session typed at a pseudo-terminal. Its parser, arithmetic, patterns and brace expansion are fuzzed, nesting has limits so that deep input is an error and not a crash, and pattern matching takes time that grows with the text, not exponentially. For it, the C library gained `/dev/fd` and `/dev/stdin`, `/dev/stdout` and `/dev/stderr` (ADR 0084), and the kernel socket pairs (ADR 0085), which its line editor needs. The first C programs of the command set have landed too: ncurses 6.6 with 45 terminal descriptions and `tput`, `tset`, `clear`, `tic` and `infocmp`; less; mandoc, so `man` shows the ports' pages; GNU grep, sed, diffutils and patch; the One True Awk; and Gavin Howard's `bc` and `dc`. Ports can now come from release archives pinned by their SHA-256, fetched from their home or a mirror, and autoconf packages build for Radix through one configure wrapper and site file (ADR 0086). GPL programs are allowed among the ports, each a program of its own (ADR 0087). Change from this plan: GNU patch replaces OpenBSD's, whose files carry four licenses, two without SPDX identifiers, and which cannot apply git's renames. Compression and archives have landed as well (ADR 0088): gzip, bzip2, xz, zstd and lz4, each its own project's, with their libraries; libarchive's `tar`, `cpio` and `unzip`, which read and write zip, 7z and ISO 9660 too; Info-ZIP's `zip` with the fixes Debian carries; `file` with its magic database; and `jq`. For them the kernel now loads a boot archive of any size, mlibc's getopt reads option clusters as POSIX has it, and `..` beneath a directory descriptor and `fchdir` work, as the C library notes each directory descriptor's path (ADR 0089). The editors have landed (ADR 0090): GNU nano 9.2 is the system's editor, with the Windows-style key bindings of the system's `nanorc`, and nvi2 is `vi`, `ex` and `view`. With them came LLVM's C++ runtime (libc++, libc++abi and libunwind), with which 7-Zip's `7zz` is built, CMake for the ports that build with it, and the settings files Spark copies into `/etc` at boot, among them `passwd` and `group` with root, the one user until Phase 13. tmux 3.7c has landed on libevent, and for it the kernel gained named `AF_UNIX` stream sockets, which bind a name in the tree and connect through it, and the passing of descriptors over them with `SCM_RIGHTS`; a socket itself cannot be passed, so no collector of cycles is needed (ADR 0091). The Radix-native administration tools have landed as one Rust program, `systools`, with a link by each tool's name (ADR 0096): ps, top, kill, pgrep, pkill, who, w, users, uptime, free, vmstat, df, which, chmod, chown and chgrp; renice, ionice, taskset, lsof, rtrace, dmesg, pidstat, watch and script; and xxd, hexdump, hd, column and tree, each with the options and output of the Linux tool it is named after, checked against it, and a manual page. For them the kernel splits CPU time between user mode and the kernel by the timer's samples and keeps Linux's load averages (ADR 0092), gives threads CPU affinity and processes I/O classes, which its disks serve by (ADR 0093), shows in `/proc` each process's place in the tree, arguments, times, scheduling, memory, I/O, threads and the capabilities it holds with the paths its files were opened by, gives the kernel log through `/dev/kmsg` (ADR 0094), and traces a process's calls through a pipe it writes, which `rtrace` prints as strace does (ADR 0095). Change from this plan: the process tools need no Linux-format `/proc`, since they are Radix's own. A normal boot now reaches a Nukesh prompt on the screen. The kernel drives the i8042 PS/2 keyboard and gives its key events, with Linux's key codes, through `/dev/kbd`, and lets one program take the screen through `/dev/fb0`, keeping its log off it meanwhile (ADR 0097). `consoled` draws six virtual consoles on it, each a terminal for `TERM=linux` on the vte parser with root's shell on a pseudo-terminal, switched with Alt+F1 to Alt+F6 (ADR 0100). Spark v2 supervises services: each `.service` file in `/system/etc/spark/services` names a program, its order, restart policy, CPU weight, memory limit and out-of-memory class; Spark starts each in a resource domain of its own, which programs can now make through capabilities (ADR 0098), copies its output to the log, starts it again after it ends with a growing backoff, and stops the services in order at shutdown (ADR 0101). `shutdown`, `reboot`, `poweroff`, `halt` and `service` ask Spark over its control socket, and Spark ends the machine through a power capability only it holds, which syncs the disks first and powers off through ACPI's S5 (ADR 0099). The new `consoles` boot test types on the screen's shell through QEMU's keyboard, switches consoles, restarts consoled and powers the machine off. Every program in `/system/bin` has its manual page in the image, Dukovo's own among them, indexed with makewhatis so that `man`, `apropos` and `whatis` find a page by each name in its NAME section, and the build fails if a program has none. From those pages the build writes a completion file per program, from which Nukesh completes its options, with their descriptions, their arguments and its operands (ADR 0102). Nukesh is now checked against code others wrote, by `ob test corpus` (ADR 0103): smoosh's POSIX shell tests, which it passes as `sh` where dash and bash do or with a written reason (163 of 186, dash 144, bash 157), and the scripts and test suites of 20 real projects (nvm, shUnit2, rbenv and pyenv with their Bats tests, eight installers, five container image entrypoints, neofetch, dehydrated and Oils' configure), which give the same status and output under Nukesh as under dash or bash, 952 tests in all. Third-party scripts run only in a sandbox with no network, a read-only system and each test as `nobody` in a PID namespace of its own. What it found is fixed: errors are bash's (a script goes on after an arithmetic error or a bad substitution) and dash's as `sh`, indirect expansion, `=~` with `|`, backslashes in replacements, `set -e` in compound commands, and a script with a long function is no longer parsed again for each line. The scripted suite of the exit is the `posix` boot test: its `utilities` and `admin` tests run every program in `/system/bin` as scripts and administrators use them, and its new `commands` test fails if one goes untested. What they found is fixed: uutils' `more` waited for keys with its output on a pipe, so `zmore` hung, `pr -t` padded pages and dropped the last row of a short file, `mkfifo` and `mknod` misreported errors, mlibc's `printf` ended a program on `%Lu`, the filetime crate could not set times on Radix, so `touch`, `cp -p` and `install -p` failed, and a bare `exit` in Nukesh's EXIT trap lost the status from before it. Change from this plan: `bash` is a name of Nukesh too, `/bin/bash`, which sets `BASH_VERSION`, so that scripts written for bash run unchanged.

### Phase 10: Native storage: Btrfs, NVMe, AHCI and swap (XL)

- **10a Disk drivers (M):** AHCI and NVMe drivers, both in 1.0: AHCI first for the Dell's SATA drives, then NVMe for the Framework Desktop's two PCIe 4.0 drives; disks of 4 KiB sectors as well as 512-byte ones. Done when both read and write raw disks in QEMU and AHCI does on the Dell. The Framework serves other work until Dukovo runs containers (Phase 20), so its NVMe drives are checked from then; until then NVMe is checked in QEMU.
- **10b Btrfs read-only (M):** superblock, chunk, root and filesystem trees, CRC32C checksums. Images made by Linux `mkfs.btrfs` must mount in Radix. Done when Radix reads every file on a Linux-made image.
- **10c Btrfs read-write (L):** copy-on-write with transaction commits; subvolumes and snapshots; zstd compression and scrub; the single and DUP profiles on one disk. Space is reserved so deleting files and snapshots always works on a full disk. Linux `btrfs check` runs in CI on every image Radix writes, and power-cut tests kill QEMU at random points and check consistency. Done when the root filesystem is Btrfs and a long stress run passes `btrfs check` clean.
- **10d Swap and other filesystems (M):** swap partitions in the standard swap format; page reclaim and swap-out; exFAT read-write, and ext4 read-write with its journal replayed after a power cut. Done when the system survives memory pressure by swapping.
- **10e Multi-disk and storage tools (M):** RAID1 and RAID10 across several disks for servers, with degraded mounts, device replacement and rebuilds. RAID5 and RAID6 are never offered, because they are unsafe even on Linux. Disk-pull tests detach a disk at random points and check consistency. Disk health: SMART for SATA and the NVMe health log, read by `diskhealth` and reported to logd and the web console. The storage rows of the administration command set (lsblk, blkid, findmnt, fstrim, iostat, the `btrfs` admin command and the rest).
- **Exit:** the root filesystem is Btrfs, a long filesystem stress run passes `btrfs check` clean, a two-disk RAID1 survives losing either disk and rebuilds onto a new one (in QEMU and on the Dell's two SSDs), and the system survives memory pressure by swapping.

### Phase 11: USB, input, platform and removable media (L)

- **11a USB core and input (M):** xHCI, hubs, HID keyboard and mouse, console keymaps, and a console over the xHCI Debug Capability for machines without a serial port. Done when a USB keyboard works alongside the built-in one.
- **11b Removable media (S):** USB mass storage (BOT, then UAS); `volumed` mounts inserted volumes at `/volumes/<label>`, grants access to the logged-in user, and ejects safely.
- **11c Power and thermal (M):** the ACPI power button with clean power-off; ACPI battery and AC status, lid-switch events, and CPU frequency scaling through Intel HWP and AMD CPPC (amd-pstate); the CPU slows at the ACPI passive trip point, and the system shuts down cleanly at the critical one.
- **11d IOMMU and server reliability (M):** IOMMU programming so device DMA is confined to granted buffers: Intel VT-d for the Dell and AMD-Vi for the Framework Desktop. A hardware watchdog (Intel iTCO, the AMD FCH watchdog, and QEMU's i6300esb) that reboots the machine if Spark stops responding; machine-check (MCE) handling and memory error reporting for Intel and AMD, including ECC error counts on machines that have ECC.
- **11e AMD platform and hardware tools (M):** AMD platform support for the Framework Desktop: Zen 5 machine-check banks, k10temp and SMU temperature sensors, the FCH SMBus, AMD GPIO and I2C controllers, and the Framework embedded controller (an open-source Chromium EC fork, BSD-3-Clause) for fan curves, the power LED and the ARGB header. The hardware rows of the administration command set (lspci, lsusb, lscpu, sensors, hwinfo, watchdog).
- **Exit:** a FAT or exFAT USB stick appears under `/volumes` in QEMU, on the Dell and on the Framework, ejecting removes it, a USB keyboard works alongside the built-in one, the power button shuts both machines down cleanly, the watchdog reboots a deliberately hung QEMU machine, and the Framework's fans follow its temperature.

### Phase 12: Networking and WireGuard (XL)

- **12a Network stack and virtual NICs (L):** the stack built from smoltcp and extended for production use: TCP window scaling, SACK, timestamps, CUBIC; ICMPv4 and v6. virtio-net and e1000 for QEMU; the e1000 driver also runs as a userspace process. BSD sockets in mlibc, gated by network capabilities (outbound, listen, per-port rights). `netd` (DHCPv4 and v6 client, optional DHCP server, static configuration), `resolved` (caching, multiple resolvers, DNS-over-TLS and HTTPS), `timed` (Network Time Security (NTS) by default with plain SNTP as a fallback, writes the RTC; ships the IANA time zone database, public domain, and Unicode CLDR locale data, Unicode license; English is the 1.0 language); `ip`, `ss`, `ping` and `curl`; netlog streams kernel and service logs over UDP to another machine for debugging on the reference machines. Done when DHCP and `curl https://` work in QEMU and a bundle without network permission cannot open a socket.
- **12b Real NICs (L):** the Realtek family, covering the Dell's RTL810x Fast Ethernet chip (PCI ID 10ec:8136) and the Framework Desktop's RTL8126 5 GbE chip, written from Realtek's documentation with OpenBSD's ISC-licensed rge(4) and re(4) drivers as reference, since Linux's r8169 is GPL; USB Ethernet (CDC-ECM, CDC-NCM and RTL8153 gigabit adapters); Intel e1000e and igc. Done when DHCP works on both reference machines.
- **12c IPv6, filtering and discovery (M):** IPv6 with NDP and SLAAC, using stable private (RFC 7217) and temporary (RFC 8981) addresses by default; bridging and VLANs; multiple interfaces; a small ordered packet-filter ruleset that denies inbound connections by default (enabling sshd opens only port 22), managed with a firewall command that lists, adds and removes rules and logs dropped packets to logd, documented in firewall(8); mDNS and DNS-SD announce the hostname on the local link so ssh hostname.local works.
- **12d WireGuard, server NICs and tools (M):** WireGuard VPN through boringtun (Cloudflare, BSD-3-Clause, Rust), moved up from the hardware phase for servers. The server NICs, Intel igb (I210, I350) and ixgbe (X520, X540, X550), with 10 GbE tuning (receive-side scaling, checksum and segmentation offloads, interrupt moderation). The network rows of the administration command set and the iperf3 port for measurement.
- **Exit:** DHCP works on QEMU and on the Dell's built-in Ethernet, `curl https://` succeeds, a bundle without network permission cannot open a socket, a WireGuard tunnel connects two VMs, and iperf3 throughput matches Linux on the same hardware: on the Framework's 5 GbE port, and on the Dell through a USB 3 gigabit adapter because its built-in port tops out at 100 Mbps.

### Phase 13: Users, bundles, service bundles and logging (L)

- **13a Logging (S):** `logd` and `logtrim` writing JSON lines under `/system/state/log`; audit log of security events; `notifyd` with console delivery. Privacy promise, stated in the handbook and the installer: Dukovo has no telemetry, and nothing leaves the machine unless the user starts it. Update checks are the only automatic network calls and can be turned off; crash reports are kept locally and shared only by hand.
- **13b Users and login (M):** `authd` with Argon2id password hashes, `login` on every console, the `user` tool, home directory skeleton, per-user and admin capability sets. An elevate command re-asks the password through authd and runs one command with the admin set; every use goes to the audit log. authd slows down repeated failed logins per account and per source. No password rules are forced: as on Linux, a short, simple or empty password is allowed after a plain warning and a confirmation, with no complexity rules, no forced expiry and no required online account. Secure defaults such as login rate limiting and key-only SSH stay defaults the owner can change. Dukovo is a users-first system: it warns and confirms, and the user decides. The user rows of the administration command set (passwd, id, last, the `sudo` front end and the rest). Done when only the admin can read logs.
- **13c SSH (M):** sshd, a Rust SSH server built on the russh crate (Apache-2.0), managed by Spark. Logins go through authd with Ed25519 public keys by default and passwords optional (SSH accepts only keys unless the user enables passwords); each session gets the user's capability set; SFTP and port forwarding are gated by capabilities. Host keys are generated on first boot; the ssh and sftp clients from the same crate ship too, with ssh(1), sftp(1), sshd(8) and sshd\_config(5) man pages. sshd is off until the installer or the user enables it. This makes the headless system usable remotely. Done when the user logs in to the Dell over SSH from another machine with a key.
- **13d Bundles (M):** bundle runtime: manifest JSON schema and validation, keyboard-only permission prompt, grant storage, launch with exactly the granted capabilities and a filesystem view built from them. Bundle signing with Ed25519 (minisign-compatible) over a hashed file list; `bundle` CLI (`install`, `list`, `info`, `grant`, `revoke`, `run`, `remove`, `sign`, `verify`).
- **13e Catalog and service bundles (L):** bundle catalog and app updates: the update server and its TUF-style signed metadata (client library, offline root key and online signing keys) are built here, and a signed catalog index is served from it; bundle update (and, only if the user turns it on, a background updater) installs new versions side by side and switches atomically, keeping the previous version for rollback. The 1.0 catalog is seeded with console and server bundles, each license-checked: git, Python, Perl, GNU bash, zsh, fish, Vim, GnuPG, GNU make, ripgrep, fd, htop and an IRC or Matrix client, later joined by the 1.0 services from Phase 16. A new version that asks for more permissions waits for the user's approval instead of gaining them silently. Apps live outside /system, so app and system updates never touch each other. Several catalogs can be configured: the official Dukovo catalog plus third-party ones. Sideloading stays open: a bundle from outside any catalog installs if a developer key signed it, after a warning that shows the publisher and the permissions it asks for; an unsigned bundle needs an explicit per-bundle override. A bundle may name its own signed update feed in its manifest, so sideloaded apps update like catalog apps. Service bundles: a bundle may declare long-running services in its manifest. Spark runs them under the bundle's grants, with a restart policy and resource limits, and they update through the catalog like any app without touching /system.
- **Exit (milestone, with Phases 10 to 12: spec 16.7 first usable system):** a user installs a sample bundle into `~/applications`, approves `documents: read-write`, launches it, and it can write `~/documents` but not `~/downloads`; unsigned bundles are refused under the default policy; only the admin can read logs; the user logs in to the Dell over SSH from another machine with a key; a sample service bundle installs, starts at boot and is restarted after a crash.

### Phase 14: Installer, rdisk, USB media and cloud images (L)

- **14a Install tools and USB media (L):** live image, `dukovo-install`, `rdisk`, `mkfs-btrfs`, `mkfs-fat`, `mkswap`, `rgrub-setup` and answer-file installs, as specified in the next section. Done when an automated QEMU test installs to a blank disk under UEFI and under BIOS, reboots and logs in.
- **14b Speech and braille (M):** accessibility from day one: a built-in Rust screen reader speaks the text console, Nukesh and the installer through eSpeak NG (GPL-3.0, run as a separate program), and BRLTTY (LGPL-2.1, a separate program) drives USB braille displays. Speech can be turned on from the boot menu, so a blind user can install Dukovo without help. Basic audio playback (Intel HDA, including the Framework Desktop's Realtek ALC623 codec, USB Audio Class and virtio-snd) carries the speech; the full audio stack stays in Phase 27. Also in 1.0: console text scaling, sticky and slow keys, and a visible focus indicator in the installer and the web console.
- **14c Server installs (S):** the server install profile offers a mirrored two-disk layout (Btrfs RAID1) when it finds two disks, asks whether to turn on the web console, and runs fully over a serial console.
- **14d Cloud and VM images (M):** qcow2 (KVM and Proxmox), raw, VMDK (VMware), VHDX (Hyper-V), and images for DigitalOcean and AWS, built by `./ob image --cloud`. First-boot setup in the style of cloud-init reads the hostname, users, SSH keys, network and enabled services from the cloud's metadata service or an attached config disk, using a subset of cloud-init's format so existing user-data files mostly work. The QEMU guest agent reports addresses and allows clean shutdown.
- **14e Release readiness (M):** before the first public image: confirm the final product name, Dukovo (chosen 2026-10-22; the code and docs moved from the working name OrthrOS the same day), then finish its trademark and domain clearance (USPTO, EUIPO), confirmed by a lawyer, and a trademark policy in the repository. SECURITY.md with private reporting through GitHub private advisories and a disclosure timeline. Quiet boot: the GRUB menu stays hidden unless Shift or Esc is held, a simple progress screen replaces kernel text, and a key shows the log. CI enforces budgets: boot to login under 10 seconds on both reference machines, idle memory under 256 MB for the console system, and an image under 4 GiB. A seven-day soak test under mixed load in QEMU runs before each release.
- **Exit:** an automated QEMU test installs to a blank disk under UEFI and under BIOS, reboots and logs in; a speech-guided install succeeds; a two-disk mirrored install and a serial-only install succeed; a cloud image boots in QEMU with a config disk and applies its settings; and manual installs from a USB stick succeed on the Dell and the Framework Desktop.

### Phase 15: Atomic updates and snapshots (M)

- **15a A/B updates and recovery (M):** `sysupdate`: signed update images, A/B system subvolumes, GRUB boot counting through `grubenv` on the EFI partition, a `boot-complete` health check in Spark, automatic rollback. A Recovery boot entry starts the previous slot into a root console with networking off, and offers a system reset that keeps /users and /shared. Done when a deliberately broken update rolls back on its own.
- **15b Snapshots and backups (M):** `snap`: create, list, restore and delete snapshots of `/system`, `/users` or any subvolume; optional inclusion of `/volumes` targets. A backup tool sends Btrfs snapshots incrementally to a USB disk or to another machine over SSH, on a schedule, encrypted with rage (Rust, MIT/Apache); after Phase 23 it can use the system key store. snap applies a retention policy (default 7 daily and 4 weekly snapshots) and prunes old snapshots and update slots automatically.
- **15c Update delivery (M):** system update delivery on the update server and TUF-style (The Update Framework) signed metadata from Phase 13, which block rollback, freeze and malicious-mirror attacks; stable, beta and nightly channels chosen with sysupdate channel; binary delta updates between consecutive images so routine updates stay small. The optional firmware package updates through the same signed path.
- **15d Unified updater and server policy (M):** one updated service and one update command check, download and apply system images, firmware, catalog apps and sideloaded apps with their own feeds, together. The user is always in charge: Dukovo never installs an update or restarts the machine on its own. The default is to check and notify only; the user can choose to download in the background, auto-update apps, or turn checks off entirely, and can skip or pin any version. A system update is staged into the spare A/B slot and takes effect at whatever reboot the user chooses, with no update screens at shutdown or boot; reminders can be snoozed or silenced, and security fixes are highlighted but never forced. One history; respects metered connections; shows which security advisories each update fixes. sysupdate and bundle update remain as the lower-level tools. Servers: optional unattended security updates on a schedule the admin picks, off by default, with an automatic reboot only if the admin turns that on as well; the A/B slots and rollback make this safe. An LTS channel next to stable, beta and nightly.
- **Exit:** an update applies in QEMU; a deliberately broken update rolls back on its own; a deleted file in `/users` is restored from a snapshot; a delta update from the previous release installs from a local test server, and a tampered or older update is refused.

### Phase 16: Server platform and web console (XL)

- **16a Service management (M):** production service management, building on the supervision from Phase 9: Spark service files declare dependencies, a health check and the capabilities a service may hold; per-service limits on CPU share, memory, disk I/O and open files through the kernel's resource domains; socket activation; scheduled jobs (timers) in the same files, with `crontab` and `at` as front ends. The `service` command (`list`, `status`, `start`, `stop`, `restart`, `enable`, `disable`, `logs`) and the remaining rows of the administration command set.
- **16b Remote and lights-out management (M):** a full serial console from GRUB to login, including Serial-over-LAN through a server's BMC; remote logging, with logd forwarding to a syslog server (RFC 5424 over TLS) and netlog as a supported feature; `metricsd`, a Prometheus-format exporter for CPU, memory, disks, Btrfs, network and per-service use, listening only once the admin enables it; kernel crash dumps saved to disk for later analysis and never sent anywhere; servers can act as a local NTS time source.
- **16c Dukovo Admin (L):** the `admind` API first, then the web console on top of it, as specified in the Server platform section below.
- **16d The 1.0 services (L):** nginx, certd, PostgreSQL, SQLite, Valkey, PHP-FPM and the rest of the services table in the Server platform section, each as a signed service bundle with its acceptance test.
- **16e Hardening and first production use (M):** `dukovo-check` reports open ports, enabled services, password logins over SSH, weak settings and pending updates, and the handbook gets a server hardening guide. The web console gets fuzzing of its API and an outside security review before 1.0. The project's own update server (updates.dukovo.org) moves onto Dukovo itself once this phase works, so the project is its own first production user.
- **Exit (milestone: Release 1.0):** the full acceptance list in the Release 1.0 section passes.

## Installer, rdisk and USB install media

One USB image boots on UEFI and legacy BIOS machines, offers a live console, and installs Dukovo with a keyboard-only installer whose disk tool, `rdisk`, also works on its own like fdisk.

### USB install image

`./ob image` produces a hybrid GPT disk image (default 4 GiB) that is written raw to a USB stick:

| Region | GPT type | Size | Contents |
| --- | --- | --- | --- |
| Protective MBR | none | sector 0 | GRUB `boot.img` for BIOS boot |
| BIOS boot | EF02 | 1 MiB | GRUB `core.img` |
| EFI System | EF00, FAT32 | 256 MiB | `EFI/BOOT/BOOTX64.EFI` (GRUB), `grub.cfg`, the `radix` kernel, `bootfs.tar`, optional `install.json` |
| DUKOVO\_LIVE | Dukovo data, Btrfs | rest | Read-only live system and the signed system image the installer writes to disk, plus the separate optional firmware package |

The system image is the same signed format that `sysupdate` uses, so installation and updates share one code path and one verification step.

The GRUB menu offers: **Install Dukovo**, **Try Dukovo (live console)**, **Install with serial console**, **Install with speech**, and **Boot from first disk**. The live system mounts tmpfs over `/system/settings`, `/system/state`, `/users` and `/tmp`, starts the installer on the first virtual terminal and a rescue Nukesh on the second.

### Installer (`dukovo-install`)

A text interface built with ratatui. It works entirely from the keyboard and over a serial console (spec 7.14). Steps:

1. Keyboard layout, and speech for blind users if it was not already turned on at the boot menu.
2. Disk choice: every disk with model, size, bus and current partitions. The USB stick the system booted from is marked and cannot be chosen.
3. Partitioning: **Erase disk and use the guided layout**, **Mirror two disks** (Btrfs RAID1, offered when two or more disks are present), **Use free space**, or **Manual** (opens `rdisk`).
4. Review: an exact list of what will be created, erased and formatted. Destructive changes require typing the disk's name.
5. Install: partition, format, create subvolumes, verify and unpack the signed system image, install GRUB for both UEFI and BIOS (with a Windows Boot Manager menu entry when one is found), register a UEFI boot entry and the fallback `EFI/BOOT/BOOTX64.EFI` path.
6. Hostname, time zone, and network (DHCP by default, or static addresses), whether to enable SSH (off by default), whether to turn on the web console (yes by default on the server profile, which also opens port 9090 in the firewall; off on cloud images), and the update preference (notify only by default, unattended security updates, or off). This screen also shows the privacy promise. Then the optional firmware package (decided; ADR 0007): closed-source but redistributable files, Intel and AMD CPU microcode and the Realtek RTL8126 firmware now, and GPU, Wi-Fi and NPU firmware after Phase 24, installed only if chosen and kept outside /system.
7. Administrator account (spec 7.7), plus optional ordinary users.
8. Summary, eject prompt, reboot.

The install log goes to `/tmp/install.log` and is copied to `/system/state/log/install.log` on the new system. An answer file (`install.json`, on the EFI partition or passed with `--answers`) runs the same steps unattended, which is how CI tests every install.

### Guided disk layout

| Partition | GPT type | Size | Filesystem | Purpose |
| --- | --- | --- | --- | --- |
| BIOS boot | EF02 | 1 MiB | none | GRUB `core.img` for BIOS machines |
| EFI System | EF00 | 512 MiB | FAT32 | GRUB, kernels and boot archives for both update slots, `grubenv` |
| Swap | Linux swap GUID | Equal to RAM, capped at 8 GiB (installer default, adjustable, 0 disables) | swap | Paging and future suspend-to-disk |
| Dukovo root | Dukovo root GUID (minted in Phase 0) | Remaining space | Btrfs | Everything else |

Btrfs subvolumes on the root partition:

| Subvolume | Mounted at | Notes |
| --- | --- | --- |
| `@root` | `/` | Only mount-point directories |
| `@system.a`, `@system.b` | `/system` | Read-only; one active slot, one for the next update |
| `@settings` | `/system/settings` | Writable, survives updates |
| `@state` | `/system/state` | Writable; logs, caches, package and grant state |
| `@applications` | `/applications` | System-wide bundles |
| `@users` | `/users` | Home directories |
| `@shared` | `/shared` | Files shared between users and services |
| `@snapshots` | not mounted | Managed by `snap` |

`/tmp` and `/system/live` (live service state: sockets, locks, process IDs and each user's runtime folder) are tmpfs; `/devices` (device files), `/processes` (processes, mounts and memory as text) and `/kernel` (the kernel's view of the hardware and its settings, one value per file) are virtual; `/volumes` holds removable media. The EFI partition is mounted at `/system/boot`. The names say what each folder holds (ADR 0113): the Unix names `/dev`, `/proc`, `/sys` and `/run` stay as links, like `/bin`, `/usr`, `/etc` and `/var`, so ported software finds its paths; the file manager hides them, and so does `ls /` unless asked; the C library reports the new names, and the Linux compatibility layer (release 1.3) shows the Linux layout inside its own view.

### `rdisk` partitioner

`rdisk` is an fdisk-style tool for GPT and MBR disks, written in Rust and documented in `rdisk(8)`.

- **Interactive mode** keeps fdisk's single-letter commands so the habit carries over: `p` print, `n` new, `d` delete, `t` type, `l` list types, `g` new GPT, `o` new MBR, `c` name, `u` units, `v` verify, `w` write, `q` quit, `m` help.
- **Script mode** (`rdisk --script layout.json /devices/disk0`) applies a layout file; the installer and `./ob image` both use it.
- GPT primary and backup headers with CRC32 checks and repair, protective MBR, 1 MiB alignment, partition names, readable type aliases (`efi`, `bios-boot`, `swap`, `dukovo-root`, `linux`, `windows-data`).
- Nothing is written before `w`; `--dry-run` prints the result; mounted disks are refused.
- Disks appear as `/devices/disk0`, partitions as `/devices/disk0p1`, with `/devices/disk/by-label/` and `/devices/disk/by-uuid/` links.
- Builds for Linux too, so `./ob image` uses the same code that runs on target machines.

### Formatting and boot tools

| Tool | Purpose | Notes |
| --- | --- | --- |
| `mkfs-btrfs` | Create Btrfs filesystems and subvolumes | Shares the on-disk structure crate with the kernel driver; output verified with Linux `btrfs check` in CI. Fallback: port btrfs-progs |
| `fsck-btrfs` | Check Btrfs (read-only in 1.0) | Same crate |
| `mkfs-fat`, `fsck-fat` | FAT12/16/32 for EFI partitions and USB sticks |  |
| `mkfs-exfat` | exFAT for large USB media |  |
| `mkswap`, `swapon`, `swapoff` | Swap setup | Standard swap format |
| `mkfs` | Front end: `mkfs -t btrfs /devices/disk0p4` |  |
| `rgrub-setup` | Installs GRUB: EFI binary to the EFI partition; for BIOS writes `boot.img` and `core.img` and patches their sector pointers as `grub-bios-setup` does | GRUB images prebuilt by `./ob` from pinned source with only the modules needed (gpt, msdos, fat, btrfs, multiboot2, loadenv, search, gfxterm, serial) |

### Writing the stick

`./ob write-usb` is the recommended path. Alternatives that also work: balenaEtcher, GNOME Disks "Restore Disk Image", Rufus in DD mode, or `sudo dd if=out/dukovo-<version>-x86_64.img of=/dev/sdX bs=4M conv=fsync status=progress`.

## Server platform and web console

Phase 16 turns the working system from Phases 0 to 15 into a server people can run in production and manage remotely. The web console is a first-class part of 1.0, not an add-on.

### Dukovo Admin web console

Dukovo Admin ships in /system on every 1.0 server install. You open `https://<host>:9090` in any browser and run the whole machine from there. It is modeled on Fedora's Cockpit. We build our own because Cockpit (LGPL-2.1) is built around systemd, D-Bus, PAM, udisks and NetworkManager, none of which Dukovo has; we take Cockpit's ideas and none of its code or artwork, and credit it as an inspiration in CREDITS.md.

| Page | What the admin can do |
| --- | --- |
| Overview | Health at a glance: CPU, memory, disk and network graphs, uptime, pending updates, warnings |
| Services | Start, stop, restart, enable, disable; edit limits and restart policy; live logs per service |
| Logs | Search and filter logd's JSON logs by service, level and time; follow live; export |
| Storage | Disks and SMART health, Btrfs volumes, RAID status and rebuilds, subvolumes, quotas, scrub |
| Snapshots and backups | Create, browse and restore snapshots; schedule backups; restore single files |
| Networking | Interfaces, addresses, DNS, VLANs, bridges, WireGuard tunnels, live traffic |
| Firewall | List, add and remove rules; see recently dropped connections |
| Users | Add and remove users, reset passwords, manage SSH keys, admin rights, login history |
| Apps | Browse the catalog, install, update and remove bundles and service bundles; review and change permissions |
| Updates | Check, download and apply system, app and firmware updates; pick the reboot time; roll back |
| Certificates | Let's Encrypt certificates for the console and for services, with renewal status |
| Terminal | A full Nukesh session in the browser, with the signed-in user's rights |
| Hardware | Sensors, temperatures, fans, power, PCI and USB devices, the watchdog |

- **API first.** `admind`, a Rust service, exposes one documented, versioned JSON API. The web console, the `admin` command and automation scripts all use it, so the console can never do anything the API can't, and nothing is web-only.
- **Same security model as the console login.** You sign in through authd with your Dukovo account. Each browser session holds only that user's capabilities, and admin actions ask for the password again (elevate), with each one written to the audit log.
- **Locked down by default.** HTTPS only. A self-signed certificate is made at first boot and its fingerprint shown on the console, or a Let's Encrypt certificate is used once a domain is set. Login rate limiting applies. Optional two-factor sign-in (TOTP) and passkeys, never forced. CSRF protection and short idle timeouts.
- **Fast and small.** No external CDNs or trackers: everything is served from the machine itself. Pages work on a phone, and live data streams over a WebSocket.
- **Accessible.** WCAG 2.2 AA, full keyboard use, and works with screen readers. Text is English first and translatable, like the rest of the OS.
- **Enabled by choice.** The server install profile asks whether to turn it on, with yes as the default, and opens port 9090 only if the admin says yes. Cloud images leave it off, to be turned on with `service enable admind` or reached through an SSH tunnel.
- **Design approval.** The console is the first Dukovo UI anyone sees. Its look goes through a short design session with the maintainer in Phase 16, under the UI/UX approval gate, and its colors and type follow the brand kit.
- **Later:** managing several Dukovo machines from one console, as Cockpit does (Phase 25), and containers and virtual machines as they arrive (Phases 20 and 32).

### Services in 1.0

The 1.0 server runs a website and a database out of the box. Only the management pieces (`admind`, `certd` and `metricsd`) live in /system. Everything else ships as a signed service bundle from the catalog, so it updates on its own schedule and never touches /system. Installing one is a click in the web console or `bundle install nginx && service enable nginx`. Every license is confirmed by the automated license check before a port lands, and GPL software is allowed only as a separate catalog bundle.

| Service | What for | Software | License | Notes |
| --- | --- | --- | --- | --- |
| Web server and reverse proxy | Websites, apps behind a proxy, load balancing, TLS | nginx | BSD-2-Clause | The default web server, ported through the Phase 8 POSIX layer |
| Certificates | Free HTTPS certificates that renew themselves | `certd`, built on instant-acme | Apache-2.0 / MIT | In /system; feeds nginx, the web console and any service that asks |
| Database | Relational database for apps | PostgreSQL | PostgreSQL License | Needs shared memory and semaphores that work across processes (shared futexes, fixed in the post-Phase-9 audit); passes its own regression suite |
| Embedded database | Small apps and the console's own data | SQLite | Public domain | Library and `sqlite3` command |
| Cache and queues | Session store, caching, job queues | Valkey | BSD-3-Clause | The open-source Redis fork; Redis itself is no longer open source in the usual sense |
| VPN | Private access to the server | WireGuard through boringtun | BSD-3-Clause | From Phase 12 |
| Remote files | Secure file transfer | sftp | Apache-2.0 | Built into sshd |
| Metrics | Graphs and alerts in Prometheus or Grafana elsewhere | `metricsd` | Apache-2.0 OR MIT | In /system, off until enabled |
| Language runtimes | Running web apps | Python, PHP-FPM | PSF; PHP License 3.01 | PHP covers WordPress, Nextcloud and many self-hosted apps |

More services follow in the 1.x line: Podman in Phase 20, and MariaDB, Unbound, Samba, a mail server, Node.js, Caddy, Prometheus and Forgejo in Phase 25.

## Lumen compositor (Phase 17, Dukovo 1.1)

Lumen is the first 1.x release (moved out of 1.0 on 2026-10-23). It is a complete Wayland compositor with no desktop environment: it shows windows, title bars and notifications, and nothing that resembles a panel, launcher, dock or settings app. with no desktop environment: it shows windows, title bars and notifications, and nothing that resembles a panel, launcher, dock or settings app.

### In scope

- **Foundation:** Smithay (Rust Wayland compositor library) with a Radix backend written for the display layer and `inputd`, and the pixman software renderer. wayland-rs uses its pure-Rust server, so libwayland is not required.
- **Protocols:** `wl_compositor`, `wl_shm`, `wl_seat`, `wl_output`, `xdg-shell`, `xdg-decoration`, `wl_data_device` (clipboard), `viewporter`, `fractional-scale`, `presentation-time`, `linux-dmabuf` equivalent once virtio-gpu buffers are shareable.
- **Title bars (spec 7.4):** Lumen draws server-side title bars with minimize, maximize/restore and close on every window. A global setting and per-application overrides force server-side decorations; client requests to disable them are ignored when forced, and the client is told through `xdg-decoration`.
- **Window management:** stacking with click-to-focus, keyboard focus cycling (Alt-Tab), move and resize by title bar and edges, maximize, minimize to a keyboard-reachable list (Super key), and a fallback key to close a window. These are compositor behaviors, not a desktop shell.
- **Capabilities:** Lumen is a Spark-supervised service and the only holder of display and input device capabilities. A bundle connects only if its manifest declares `"display": true` and the grant was approved; otherwise its connection fails cleanly. Clipboard read, screen capture and input injection are separate grants.
- **Notifications:** Lumen subscribes to `notifyd` and shows plain-text notifications with position, timeout and do-not-disturb settings.
- **Laptop and session basics:** a lock screen (on demand, on lid close and after idle), battery and brightness indicators with the brightness keys, a shutdown and restart shortcut, and the Inter, JetBrains Mono and Noto fonts rendered through cosmic-text and swash.
- **Accessibility:** a Lumen accessibility protocol exposes window titles, focus changes and basic roles to the built-in screen reader; high-contrast and large-title-bar themes; everything usable from the keyboard. Accessibility from day one: a built-in Rust screen reader speaks the text console, the installer and Lumen through eSpeak NG (GPL-3, run as a separate program), and BRLTTY (LGPL-2.1, a separate program) drives USB braille displays. Speech can be turned on from the boot menu, so a blind user can install Dukovo without help. Basic audio playback (Intel HDA, including the Framework Desktop's Realtek ALC623 codec, USB Audio Class and virtio-snd) is in 1.0 to carry speech in the console and installer; the full audio stack stays in Phase 27. Also in 1.0: text scaling, larger cursors, reduced motion, sticky and slow keys, and a visible focus indicator everywhere.
- **Configuration:** `/system/settings/lumen.json` for system defaults and `~/.config/lumen.json` per user, both documented in `lumen.json(5)`.
- **Starting it:** `lumen` from Nukesh, or enable `lumen.service` in Spark to start it after login. Leaving Lumen out of `dukovo.toml` builds a console-only image (spec 7.4, 16.6), which is the 1.0 default.
- **Phase 17 work items:** the display layer with modes, framebuffers and page flips for GOP and virtio-gpu; `inputd` event devices behind capabilities; the Dell's touchpad, most likely HID over I2C, which needs Intel LPSS I2C, Intel GPIO and I2C-HID drivers (a USB mouse works until then); the renderer behind a trait so a Vulkan renderer can replace pixman once GPU drivers exist (Phase 24); fonts Inter, JetBrains Mono and Noto (all OFL) through cosmic-text and swash; the lock screen; lid close locking and turning the screen off; ACPI backlight and the battery level; `terminal.bundle` as the supported terminal, with tabs, scrollback and Windows-style copy and paste; and VirtualBox as a guest. The full UI/UX design session with the maintainer comes before the visual work.
- **Test client:** `terminal.bundle`, a small Rust terminal emulator (alacritty\_terminal for the VT parser, cosmic-text and swash for text in the Lumen fonts) that proves input, rendering, decorations and permissions end to end.

### Out of scope for 1.1

- A desktop shell, panel, launcher, wallpaper manager or settings application.
- Hardware-accelerated rendering on native GPUs (virtio-gpu 2D and software rendering only).
- XWayland, and GTK and Qt inside third-party bundles (never in /system); these come after 1.0 with Phase 19.
- Multi-monitor layout beyond mirroring or a single extended output on virtio-gpu.

## Documentation and man pages

Every shipped command, service, file format, system call and library interface gets a man page, and `./ob docs --check` fails the build when one is missing.

### Formats and tools

- **Man pages** are written in scdoc (a small Markdown-like syntax, MIT) and compiled to roff. On the system, `man` is mandoc (ISC), which also renders HTML for the project website. Pages install to `/system/resources/man`.
- **Handbook** is written in mdBook (MPL-2.0) and installs as plain text and HTML under `/system/resources/doc/handbook`, readable from the console with `less` (spec 7.12).
- **Design documents** live in `docs/design/` as Markdown, one per subsystem, each with its `unsafe` inventory and safety arguments.
- **API reference** comes from rustdoc for every crate and from `ridlc` for every interface.
- Ported tools ship their upstream man pages; uutils pages are generated from its argument definitions. Where Dukovo behaves differently (for example permissions), the page gets an "Dukovo notes" section.
- Every tool supports `--help`, and Nukesh has a `help` builtin.

### Man page sections

| Section | Contents | Examples |
| --- | --- | --- |
| 1 | User commands | `nukesh(1)`, `bundle(1)`, `snap(1)`, `ps(1)`, `ssh(1)`, `editor(1)`, `rtrace(1)`, `lsof(1)`, `log(1)`, `admin(1)`, `lumen(1)` (1.1) |
| 2 | System calls, generated from `.ridl` files | `cap_call(2)`, `ring_setup(2)`, `map(2)` |
| 3 | Library interfaces | `libradix(3)`, `radix_ring(3)` |
| 4 | Devices and drivers | `nvme(4)`, `xhci(4)`, `e1000(4)`, `virtio-gpu(4)` |
| 5 | File formats | `manifest.json(5)`, `spark.service(5)`, `install.json(5)`, `lumen.json(5)`, `network.json(5)`, `grants(5)`, `sshd_config(5)`, `service.toml(5)`, `admind.toml(5)`, `cloud-config(5)` |
| 7 | Overviews and concepts | `radix-capabilities(7)`, `dukovo-hier(7)`, `radix-fs-rights(7)`, `bundles(7)`, `dukovo-boot(7)`, `dukovo-update(7)`, `dukovo-privacy(7)` |
| 8 | Administration | `rdisk(8)`, `firewall(8)`, `dukovo-install(8)`, `mkfs-btrfs(8)`, `sysupdate(8)`, `spark(8)`, `netd(8)`, `logd(8)`, `user(8)`, `rgrub-setup(8)`, `elevate(8)`, `update(8)`, `updated(8)`, `backup(8)`, `sshd(8)`, `service(8)`, `admind(8)`, `certd(8)`, `metricsd(8)`, `dukovo-check(8)`, `btrfs(8)`, `diskhealth(8)`, `watchdog(8)` |

### Handbook chapters

1. Introduction to Dukovo and Radix
2. Building Dukovo with `ob`
3. Creating USB install media
4. Installing, including manual partitioning with `rdisk`
5. First boot and login
6. Nukesh guide and scripting
7. Files, the directory hierarchy and `/volumes`
8. Bundles and permissions
9. Users and administration
10. Networking
11. Updates, snapshots, backups and recovery
12. The Dukovo Admin web console
13. Running services: web, databases and certificates
14. Cloud and virtual machines
15. Server hardening
16. Logs, monitoring and troubleshooting
17. Supported hardware
18. SSH and remote access
19. Power, laptops and desktops
20. Privacy and security reporting
21. Moving from Linux: familiar commands and their Dukovo equivalents

The Lumen chapter joins the handbook with 1.1.

Developer documentation adds a porting guide (writing `ports/` recipes), a driver-writing guide, the ABI reference, the contributor guide and the ADR log (spec 13).

### How completeness is enforced

- `./ob docs --check` confirms that every binary in `/system/commands`, every service, every `.ridl` interface and every JSON schema has its page, and runs `mandoc -T lint` on all of them.
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
| System suites | Every spec 7.2 utility, bundle permissions, users, logging, two-VM network tests, update and rollback, the web console driven end to end in headless Chromium, each 1.0 service's acceptance test, the shell script corpus | Nightly |
| Filesystem torture | Linux `btrfs check` on images Radix wrote; images from Linux mounted by Radix; random power-cut tests; stress workloads | Nightly |
| Fuzzing | cargo-fuzz (libFuzzer) on ELF, manifests, JSON configs, GPT and MBR, Btrfs, FAT and exFAT metadata, network packets, IPC decoders, the Nukesh parser, cloud-config files and the admind API; a syscall fuzzer running in QEMU (the `fuzz` boot test since Phase 5, which also checks every error against the call's documentation), with longer runs under KVM | Every push; continuous on a dedicated runner |
| Performance | Microbenchmarks for syscall, context switch, IPC, page fault and memory bandwidth; storage and iperf3 throughput, nginx requests per second (wrk) and PostgreSQL transactions per second (pgbench); each compared with Linux on the same hardware, failing CI when Radix is slower at any of them (`ob bench --gate`, ADR 0107); instruction counts per operation under QEMU held to budgets (`ob bench --instructions --gate`) | Every push in QEMU; per release on the Dell and the Framework Desktop |
| Real hardware | Install and acceptance checklist on the Dell and the Framework Desktop, with remote power control, netlog and the xHCI debug console once automated | Every release |
| Dependency checks | cargo-deny (licenses and advisories), cargo-audit, and an `unsafe` inventory report | Every push |

### Audits

A full audit closed Phase 5, and three more are checkpoints of the plan: after Phase 9 (the console system), after Phase 12 (networking) and before Phase 14 (the installer and the first public image). Each covers every `unsafe` block and its safety argument, lock ordering, the capability checks and input validation of every system call and server interface, that comments, man pages, the handbook, ADRs and this plan match the code, and a profile of the hot paths against the Linux gate and the instruction budgets. Its findings are fixed, or recorded with a plan, before the next phase starts, and the report goes with the pull request that fixes them.

### CI setup

CI runs on GitHub Actions or Forgejo Actions, both free for open source projects. QEMU tests need runners with KVM access; fuzzing and nightly torture runs fit best on one self-hosted machine. Every pipeline step is just an `./ob` command, so anything CI does, can be run locally the same way.

### Reproducible builds

- Pinned Rust, Clang and GRUB versions; `Cargo.lock` plus vendored crates; SHA-256-pinned port sources.
- `SOURCE_DATE_EPOCH` everywhere; filesystem UUIDs, timestamps and file order in images derived from the version, not the clock.
- CI builds each release twice in different directories and compares the outputs with diffoscope.
- Release images ship with SHA-256 checksums and Ed25519 (minisign) signatures.
- License compliance is automated. cargo-deny (already in CI) enforces an allow-list per area: permissive licenses only in the kernel and /system Rust code; GPL and LGPL only for ports shipped as separate programs or bundle-private shared libraries; LGPL never statically linked into /system. Each port recipe declares its SPDX license. ./ob image writes a license manifest (an SPDX SBOM listing every crate, port and firmware file with its license and source hash) into the image at /system/resources/licenses together with all notice files, and ./ob release publishes the exact source tarballs of every GPL and LGPL component next to the image. CI fails if a component lacks a license, uses one outside its area's allow-list, or ships without its source. Credit goes beyond the legal minimum. Code taken from another project keeps its original copyright and license headers, and the commit names the source project, version and URL. The repository follows the REUSE specification: every file carries an SPDX license and copyright line, checked in CI with the reuse tool (a GPL build-time tool that never ships). Users see the credits too: System info has an Acknowledgements page generated from the SPDX manifest (each project, its authors, license and home page), every first-party app's About box lists the projects it uses, and the credits(1) command prints the same list. CREDITS.md in the repository, the handbook and each release's notes thank upstream projects, and projects whose ideas we follow without using their code (TMOG, macOS, Ubuntu and others) are named where the design draws on them. Fixes we make to upstream code are sent back upstream.

## Release and maintenance

This process starts at Phase 14, with the first installable image: vulnerability scanning, advisories and the offline root signing key are in place before anyone installs Dukovo. From 1.0 on, Dukovo ships a monthly stable release, and security fixes go out within days. Servers can follow an LTS channel instead: once the release process is proven after 1.0, one release a year becomes an LTS release that gets security and critical fixes for at least two years, longer once there are more maintainers. Machines apply both through the signed update path from Phase 15; apps update separately through the bundle catalog from Phase 13.

- **Cadence:** nightly builds from every merge, a beta two weeks before each monthly stable release, and out-of-band security releases. Each stable release is supported until two newer ones exist.
- **Vulnerability tracking:** a daily CI job matches each supported release's SPDX SBOM (from the license manifest) against the OSV and NVD databases, plus cargo-audit for crates; a match opens an issue with its severity.
- **Dependency updates:** a bot (Renovate) opens pull requests to bump crates, the pinned sources of ported tools, and GRUB; the Rust toolchain moves quarterly, as in the risks table.
- **Security advisories:** published as Dukovo Security Advisories in the repository with a JSON and RSS feed; sysupdate shows which advisories an update fixes.
- **Signing keys:** an offline root key signs online release and catalog keys, which rotate yearly; the procedure lives in the handbook. The update server publishes a short privacy policy: access logs are kept 14 days at most, IP addresses are never tied to machines, and nobody tracks who installed what. Every commit carries a Developer Certificate of Origin sign-off, checked in CI, and the project has a code of conduct (Contributor Covenant) and a short governance note naming the maintainer.
- **Firmware:** the optional microcode, network, Wi-Fi and GPU firmware package is versioned and signed like the system. BIOS and device firmware updates come post-1.0 through a port of fwupd (LGPL, a separate program) using LVFS metadata.
- Catalog policy: the official catalog hosts only bundles whose licenses allow redistribution, and publishes the source of every GPL and LGPL one. Proprietary apps such as Steam are listed as entries whose bundle downloads from the vendor's own signed feed, so Dukovo never redistributes them. Paid apps stay out of scope until a later decision.

## Reference machines

Dukovo has two real-hardware targets for 1.0: the Dell Inspiron 15 5570 laptop and the Framework Desktop. Every release is installed and checked on both.

### Dell Inspiron 15 5570

The Dell Inspiron 15 5570 with a Core i7-8550U was the first real-hardware target. The table comes from Dell's factory configuration for this unit, which its owner shared. The Ethernet chip is not in that list; it comes from published 5570 specs ([LaptopMedia](https://laptopmedia.com/laptop-specs/dell-inspiron-15-5570-667/review/), [Ubuntu certification for the sister 5580](https://ubuntu.com/certified/platforms/9412)) and still needs a check with `lspci -nn` from any Linux live USB.

| Component | This unit | Work in the plan | Phase |
| --- | --- | --- | --- |
| CPU and memory | Core i7-8550U, 4 cores and 8 threads, VT-x and VT-d; 8 GB DDR4-2400 in one slot | SMP, PCID, SMEP and SMAP. No CET; the Framework covers shadow stacks for Phase 23 | 1, 2 |
| Firmware | Dell UEFI, factory-set to UEFI boot with GPT; likely offers a Legacy boot option and Intel PTT (TPM 2.0) | GRUB UEFI and BIOS paths; TPM in Phase 23 | 0, 1, 23 |
| Storage | 128 GB Samsung PM871b M.2 SATA SSD and a 512 GB TEAM T253 2.5-inch SATA SSD, in place of the factory SK hynix SC311 SSD (Windows 10) and 1 TB 5400 rpm hard drive; shipped in Intel RST "RAID On" mode | AHCI driver first; NVMe is covered by the Framework | 10 |
| Ethernet | Realtek RTL810x 10/100 Fast Ethernet, PCI ID 10ec:8136 (from published specs, to confirm) | Realtek r8169-family driver; a USB 3 gigabit adapter for speed tests | 12 |
| Wi-Fi and Bluetooth | Qualcomm QCA9377 (Dell Wireless 1810) on an M.2 card | Post-1.0 | 24 |
| Graphics and display | Intel UHD 620 only, no discrete GPU; 15.6-inch 1920x1080 AU Optronics eDP panel, non-touch | UEFI GOP text console at 1920x1080 in 1.0; Lumen in 1.1; native Intel driver post-1.0 | 1, 17, 24 |
| Keyboard and hotkeys | US keyboard on the i8042 PS/2 controller. Dell's driver list includes the Intel HID event filter, so the power button and hotkeys probably arrive through it | i8042 driver; Intel HID ACPI device (INT33D5) handled with the power button | 9, 11 |
| Touchpad | Dell's driver list includes Intel Serial IO, the I2C controller driver, so HID over I2C is very likely | Intel LPSS I2C, Intel GPIO and I2C-HID drivers | 17 |
| USB | Intel xHCI with USB 3.1 Gen 1 and USB 2.0 ports | xHCI, hubs, HID, mass storage, debug console | 11 |
| Other devices | Realtek ALC3246 audio, Realtek USB card reader, 720p USB camera, Goodix fingerprint reader, 42 Wh battery | Battery status in 1.0; the rest post-1.0 | 11, 24, 27 |

#### Firmware setup before the first boot

The handbook's "Creating USB install media" chapter gets a page for this laptop with these steps:

1. Turn off Secure Boot (signed boot returns in Phase 23).
2. Set **SATA Operation** to **AHCI** instead of **RAID On**. Dell ships Intel RST RAID mode, which hides the SSD from other operating systems. If Windows is installed and should keep booting, switch it to AHCI first by booting once into Safe Mode.
3. If the setup offers a Legacy boot list, enable it to test the BIOS path on the same laptop; otherwise BIOS boot is tested in QEMU only.
4. Boot the USB stick from the F12 one-time boot menu.

#### Debugging without a serial port

Neither machine has a serial port, so three tools are folded into the phases above:

- **Phase 1:** a full-screen framebuffer panic report, with the log kept in reserved memory across a warm reboot so the next boot can print it.
- **Phase 11:** a console over the xHCI Debug Capability, using a USB 3 A-to-A debug cable to a second computer.
- **Phase 12:** `netlog`, which streams kernel and service logs over UDP to another machine on the network.

#### Test rhythm and laptop scope

- The latest USB image is smoke-booted on the Dell at the end of Phases 1, 2, 6 and 9, then at the end of every later phase. Results go in the handbook's Supported hardware chapter (`docs/handbook/src/hardware.md`), the first entry of the supported-hardware list.
- In 1.0 the laptop gets the power button, a clean power-off, battery and AC status, and CPU frequency scaling. The lid lock and brightness keys arrive with Lumen in Phase 17, and suspend with Phase 27.

Decided: Dukovo installs onto the 128 GB SSD with the guided layout, erasing Windows 10. The guided install leaves the 512 GB SSD untouched; it can be formatted later with rdisk and used under /shared or /volumes. Anything worth keeping on the SSD needs backing up before the first Phase 14 install.

### Framework Desktop

The Framework Desktop is the second reference machine, and every one of its devices gets a driver. Framework publishes the mainboard's specifications and its embedded controller firmware is open source, which makes it a good fit. It also serves as the bare-metal test server for Phase 16, the multi-core performance machine (its Zen 5 cores match the CI runners), and the AMD GPU machine that Phase 29 gaming needs. Parts come from Framework's published specifications and Notebookcheck's review, and get confirmed with `lspci -nn` and `lsusb` from a Linux live USB.

| Component | This unit | Work in the plan | Phase |
| --- | --- | --- | --- |
| CPU and memory | AMD Ryzen AI Max+ 395 (Zen 5, 16 cores and 32 threads, AMD-V and AMD-Vi); 128 GB LPDDR5x-8000 shared with the GPU | SMP to 32 threads, amd-pstate (CPPC), Zen 5 machine-check banks, AMD-Vi IOMMU; scaling tests; CET shadow stacks (AMD has no indirect-branch tracking); AMD-V host | 1, 2, 11, 23, 32, 35 |
| Firmware | Insyde UEFI (closed source), UEFI boot only, AMD firmware TPM (fTPM) | GRUB UEFI path; BIOS boot is tested in QEMU and on the Dell; fTPM for measured boot | 1, 23 |
| Storage | Two M.2 2280 NVMe PCIe 4.0 slots | NVMe driver; Btrfs RAID1 across both drives | 10 |
| Ethernet | Realtek RTL8126 5 GbE | Realtek-family driver, written with OpenBSD's ISC-licensed rge(4) as reference; its firmware file is in the optional firmware package | 12 |
| Expansion slot | One PCIe 4.0 x4 slot (not reachable in the standard case without a riser) | An Intel I350 or X550 card tests the igb and ixgbe server NIC drivers | 12 |
| Platform | AMD FCH (SMBus, GPIO, I2C, watchdog), SMU temperature and power sensors, the Framework embedded controller (open-source Chromium EC fork, BSD-3-Clause) driving the fans, power LED and ARGB header | FCH drivers, k10temp and SMU sensors, the hardware watchdog, an EC driver for fan curves, LEDs and ARGB | 11 |
| USB | Two USB4 (40 Gbps) Type-C ports, two USB-A 3.2 Gen 2 ports, two front expansion card slots | xHCI, hubs and mass storage in 1.0; the USB4 connection manager and tunneling (displays, PCIe, networking) later | 11, 24 |
| Graphics and display | Radeon 8060S (RDNA 3.5, 40 compute units), HDMI 2.1 and two DisplayPort 2.1 outputs | GOP text console in 1.0; amdgpu (MIT) through the GPU shim with RADV, display engine for all three outputs, variable refresh rate | 1, 24 |
| NPU | AMD XDNA 2 neural processing unit | Our own driver, since Linux's amdxdna is GPL-2.0; AMD's XRT userspace is Apache-2.0; the local AI service uses it alongside Vulkan on the GPU | 28 |
| Wi-Fi and Bluetooth | AMD RZ717 (MediaTek MT7925), Wi-Fi 7 and Bluetooth 5.4 | Port of the ISC-licensed mt76 driver; MediaTek Bluetooth over USB; firmware in the optional package | 24 |
| Audio | AMD HD Audio controller with a Realtek ALC623 codec, rear combo jack and front audio header; HDMI and DisplayPort audio through the GPU | HDA and ALC623 for screen-reader speech in 1.0; full audio and display audio later | 1.0, 27 |
| Power | 400 W internal power supply, no battery | Power button and clean power-off; s2idle sleep later | 11, 27 |

What stays closed: the Insyde UEFI firmware, AMD's PSP and SMU firmware, and the firmware files that the GPU, Wi-Fi, NPU and Ethernet chips load. All of them are redistributable, are listed in the license manifest, and (except the UEFI firmware itself) live in the optional firmware package outside /system. Every driver we write for this machine is open source under Dukovo's license.

#### Firmware setup and debugging

1. Turn off Secure Boot in the Framework's UEFI setup (signed boot returns in Phase 23).
2. Boot the USB stick from the one-time boot menu (F12).
3. With no serial port, debugging uses the framebuffer panic report, netlog over the 5 GbE port, and the xHCI debug console where the AMD controller supports it.

The Framework is smoke-booted at the end of every phase from the moment it is set up, and its results join the Dell's in the Supported hardware chapter.

## Release 1.0 acceptance: a working system

Dukovo 1.0 ships when every item below passes, automated in QEMU where possible and by hand on the two reference machines, the Dell laptop and the Framework Desktop.

**Build and media**

- [ ] On a clean Linux host, `./ob setup && ./ob image` produces `dukovo-1.0-x86_64.img` with no manual steps.
- [ ] Two builds of the same commit are bit-for-bit identical. Boot to login takes under 10 seconds on both reference machines, the idle console system uses under 256 MB, and the seven-day soak test passes.
- [ ] `./ob write-usb` writes and verifies a stick that boots the Framework Desktop and the Dell in UEFI mode, and boots the Dell in legacy BIOS mode if its setup offers Legacy boot (otherwise BIOS boot is checked in QEMU).

**Installation**

- [ ] The guided install erases a disk and installs; the system reboots into the installed copy without the stick.
- [ ] A manual install with `rdisk` (custom partition sizes, existing partitions kept) succeeds.
- [ ] An unattended install from `install.json` succeeds in CI under UEFI and BIOS.
- [ ] The time zone chosen in the installer is honored, including daylight saving changes.
- [ ] A server install done entirely over a serial or xHCI debug console, with no screen attached, succeeds, and a mirrored two-disk install survives losing either disk.
- [ ] A cloud image boots on DigitalOcean, takes its hostname and SSH key from first-boot setup, and is reachable over SSH within 60 seconds (by hand, before release).

**Spec 16.7 first usable system**

- [ ] Boots to a Nukesh login; the administrator logs in.
- [ ] The user creates files under `~/development` and `~/documents`.
- [ ] A bundle copied into `/applications` or `~/applications` asks for its permissions, receives only what was approved, and launches.
- [ ] Pipes, redirection and job control work in Nukesh; `/bin/sh` passes the POSIX shell conformance suite, and the real-world script corpus gives the same results as under bash or dash.
- [ ] Every command in the administration command set runs and has a man page, and the systemctl, journalctl, sudo and crontab front ends handle their everyday verbs.
- [ ] A USB drive appears under `/volumes`, accessible according to capabilities, and ejects safely.
- [ ] All TCB code is memory-safe or has documented, reviewed `unsafe` sections.

**System services**

- [ ] DHCP over wired Ethernet, DNS resolution, `curl https://` and time synchronization all work.
- [ ] A bundle without network permission cannot open a socket.
- [ ] Logs are written as text or JSON lines under `/system/state/log` and readable only with the log capability.
- [ ] Swap is active and the system survives memory pressure. A user logs in over SSH with a key, and the session has only that user's capabilities.
- [ ] KASLR offsets differ across boots, and every process can draw from getrandom.
- [ ] sshd is off on a fresh install; the packet filter denies inbound connections by default, and enabling sshd opens only port 22. Repeated failed logins are slowed down, SSH accepts only keys by default, and ssh hostname.local reaches the machine.
- [ ] A USB Ethernet adapter (CDC-ECM, CDC-NCM or RTL8153) gets an address by DHCP.
- [ ] elevate runs one command with the admin set after the password is re-entered, and the use appears in the audit log.
- [ ] Battery level and AC status are reported, the CPU scales its frequency, and shutdown, reboot and poweroff work. At the critical temperature the system shuts down cleanly; when memory runs out, user apps stop before system services.
- [ ] A network capture during a day of idle use shows only update checks, and nothing once checks are off.
- [ ] A catalog bundle installs with bundle install and updates through update.

**Updates and recovery**

- [ ] A signed update installs atomically; an unsigned one is refused.
- [ ] A deliberately broken update rolls back automatically.
- [ ] A snapshot of `/users` is created, listed and restored.
- [ ] Tampered or older update and catalog metadata are refused.
- [ ] A delta update from the release candidate installs.
- [ ] Nothing installs and the machine never restarts without the user's action; update checks can be turned off.
- [ ] The Recovery boot entry starts the previous slot, and the reset keeps /users and /shared.
- [ ] A backup to a USB disk is restored onto a fresh install. Snapshot retention prunes old snapshots, and deleting files works on a full disk.

**Server platform and web console**

- [ ] From another machine's browser, the web console signs in, shows live graphs, restarts a service, adds a firewall rule, creates and restores a snapshot, and applies an update with a reboot at a chosen time.
- [ ] Signed in as a non-admin user, the web console cannot stop a system service until the password is entered again, and the attempt appears in the audit log.
- [ ] nginx serves a site over HTTPS with a certificate that certd obtained and renewed, against a local test ACME server (Pebble).
- [ ] PostgreSQL passes its own regression suite (`make check`), and Valkey, SQLite and PHP-FPM pass their acceptance tests.
- [ ] A service that crashes is restarted by Spark within its policy, and one that exceeds its memory limit is stopped without affecting others.
- [ ] Btrfs RAID1: pulling one of two disks in QEMU keeps the system running, the web console shows the degraded array, and adding a new disk rebuilds it with `btrfs check` clean.
- [ ] The hardware watchdog reboots a hung machine in QEMU, and the boot after it is healthy; logs forward to a remote syslog server over TLS.
- [ ] A seven-day soak test serving nginx and PostgreSQL under load ends with no crashes, no memory growth and no errors from a Btrfs scrub.
- [ ] Performance gates against Linux on the same hardware: nginx requests per second (wrk), PostgreSQL transactions per second (pgbench) and network throughput (iperf3), on the Framework's 5 GbE port. Slower than Linux counts as a bug, as with the kernel benchmarks.
- [ ] `dukovo-check` on a fresh server install reports no warnings, and unattended security updates, when turned on, install and reboot only in the window the admin chose.
- [ ] On the Framework Desktop, both NVMe drives, the 5 GbE port, the USB ports, the fans and sensors, and the watchdog work.

**Accessibility**

- [ ] Using only speech or a braille display, a user can turn on the screen reader at the boot menu, install Dukovo, log in and use Nukesh, on the Dell and on the Framework's ALC623 audio.
- [ ] The web console meets WCAG 2.2 AA and works fully from the keyboard and with a screen reader.
- [ ] Windows-style shortcuts work in nano, including Ctrl+C copying a selection and interrupting otherwise.

**Documentation**

- [ ] `./ob docs --check` passes: every command, service, file format and interface has a man page.
- [ ] The handbook covers building, media creation, installation, administration, the web console, services, cloud images, hardening and recovery.
- [ ] /system/resources/licenses lists every component, and the GPL and LGPL sources are published with the image.
- [ ] The release notes state the known limits: no graphical desktop (Lumen arrives in 1.1), no Wi-Fi, Bluetooth, sound beyond screen-reader speech, suspend, GPU acceleration, disk encryption, containers (Podman arrives in Phase 20) or Go-based software (Phase 18). SECURITY.md, the update server privacy policy and the code of conduct are published.

## Post-1.0 roadmap

After 1.0, twenty-one more phases (17 to 37) close every remaining spec requirement and add the desktop, the app platform, containers, more server services, gaming, a local AI assistant, self-hosted developer tools and a full set of first-party apps. They were renumbered on 2026-10-23 so that the numbers follow the recommended order. Two lines run side by side, and each phase starts only after the one before it in its line ships:

- **Desktop line:** 17 Lumen (Dukovo 1.1), 19 UI kit, Store and apps, 21 developer tools, 22 WebAssembly, 24 hardware breadth (GPU, Wi-Fi, Bluetooth), 26 personality shell, 27 audio, printing and power, 28 local AI, 29 gaming, 30 Mail, Calendar and Contacts, 31 everyday apps.
- **Server line:** 18 Go, 20 containers and Podman, 25 server services and fleet management, 32 virtualization host, 33 observability and live patching, 35 scale and real time.
- **Any time, when people are available:** 23 platform security, 34 AArch64 (needed for full spec 15 conformance), 36 confidential computing, 37 RISC-V.

| Phase | Theme | Main work | Spec |
| --- | --- | --- | --- |
| 17 | Lumen compositor (Dukovo 1.1) | The Lumen compositor as specified in its own section, moved out of 1.0 on 2026-10-23: display layer, inputd, the Dell's I2C-HID touchpad, terminal.bundle, the Lumen fonts, lock screen, lid, backlight and battery display, and VirtualBox as a guest. The full UI/UX design session with the maintainer comes first. Lumen joins the default image from this release on, and the server image stays console-only | 7.4, 7.14, 7.16 |
| 18 | Go toolchain port | Go (BSD-3-Clause) as a new target, linking through mlibc as Go does on OpenBSD and Solaris rather than making raw system calls, with the Go test suite passing. One port unlocks a large share of modern server software: Podman (Phase 20), Caddy, Prometheus, Forgejo, many cloud tools, and the micro editor. Needs only the 1.0 POSIX layer, so it starts right after 1.0, alongside Lumen | 13 |
| 19 | UI kit and applications | The Dukovo UI kit, built on Iced (MIT), is the only toolkit in /system: the shell, settings and every app we ship use it, always with Lumen's title bar, and Wasm apps reach it through WIT. Lumen requests server-side decorations from every app; an Dukovo Wayland extension lets apps that want a custom title bar declare its region and buttons so Lumen can restyle it per personality; a user-controlled per-app exception list covers the rest. Our own maintained ports of major apps (Chromium or Firefox first, later office and media apps) in ports/ with the system title bar on. GTK and Qt never enter /system: they exist only as private libraries inside third-party bundles, and the SDK ships patched builds of both with the title-bar patch and theme hooks. Catalog bundles must honor the title-bar toggle. The UI kit renders through wgpu on Vulkan, with a software fallback until GPU drivers exist. The Dukovo Store, built with the UI kit, is the graphical front end for the catalogs and the unified updater: browse, search, screenshots, ratings, one-click install, the permission prompt, and an Updates tab for system and apps together (in the spirit of the macOS App Store). A Settings app built with the UI kit, with a page for every system toggle: updates, privacy, AI, title bars, personalities, keyboard and network. Desktop integration: clipboard, drag and drop, Open with defaults, and capability-gated portals for file pickers, screenshots and screen sharing. Accessibility: AccessKit (MIT/Apache) in the UI kit, a magnifier, high contrast, keyboard-only navigation, and the 1.0 screen reader extended to every UI kit app through AccessKit. Translations through Fluent (Apache/MIT), right-to-left text through cosmic-text. Optional XWayland | 7.4, 13 |
| 20 | Containers: Linux compatibility core and Podman | A server subset of the Linux binary compatibility layer, outside the TCB: Linux system calls for 64-bit glibc and musl programs (files, sockets, epoll, futex, signals, threads, basic /proc and /sys) mapped onto Radix capabilities, as FreeBSD's Linux layer does. Our own OCI runtime in Rust on resource domains and capability sandboxes (the role runc and crun play on Linux; crun is GPL, so we write ours), image storage on Btrfs subvolumes, and rootless networking. Then Podman (Apache-2.0, written in Go) with podman compose; Podman is daemonless and rootless by default, which fits Dukovo's no-root capability model, and it accepts Docker's commands and API, so most Docker scripts and compose files keep working. Docker itself is not planned. Containers appear in the web console. Phase 29 extends the compatibility layer for desktop programs and games. Needs Phase 18 | 4.1, 13 |
| 21 | Developer tools and SDK | Native developer tools on Dukovo: ports of Rust and Cargo, LLVM and Clang, git, LLDB, CMake, Meson, Python and Node, ending in the milestone where Dukovo builds itself with ./ob. One SDK with templates for native and Wasm bundles, ridlc and WIT bindings, and developer docs. Developers generate their own signing keys; the store records publisher identity, with no paid notarization. All ported tools are permissive or GPL shipped as separate programs. Driver Kit and driver agent: a Rust driver framework with a template per device class, a hwprobe tool that writes a hardware report (PCI and USB IDs, descriptors, ACPI tables), QEMU device models and record-and-replay tests, and a test loop that builds, loads, exercises and restarts a userspace driver without rebooting. On top sits a driver expert agent: a knowledge pack (Radix driver interfaces, every in-tree driver, device-class specifications) plus an MCP server that exposes hwprobe, build, load, test and logs, so any agent can use it. By default it runs on the local AI service (Phase 28); a developer may connect a coding agent of their own choice instead, since this is a developer tool and nothing leaves the machine unless the developer sets that up. Because a driver under development runs as a userspace process (and ships either that way or as a restartable compartment), a bad attempt crashes and restarts instead of taking the machine down, which makes AI-assisted iteration safe. License guard: the agent records where every line comes from. A driver written from datasheets and specifications can be permissive; one translated from a GPL Linux driver stays GPL and ships as a separate program with its source. Drivers submitted to the catalog are signed, reviewed by a person and listed in the hardware compatibility list. | 12.1, 13, 14 |
| 22 | WebAssembly app platform | Dukovo's answer to .NET: one portable binary format and one toolset, any source language. Wasmtime runs as a Spark service; bundles may ship .wasm components instead of native binaries. WASI is capability-based, so each WASI handle is backed by a Radix capability and a bundle's manifest grants become exactly its WASI imports. ridlc also emits WIT files, so every Radix interface is callable from every Wasm language. Components are compiled ahead of time at install, so no JIT runs and W^X holds. An SDK with ./ob templates for Rust, C and C++, Go, C#, Python and JavaScript, plus Wayland bindings so Wasm apps can draw in Lumen. Needs only the bundle runtime from Phase 13, so it can start right after 1.0. A .NET (CoreCLR) port on mlibc stays possible later but is not the primary runtime. Wasm apps get GPU access through WebGPU (wgpu on Vulkan). | 7.1, 13, 14 |
| 23 | Platform security | Secure Boot with user-enrolled keys (sbctl or MOK), measured boot with TPM 2.0, remote attestation, root-filesystem encryption unlocked by passphrase or TPM, CET shadow stacks (the Framework's Zen 5 CPU has them) and Intel indirect-branch tracking (needs an 11th gen or newer Intel test machine; the Dell's 8th gen CPU lacks CET), wider audit logging | 8 |
| 24 | Hardware breadth | Wi-Fi with a WPA supplicant: the Framework Desktop's MediaTek MT7925 (Wi-Fi 7) through a port of the ISC-licensed mt76 driver, and the Dell's Qualcomm QCA9377, which needs Qualcomm's redistributable but closed firmware file; Bluetooth, including MediaTek Bluetooth over USB; the USB4 connection manager with display, PCIe and network tunneling; GPU drivers with Vulkan as Dukovo's one native graphics API: Linux's amdgpu and i915 kernel drivers run through a compatibility shim as userspace driver processes (they are mostly MIT-licensed, and running them as separate processes keeps any GPL-only parts out of the kernel), Mesa's Vulkan drivers (RADV, ANV) on top, starting with the Framework's Radeon 8060S (RDNA 3.5) and its HDMI 2.1 and DisplayPort 2.1 outputs, then the Dell's UHD 620, OpenGL only through Mesa's Zink on Vulkan, and Lumen switches to a Vulkan renderer with direct scanout and variable refresh rate, PCIe and memory hotplug; a Wi-Fi picker and firewall rules in Settings | 3.1, 6.1, 6.2 |
| 25 | Server services and fleet management | More services as catalog bundles: MariaDB (GPL-2.0), Unbound (BSD-3-Clause), Samba (GPL-3.0), a mail server with Postfix (EPL-2.0 or IPL-1.0) and Dovecot (MIT and LGPL-2.1), Node.js (MIT; needs V8's JIT memory support), and the Go-based Caddy (Apache-2.0), Prometheus (Apache-2.0) and Forgejo (GPL-3.0). Grafana is AGPL-3.0, so it is a catalog entry only, never part of the base system. The web console manages several Dukovo machines from one place, as Cockpit does, with hooks for configuration management tools such as Ansible over SSH. GPL services ship only as separate catalog bundles. Needs Phases 18 and 20 for the Go and container parts | Beyond the spec |
| 26 | Personality shell (optional) | An optional desktop shell built from scratch with the Dukovo UI kit. It is assembled from components (taskbar, dock, start menu or launcher, overview, top bar, notifications, decorations, animations, keybindings, workspaces), and each personality is a config that picks and themes them: Windows 7, GNOME default, Hyprland and macOS styles first. Window management is a pluggable policy module in Lumen (stacking or dynamic tiling). Users switch personalities without logging out; only the shell restarts. Personalities can ship as sandboxed Wasm components (Phase 22). A graphical login screen (greeter) built with the UI kit signs in through authd and follows the active personality. A global menu bar needs apps to export menus; the UI kit does, others keep in-window menus. Layout and behavior are copied, never Microsoft or Apple icons, fonts or artwork. Input methods for Chinese, Japanese and Korean through Fcitx5 (LGPL, a separate program) integrate with every personality. Needs Phases 17, 19 and 22. | 7.4, 7.14, 14 |
| 27 | Audio, printing, power | HDA, virtio-snd and USB audio with `audiod`; CUPS port behind a capability wrapper; ACPI S3 suspend, suspend-to-disk to swap, runtime device power management, battery and thermal reporting, s2idle sleep on the Framework Desktop, and HDMI and DisplayPort audio | 7.9, 7.10, 7.15 |
| 28 | Local AI assistant (optional) | An optional package outside /system, chosen at install or in settings; when disabled it is not running and holds no capabilities. Fully local: the AI service is granted no network capability, so the kernel prevents any data leaving the machine; models arrive through the unified updater. Inference through llama.cpp (MIT) with GGUF models on the CPU, and its Vulkan backend once Phase 24 lands; on the Framework Desktop the Radeon 8060S with up to 128 GB of shared memory runs large models, and our own XDNA 2 NPU driver (Linux's amdxdna is GPL-2.0, AMD's XRT userspace is Apache-2.0) runs small ones at low power. Users pick models from the store, which shows each model's license; the default model is Apache-2.0 or MIT licensed (for example Qwen, Mistral or Phi), and restricted licenses such as Llama's and Gemma's are labeled. Per-user data store held only by the AI service, encrypted with the user's login key, readable by no other app or admin. The memory log is off by default; when on, the user can view, edit, export and delete entries, and the store is excluded from snapshots so forgetting is real. Interfaces: an ask command in Nukesh and an assistant panel in Lumen; any command it proposes runs only after the user approves it. Access is system-only by default; a user toggle lets third-party bundles request an ai.ask permission, which sends prompts and receives answers but never sees the memory log or other apps' conversations. Every first-party app (Mail, Calendar, Contacts, the Store, Settings, the terminal, the shell and every later app we ship) has full AI support through one shared assistant API in the UI kit: summarizing, drafting, plain-language search and suggested actions, each confirmed by the user. First-party apps count as system tools, so they need no third-party toggle; when AI is disabled these features are hidden and every app works fully without them. Voice: talking to the machine should feel normal. A system push-to-talk hotkey turns the microphone on and off; a taskbar voice toggle is part of every personality shell from the start; an optional wake phrase, like Siri's, can start listening hands-free. Speech recognition runs locally through whisper.cpp (MIT) and replies are spoken through Piper (MIT) or eSpeak NG. Voice commands drive the assistant, dictate into any text field, and control the system, which also serves users who cannot use a keyboard or mouse. Privacy: the wake phrase is off by default; only the voice service holds the microphone capability while it listens; a microphone indicator is always shown while it is on; audio stays on the device and is not stored unless the user turns on history. Wake-word models must have licenses that allow commercial use, so we train our own rather than ship openWakeWord's non-commercial models. Needs Phase 27's audio input; the taskbar toggle arrives with Phase 26. Ships in two steps: first the ask command (needs Phases 13 and 15), then the Lumen assistant panel (needs Phase 19). The encrypted store and memory log use Phase 23's key handling, so the memory log arrives with or after Phase 23. Best with Phase 24. | 2.3, 8, 14 |
| 29 | Gaming and full Linux compatibility | Goal: run Steam, Proton and native Linux games. The Linux binary compatibility layer from Phase 20, extended outside the TCB for desktop programs, translating Linux system calls (futex, epoll, io\_uring basics, namespaces for the Steam Runtime) onto Radix capabilities, with glibc-linked 64-bit and 32-bit programs, as FreeBSD's Linux layer does. Games use Vulkan from Phase 24; DirectX runs through DXVK and VKD3D-Proton as in Proton. Gamepads (USB and Bluetooth HID, including Xbox, PlayStation and Steam controllers), low-latency audio from Phase 27, a game-aware scheduling hint, and Lumen fullscreen direct scanout with variable refresh rate. Games with kernel-level anti-cheat stay unsupported, as on Linux. Builds on the compatibility core from Phase 20. Needs Phases 27, 24 and 19; the Framework Desktop (Radeon 8060S) is the AMD GPU test machine. | 4.2, 6.1, 13 |
| 30 | Mail, Calendar and Contacts | First-party Mail, Calendar and Contacts apps built with the UI kit, in the spirit of macOS. A per-user personal data service, pimd, owns the mail store, calendars, reminders and the address book, encrypted and sealed per user. It offers them to other apps through versioned RIDL interfaces and WIT bindings, like Apple's EventKit and Contacts frameworks: an app can read or add events and contacts, open a prefilled compose window or hand over attachments, but only with a per-category permission the user grants and can revoke in Settings. Accounts: IMAP, SMTP and JMAP for mail, CalDAV and CardDAV for calendars and contacts, OAuth sign-in for Gmail and Outlook.com, and iCalendar and vCard import and export; Exchange through Microsoft Graph later. Features: unified inbox, conversation threads, offline full-text search, rules, invitations and free/busy, time zones from tzdata, reminders through notifyd, OpenPGP encryption (S/MIME later), and remote images blocked by default. Libraries are permissive Rust crates (mail-parser, mail-builder, lettre, async-imap, calcard, tantivy, rPGP), each checked by cargo-deny. Gmail sign-in requires Google's OAuth app verification and a yearly security assessment, which must be budgeted before release. Data stays on the device and syncs only with accounts the user adds, with no telemetry. Full AI support, as in every first-party app: summarize a thread, draft or rewrite a reply, find a meeting time, turn an email into an event or contact, and search mail in plain language, always run locally and confirmed by the user. Needs 19, 22 and 28. | Beyond the spec |
| 31 | Everyday apps | The rest of the first-party app list: Notes, Paint, the torrent client, the FTP client and the games pack (see First-party apps). Built with the UI kit, with full AI support like every first-party app. Needs 19 and 28; Notes links to Phase 30 data when present. | Beyond the spec |
| 32 | Virtualization host | VT-x and AMD-V host support with a Radix VMM API and a VMM built from rust-vmm crates (AMD-V with nested paging on the Framework Desktop, VT-x on the Dell); the web console manages virtual machines. Containers moved to Phase 20 | 4.1, 6.2, 13 |
| 33 | Observability and live patching | Dynamic probes with a verified safe bytecode, hardware performance counters, crash dumps to reserved memory with a post-mortem tool, kernel live patching | 9, 10, 15 |
| 34 | AArch64 | HAL for AArch64, GICv3, generic timer, device tree and ACPI; QEMU `virt` first, then one real board; GRUB arm64 EFI images and an AArch64 USB image from `./ob image --arch aarch64` | 1.2, 6.2, 15 |
| 35 | Scale and real time | Scaling work to 128 to 256 cores (starting from 32 threads on the Framework Desktop), deadline scheduling class, tail-latency tuning, programmable packet path (XDP-style), RDMA, zero-copy networking, persistent memory | 4.2, 6.3, 11 |
| 36 | Confidential computing and CXL | Intel TDX and AMD SEV-SNP guest support, Arm CCA, CXL memory tiering | 6.2, 8, 14 |
| 37 | RISC-V | HAL for RISC-V (SBI, PLIC or AIA), QEMU `virt`, then one real board | 1.2, 6.2 |

A Linux system-call compatibility layer (spec 13) is built outside the TCB once the native ABI is stable: a server subset in Phase 20 for containers, extended in Phase 29 for desktop programs and games.

### First-party apps

The apps we build and ship ourselves (decided on 2026-10-21). All of them use the Dukovo UI kit, always show Lumen's title bar, have full local AI support through the shared assistant API (Phase 28), and work fully with AI turned off. Their look follows the UI/UX design language the maintainer approves (see Decisions). Every library named here is permissive unless marked, and cargo-deny checks each one. System tools come first: within each phase, the tools that manage and look after the machine (the first eleven rows) are built before everyday apps, and the everyday apps of Phase 31 come last.

| App | Phase | Scope and notes |
| --- | --- | --- |
| Control Center | 19, with panels added as hardware support lands | Grows the Settings app into a full control center: backgrounds, mouse and touchpad, keyboard, default apps, users, privacy, updates and AI. Driver manager: drivers in use, the optional firmware package, driver process restarts. Sounds and printers (Phase 27). Displays with multi-monitor arrangement, scaling and refresh rate (Phase 24, which brings Lumen multi-output). Network, Wi-Fi and Bluetooth (Phase 24). Power and battery. |
| System monitor | 19 | Our own monitor, modeled on the features of TMOG. TMOG is proprietary, so we take its ideas only, never its code, name or artwork. Goal: one glance names the process and the subsystem slowing the machine down right now. Views: summary, performance, processes (a sortable, filterable live hierarchy), system info, startup items (Spark services and login items), users, services, power and frequency, network connections, installed bundles, disk space, and benchmarks. It compares CPU, memory (pressure, compression, cache, swap and trend), storage (each device or the whole path combined), network (live bandwidth, history and per-connection detail), GPU, temperatures and energy (watts, power state and the processes responsible). Performance and efficiency cores are shown separately, with logical-processor and NUMA views. Honest measurement: a zero is never shown unless measured, unavailable sensors say so, and stopped sampling leaves a gap in the graph. Process actions use a Radix process handle, not a bare PID, so a recycled PID can never redirect an action. A flight recorder records, saves and replays a slow hour. Optional self-monitoring shows the monitor's own cost. Themes: system light and dark, plus green, amber, blue and mono phosphor looks. Radix extras: the capabilities each process holds, and the restartable kernel compartments with their restart history. No telemetry. The energy view needs Phase 11's ACPI work and RAPL counters; the GPU view needs Phase 24. |
| System info | 19 | Hardware, OS version, drivers in use, and the license manifest from /system/resources/licenses. |
| Disks | 19 | A disk and media manager over rdisk, volumed and snap: partitions, formatting, SMART health and snapshots. A USB formatter and image writer with the same typed confirmation and verification as ./ob write-usb. ISO and disk-image mounting through volumed, with new read-only ISO 9660 and UDF filesystem servers in Rust. |
| Disk usage analyzer | 19 | Treemap and sunburst views. Btrfs-aware, so shared extents and snapshots are not counted twice. |
| Notification center | 19 | Builds on notifyd and Lumen's notifications: history, actions and inline replies, per-app permission, and do-not-disturb schedules. Every personality shell (Phase 26) uses it. |
| Password and key manager | 23 (service), 19 (app) | A system keychain, like macOS Keychain, sealed with the user's login key: passwords, passkeys (WebAuthn), SSH and OpenPGP keys, Wi-Fi passwords and certificates. Apps get items only with a per-app permission the user grants. Autofill in our browser ports. Import from CSV and KeePass files. Passkeys are supported but never forced: the system never requires or nags about them, and passwords keep working everywhere, including login, elevate and the keychain itself. |
| Virtual keyboard | 19 | On-screen keyboard through Lumen's input-method protocol, for accessibility and touchscreens. The input methods from Phase 26 plug into it. |
| Screen capture and recorder | 19 | Screenshots of a region, a window or the screen; screen recording through the capability-gated screen portal. Video is AV1 or VP9 in WebM (rav1e, BSD-2-Clause; libvpx, BSD-3-Clause), because H.264 and HEVC encoders carry patent licensing. Recorded audio arrives with Phase 27. |
| Bluetooth manager | 24 | Pairing, device list, battery levels and file sending. |
| Fingerprint | 24 (driver), 19 (manager) | libfprint (LGPL-2.1) ported as a separate, dynamically linked driver process, with its source published. Fingerprint templates are encrypted on the device and never leave it. Used for login, unlocking and elevate; a password always works too. |
| Text editor | 19 | Graphical editor on cosmic-text with tabs, syntax highlighting (syntect, MIT), find and replace, and Windows-style shortcuts. nano stays the console default. |
| Image preview | 19 | Quick preview from the file manager with the space bar, like macOS Quick Look, plus a full viewer. PNG, JPEG, GIF, WebP, AVIF and SVG through the image and resvg crates. HEIC is left out by default because of its patent licensing. |
| Calculator | 19 | Standard, scientific and programmer modes, with offline unit conversion. |
| Markdown viewer and editor | 19 | Edit with a live side-by-side preview through pulldown-cmark (MIT). The same component renders Markdown in Notes and in the help pages. |
| PDF viewer and Save as PDF | 19 | Fast viewing of large documents with thumbnails, outline, search, text selection and copy, zoom and two-page layouts; filling in PDF forms; highlights, notes and freehand annotations saved into the file; adding a drawn or typed signature; printing; reading mode for the screen reader. Rendering through PDFium (BSD-3-Clause, the engine in Chromium), with the pure-Rust hayro renderer (MIT/Apache) evaluated as a replacement; MuPDF is ruled out because it is AGPL. Quick preview of PDFs uses the same engine. Save as PDF: a built-in Save as PDF printer is always present in the system print dialog, even with no printer installed, so every app can print to PDF. Our print pipeline produces PDF natively through krilla and pdf-writer (MIT/Apache), with selectable text, embedded fonts, links and PDF/A for archiving. Real printers (Phase 27) receive the same PDF. |
| Notes | 31 | Notebooks, sections and pages, in the spirit of OneNote but simpler: free placement of typed text, ink, images and checklists. Stored in an open format (Markdown plus assets), with export to Markdown and PDF. Pages can link mail and events from Phase 30. Local only; users can sync it through storage they choose. |
| Paint | 31 | A raster editor in the spirit of Microsoft Paint: brushes, shapes, fill, text, selection, crop and resize. All icons and art are our own, with no Microsoft assets. |
| Torrent client | 31 | A full client on librqbit (Apache-2.0, Rust): magnet links, DHT, selective and sequential downloads, bandwidth schedules and seeding limits. BitTorrent and its clients are legal; what users download is their own responsibility. We ship no search of third-party sites and no bundled trackers. Dukovo release images are also offered as torrents. |
| FTP client | 31 | A graphical file transfer client for FTP, FTPS (FTP over TLS) and SFTP, with WebDAV later. Site manager with passwords and keys kept in the system keychain, two-pane drag and drop, a transfer queue with resume, folder sync, and remote folders that open in the file manager. Plain FTP shows a warning that it is unencrypted and works after a confirmation. Built on the suppaftp and russh-sftp crates (permissive licenses, checked by cargo-deny), with no code from GPL clients like FileZilla. AI-ready like every first-party app. From 1.0, the console already covers this: curl handles FTP and FTPS, and sftp ships with ssh in Phase 13. |
| Games | 31 | An optional games pack, installed by default and removable in one step; once Phase 31 ships, the installer shows it as a checked box. Mines (Minesweeper rules, with custom board sizes and no-guess boards). Solitaire: one app with Klondike, Spider, FreeCell, Pyramid, TriPeaks, Golf, Clock and more. Mahjong solitaire with several tile layouts. Jigsaw puzzles from bundled photos or the user's own images, with a choice of piece count. Tetravex. Legal: game rules are not copyrightable, but names can be trademarks and art and look are copyrighted, so every game uses its own names, art, card faces and sounds, with no Microsoft or GNOME assets; GNOME's Aisleriot, Mines and Tetravex are GPL, so we write ours from scratch. All games work offline, with keyboard and screen-reader support, no ads, no accounts and no telemetry. AI-ready like every first-party app: hints and an explanation of the best move, on request only. |

#### Open media and file formats

Dukovo supports the main open, royalty-free formats natively and system-wide. One media service decodes them, so the file manager, quick preview, the notification sounds and every app (native or Wasm) can use them without bundling their own codecs. Audio arrives with Phase 27 and video playback with Phase 19. Every decoder runs in a sandboxed process with no file or network capability of its own, and is fuzzed in CI.

| Kind | Formats | Libraries and licenses |
| --- | --- | --- |
| Audio | Ogg Vorbis, Opus, FLAC, Speex, WAV, AIFF, MP3 (its patents have expired) | lewton (MIT/Apache), libopus (BSD-3-Clause), claxon (Apache-2.0), ogg (BSD-3-Clause), libspeex (BSD-3-Clause), hound (Apache-2.0), minimp3 (CC0) |
| Video | AV1, VP9, VP8, Theora | dav1d (BSD-2-Clause) and rav1e (BSD-2-Clause) for AV1, libvpx (BSD-3-Clause), libtheora (BSD-3-Clause) |
| Containers | Ogg, WebM, Matroska (MKV), WAV | ogg (BSD-3-Clause); a Matroska and WebM demuxer, permissive only, chosen in Phase 19 |
| Images | PNG, JPEG, GIF, WebP, AVIF, JPEG XL, SVG, TIFF, BMP, ICO, QOI | image (MIT/Apache), jxl-oxide (MIT/Apache), resvg (Apache/MIT) |
| Archives | ZIP (with ZIP64 and AES encryption), tar, gzip, bzip2, xz, lzma, zstd, lz4, lzip, brotli, 7z, cpio, CAB and ISO; RAR extraction only | libarchive (BSD-2-Clause) in the media service and the console tools, plus zip, tar, flate2, xz2/liblzma (public domain/BSD), zstd (BSD-3-Clause), lz4 (BSD-2-Clause), brotli (MIT), bzip2, sevenz-rust (Apache-2.0). RAR is read through libarchive's own open decoder; creating RAR files needs RARLAB's proprietary tool, so we do not. An Archive Manager app and Compress and Extract actions in the file manager arrive with Phase 19, built with the UI kit and AI-ready like every first-party app. |
| Documents and fonts | Markdown, plain text, PDF viewing, OpenType, TrueType, WOFF2 | pulldown-cmark (MIT), PDFium (BSD-3-Clause) for viewing and krilla with pdf-writer (MIT/Apache) for creating PDFs (not MuPDF, which is AGPL), swash (MIT/Apache) |

Patent-encumbered formats (H.264, HEVC, AAC, HEIC and similar) stay out of the base system, but users can add them with one checkbox, the way Ubuntu's installer offers "additional media formats". The installer's step 6 offers "Install extra media codecs" next to the firmware package, off by default, with a note that some of these formats are covered by patents in some countries; Settings and the update command can add or remove it later. It installs an optional codec package outside /system, built from the same sources Linux distributions use: FFmpeg's libavcodec in its LGPL build (decoders only, no GPL or non-free options, so no x264 or x265), libheif and libde265 (LGPL) for HEIC, and Cisco's OpenH264, downloaded straight from Cisco at install time because Cisco's patent license covers only binaries Cisco distributes. The LGPL libraries are dynamically linked into separate sandboxed decoder processes that the media service starts, with their exact source published next to each release. Hardware decoding of H.264 and HEVC through Mesa's VA-API and Vulkan Video arrives with Phase 24, enabled only when the codec package is installed. The package ships with Phase 27 (audio) and Phase 19 (video). Before its first release we get a legal review of patent exposure, and if needed we host it from a separate repository the way Fedora's codecs live in RPM Fusion. Microsoft's core fonts, which Ubuntu's restricted extras also install, are left out because of their license.

## Spec traceability matrix

Every section of spec v1.4 maps to at least one phase in this table, so a requirement cannot be silently dropped.

| Spec section | Requirement | Phases |
| --- | --- | --- |
| 2.1, 2.2 | Hybrid structure, minimal TCB | 1 to 5 (TCB), 7 and 10 to 12 (compartments) |
| 2.3 | Capability security, no ambient authority | 3, enforced in every later phase |
| 3.1 | Physical memory, zones, NUMA | 2; hotplug in 24 |
| 3.2 | Virtual memory, demand paging, copy-on-write, ASLR | 2, 7, 8 |
| 3.3 | Memory protection, IOMMU, W^X, speculative-execution mitigations | 2, 11; CET in 23; memory tagging on Arm in 34 |
| 3.4 | Allocation interfaces | 2 |
| 4.1 | Processes, threads, namespaces, resource controllers | 4, 6, 13, 16, 20, 32 |
| 4.2 | Scheduling | 4; energy-aware and heterogeneous CPUs in 34; deadline class in 35 |
| 4.3 | Synchronization, RCU | 4 |
| 5.1, 5.2 | System calls, async I/O, IPC | 5 |
| 6.1 | Driver model, userspace drivers | 7, 10 to 12; power management in 27 |
| 6.2 | HAL, architectures, virtualization, confidential computing | 1, 34, 37; 32, 36 |
| 6.3 | Storage and networking | 7, 10, 12; high-performance paths in 35 |
| 7 | Filesystems, Btrfs, swap, FAT and exFAT, ext4 | 7, 10, 11 |
| 7.1 | Hierarchy, bundles, manifests, `/volumes` | 7, 11, 13 |
| 7.2 | Nukesh and core utilities | 9; administration commands in 9 to 16 |
| 7.3 | Spark init and service management | 6, 9, 13, 16 |
| 7.4 | Lumen compositor | 17; UI kit in 19; personality shell in 26 |
| 7.5 | Logging | 1 (early log), 13; remote logging in 16 |
| 7.6 | Atomic base-system updates | 15 |
| 7.7 | Users and accounts | 13, 14 |
| 7.8 | Networking | 12 |
| 7.9 | Audio | 27 (speech output in 1.0) |
| 7.10 | Power management | 11, 27 |
| 7.11 | Time synchronization | 1 (hardware clock), 12, 16 |
| 7.12 | Documentation | 0 and every phase's definition of done |
| 7.13 | Snapshots and backups | 15 |
| 7.14 | Accessibility baseline | 9, 14, 16, 17, 19, 26 |
| 7.15 | Printing | 27 |
| 7.16 | Notifications | 13, 17 |
| 8 | Security features | 2, 13, 16, 23, 36 |
| 9 | Reliability and fault isolation | Compartment restart from 7; watchdog in 11; Spark health checks in 15 and 16; live patching in 33 |
| 10 | Observability and debugging | 1, 5, 16; completed in 33 |
| 11 | Performance | Benchmarks from 5; server gates in 16; tuning in 35 |
| 12.1, 12.2 | Tooling, CI, coding standards | 0, ongoing |
| 12.3 | Boot, GRUB 2, UEFI and BIOS | 0, 1, 14 |
| 13 | Stable ABI, compatibility, containers | 5, 8, 18, 19, 20, 22, 29 |
| 14 | Extensibility | Versioned interfaces from 5; new architectures in 34, 37 |
| 15 | Acceptance criteria | Release 1.0 plus Phase 34 (second architecture) |

## Risks and open decisions

The biggest risk is still the Btrfs implementation, now with RAID; the server-first order and both reference machines are settled, so the remaining open decisions are small.

### Risks

| Risk | Impact | Mitigation |
| --- | --- | --- |
| Btrfs written from scratch is the largest single component | Data loss, or Phase 10 running long | Read-only first; single-device profiles only; Linux `btrfs check` and power-cut tests in CI from day one. Fallback (decided): port OpenZFS through its portability layer as a userspace filesystem server |
| Real-hardware quirks in ACPI, USB and NICs | Works in QEMU, fails on real machines | Smoke boots on the Dell from Phase 1 onward, a published supported-hardware list, uACPI isolated in a compartment as a fallback AML interpreter |
| Rust `std` port drifts with nightly Rust | Broken userspace builds | Pinned toolchain, deliberate quarterly upgrades, upstream the target as tier 3 |
| smoltcp is not yet production-grade for servers | Spec 7.8 requires a production-quality stack at first release | Extend it (SACK, window scaling, CUBIC), fuzz it, and run interoperability tests against Linux peers every night |
| IPC overhead for userspace drivers | Slow I/O | Drivers run as in-kernel compartments by default; only the e1000 demonstration runs in userspace until benchmarks justify more |
| C tools assume Linux (procfs, ioctl, UID/GID) | Port breakage and patch load | Compatibility shims in mlibc; prefer Rust tools where a good one exists |
| GPL code leaking into the kernel | License conflict | cargo-deny license policy on kernel crates; GPL ports confined to `ports/` |
| Secure Boot must be disabled for 1.0 | Install friction on new PCs and cloud hosts | Handbook chapter with firmware steps; cloud images boot without it; signed boot path in Phase 23 |
| Scope versus team size | Long time to 1.0 | Each milestone is usable on its own; the console system after Phase 9 is already shareable for contributors, and the server release is useful without any graphical work |
| Post-1.0 scope growth | The post-1.0 roadmap grew from 10 to 21 phases and could stall progress | Phase numbers now follow the build order; start a phase only after the previous one in its line ships, and keep the 1.0 scope frozen |
| The web console is a network-facing attack surface | It is the most valuable target on the machine | Fuzz the API, run an outside security review before 1.0, keep it off on cloud images unless enabled, and follow the SECURITY.md disclosure process |
| Btrfs RAID adds risk to new filesystem code | Losing a disk must never lose data | RAID1 and RAID10 only, never RAID5 or RAID6; disk-pull and power-cut tests in CI on every change |
| No containers in 1.0 | Many people choose a server OS mainly to run containers | Service bundles fill the role in 1.0; the Go port (Phase 18) and Podman (Phase 20) are the first server items after 1.0 |
| Cross-process futexes | PostgreSQL's semaphores live in shared memory and need them | Fixed in the post-Phase-9 audit (ADR 0110), with two POSIX benchmark rows against Linux |
| Narrow bare-metal server hardware support | Real servers use SAS HBAs, RAID cards and server NICs | 1.0 supports AHCI, NVMe, virtio and the listed Intel and Realtek NICs, and a published hardware list sets expectations; Btrfs RAID covers what hardware RAID cards do |
| No BMC or ECC on the test machines | Serial-over-LAN, IPMI and ECC reporting go untested on real hardware | The Framework Desktop covers bare-metal server testing otherwise; borrow or buy a used server with IPMI and ECC before release if those features are to be listed as supported |
| The desktop waits longer | Some users will wait for the graphical side | Lumen is the first item after 1.0, and design sessions can start any time |

### Decisions

Twenty-two are decided and three remain open. The repository also records each one as an architecture decision record in docs/adr/.

| Decision | Default | Alternatives |
| --- | --- | --- |
| Reference machines | Decided: the Dell Inspiron 15 5570 (Core i7-8550U) and the Framework Desktop (Ryzen AI Max+ 395, 128 GB) | The Dell was confirmed from Dell's factory configuration and kept on 2026-10-22; the Framework Desktop was added on 2026-10-23 with every one of its drivers in the plan. Ethernet chips still to confirm with lspci |
| Shape of 1.0 | Decided on 2026-10-23: a headless server release with a first-class web console; Lumen becomes Phase 17 (Dukovo 1.1), and post-1.0 phases are renumbered in build order | A desktop 1.0 with Lumen (the earlier plan) |
| Web console | Decided: Dukovo Admin (`admind`), our own, on port 9090; on by default for the server install profile, off on cloud images | Port Cockpit, which is tied to systemd, D-Bus and PAM |
| 1.0 services | Decided: nginx, certd, PostgreSQL, SQLite, Valkey, WireGuard, PHP-FPM and the metrics exporter | Add MariaDB or a mail server to 1.0 |
| Container tool | Decided on 2026-10-22: Podman in Phase 20, after the Go port in Phase 18 | Docker; containers only through the full Linux layer in Phase 29 |
| Familiar Linux commands | Decided on 2026-10-23: a full administration command set with man pages, plus thin systemctl, journalctl, sudo and crontab front ends; Nukesh doubles as a POSIX `/bin/sh` | Native commands only; ship GNU bash as `/bin/sh` |
| Code hosting and CI | Decided: GitHub with GitHub Actions | Codeberg or self-hosted Forgejo |
| License for Dukovo code | Decided: Apache-2.0 OR MIT | MPL-2.0, GPL-2.0 |
| Native filesystem | Decided: Btrfs, our own Rust implementation | Fallback: OpenZFS port as a userspace filesystem server. Rejected: reusing Linux's GPL Btrfs code, a purpose-built filesystem |
| Linking of system programs | Decided: Static | Dynamic with mlibc's loader |
| Default install image | Decided (2026-10-23): the server image, with the console, the web console and the 1.0 services; Lumen joins the default image in 1.1 | A full image with Lumen in 1.0 (the earlier plan); a console-only image without the web console |
| Install target on the Dell | Decided: the 128 GB SSD, erasing Windows; the 512 GB SSD is kept for /shared | Keep Windows and dual boot |
| Closed firmware (microcode, Wi-Fi) | Decided: a separate optional package, kept outside /system | Include it in the base system, or leave it out entirely |
| Unified app runtime | Decided: WebAssembly components (WASI) on Wasmtime, post-1.0 Phase 22 | Port .NET (CoreCLR) as the primary runtime |
| UI toolkit and desktop look | Decided: our own UI kit on Iced; Lumen draws title bars; GTK and Qt only inside third-party bundles; optional personality shell in Phase 26 | Port GTK and Qt into /system; a fully from-scratch toolkit; Slint |
| Default editor and shortcuts | Decided: nano with modern bindings; Windows-style shortcuts system-wide; editor swappable through EDITOR | micro (arrives with the Go port in Phase 18), Microsoft Edit |
| Graphics API | Decided: Vulkan through Mesa is the one native GPU API; wgpu for the UI kit and WebGPU for Wasm; OpenGL through Zink; DirectX through DXVK and VKD3D-Proton | OpenGL as the primary API; our own GPU drivers |
| App store and updates | Decided: Dukovo Store over signed catalogs (Phase 13 backend, Phase 19 app); sideloading allowed with signing warnings; one unified updater for system, firmware and apps (Phase 15) | Separate system and app updaters; catalog-only installs |
| AI assistant | Decided: optional, local-only llama.cpp service with no network capability, user-chosen models, opt-in encrypted memory; system-only access by default with a user toggle for third-party apps (Phase 28) | Cloud models; built into /system; open access for all apps |
| Driver priorities | Decided on 2026-10-23: follow both reference machines | AHCI and NVMe; the Dell's Realtek Fast Ethernet, the Framework's RTL8126 5 GbE and USB Ethernet; AMD platform support and the Framework EC in Phase 11; the Dell's I2C-HID touchpad in Phase 17; MT7925 and QCA9377 Wi-Fi and the Radeon 8060S in Phase 24; the XDNA 2 NPU in Phase 28 |
| Final product name | Decided on 2026-10-22: Dukovo, read as "Duke's place". Self-checks were clean: USPTO had no hits and TMview had no live identical mark (only an expired Greek liquor mark and unrelated substring matches). dukovo.com is a small Indian business-directory site with no trademark, so the main site is dukovo.org. Still to do: an attorney's clearance search, then a US intent-to-use filing in classes 9 and 42 (about $700, plus $300 on use), before Phase 14 | Rejected: Kyrion (Ateme owns an EU/UK class 9 mark), Dukora (too close to Dukoral, a cholera vaccine). OrthrOS was only the working name; the plan, code and docs were renamed to Dukovo on 2026-10-22, and dukovo.org is the project's home |
| Default app set | Partly decided: the list in First-party apps; any further apps after Phases 19 and 26 |  |
| Paid apps in the Store | Open | Free apps only, or paid apps through a payment provider |
| Responsiveness guidelines | Open: review once the UI is well underway, during Phase 19 | Keep them as guidelines, turn some into hard CI gates, or change them; see Guiding principle: a responsive UI |
| UI/UX design language | Open: decided in a full UI/UX design session with the maintainer, and nothing visual is final without the maintainer's approval | A short design review of the web console with the maintainer in Phase 16, then approval before Phase 17's visual work (Lumen title bars, lock screen, colors, icons, fonts), then extended before Phase 19's UI kit and apps and Phase 26's personalities. The whole UI is themeable underneath from the start: one set of design tokens (colors, fonts, spacing, icons, title bars) drives Lumen, the UI kit and every first-party app. Version 1 ships one standard light theme and one standard dark theme, plus the high-contrast accessibility theme; the maintainer decides if and when themes open up to users. Accessibility is part of the design language from the start: WCAG 2.2 AA contrast in both themes, scalable text, a visible focus indicator and reduced motion |
