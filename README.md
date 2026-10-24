<!--
SPDX-FileCopyrightText: 2026 The Dukovo contributors
SPDX-License-Identifier: Apache-2.0 OR MIT
-->

# Dukovo

Dukovo is an operating system built around **Radix**, a capability-based
kernel written in Rust. Release 1.0 is a headless **server** system, in
the spirit of Ubuntu Server: the kernel, the Nukesh shell with a full set
of administration commands, Btrfs storage with mirroring, networking,
users and logging, bundles and service bundles, atomic updates, an
installer with the `rdisk` partitioner, USB and cloud images, and the
**Dukovo Admin** web console. The **Lumen** compositor follows in 1.1
([ADR 0078](docs/adr/0078-server-first-release.md)). Every command and file
format comes with a man page.

Dukovo has finished **Phase 8** of its [roadmap](docs/roadmap.md):

- **Boot.** The Radix kernel boots through GRUB under UEFI and BIOS at a
  random address.
- **Memory.** It manages memory with per-NUMA-node frame allocators and a
  slab heap with per-CPU caches.
- **Protection.** It protects itself with W^X, guard pages, SMEP and SMAP.
- **Hardware.** It starts every CPU and reads the ACPI tables.
- **Capabilities.** Every resource is reached through unforgeable
  capabilities, checked by Kani proofs, TLA+ models and in-kernel tests.
- **Scheduling.** Threads are scheduled fairly or by real-time priority,
  in resource domains.
- **IPC.** Programs talk over endpoints, notifications, shared-memory
  channels and io_uring-style rings.
- **Processes.** Programs run in processes of their own, which Spark
  starts from a boot archive.
- **Files.** Files live behind directory capabilities: tmpfs, FAT on
  virtio disks, `/proc` and `/sys`.
- **POSIX.** A POSIX layer covers fork, exec, signals, pipes, terminals
  and job control. It runs on mlibc and a port of Rust's standard
  library, so C and Rust programs from elsewhere build for it.
- **Speed.** CI checks every kernel operation it measures against Linux
  in the same virtual machine, and fails if Radix is slower.

Phase 9 brings the shell and the command-line tools. There is nothing to
install yet.

## Quick start

You need an x86-64 Linux machine. Everything else is checked for you.

```sh
git clone https://github.com/openlinuxsystems/orthros
cd orthros
./ob setup     # installs the pinned Rust toolchain, then lists any missing tools
./ob run       # builds the kernel and boot image, then boots it in QEMU
```

`./ob doctor` prints the exact command that installs anything missing on
Debian, Ubuntu, Fedora or Arch Linux. On macOS, Windows or another
distribution, put `--container` in front of any command to run it inside
the pinned build container with Podman or Docker:

```sh
./ob --container test
```

## The `ob` build tool

| Command | What it does |
| --- | --- |
| `./ob setup` | Install the pinned Rust toolchain, then run `doctor` |
| `./ob doctor` | Check host tools and print the commands that install what is missing |
| `./ob build` | Build the kernel, with its symbol table, into `out/sysroot/` |
| `./ob ports` | Build the software Dukovo ports from elsewhere (mlibc, Rust's standard library, Lua and the rest) at their pinned commits |
| `./ob image` | Build the hybrid USB/disk image `out/dukovo-<version>-x86_64.img` (`--iso` for an ISO, `--cmdline`, `--microcode`) |
| `./ob run` | Boot the image in QEMU with the serial console in your terminal (`--bios`, `--media usb\|cdrom\|disk`, `--gdb`, `--cmdline`) |
| `./ob test` | Run unit tests, then the boot tests in QEMU, from the boot paths and the memory manager to processes, storage and POSIX (`kani` for the Kani proofs, `tla` for the TLA+ models, `corpus` for the shell corpus instead) |
| `./ob bench` | Measure kernel operations against Linux in the same virtual machine (`--gate` fails if Radix is slower; `--instructions` counts instructions per operation) |
| `./ob docs` | Build the man pages and the handbook into `out/docs/` (`--check` for completeness checks) |
| `./ob lint` / `./ob fmt` | Check or apply formatting, and run Clippy with warnings denied |
| `./ob signoff` | Check that your new commits carry a Developer Certificate of Origin sign-off |
| `./ob clean` | Remove build output |

Settings live in [`dukovo.toml`](dukovo.toml). The full manual is
[`ob(1)`](docs/man/ob.1.scd); after `./ob docs`, read it with
`man -l out/docs/man/man1/ob.1`.

## Writing the image to a USB stick

```sh
./ob image
lsblk -d -o NAME,SIZE,MODEL,TRAN          # find your stick, for example sdb
sudo dd if=out/dukovo-0.0.1-x86_64.img of=/dev/sdX bs=4M conv=fsync status=progress
```

Everything on the target device is erased. balenaEtcher, GNOME Disks
("Restore Disk Image") and Rufus in DD mode write the same image. The
handbook chapter [Creating USB install media](docs/handbook/src/usb-media.md)
covers booting it on real hardware.

## Repository layout

```
ob                  build entry point (shell wrapper around tools/xtask)
dukovo.toml        build configuration, see dukovo.toml(5)
rust-toolchain.toml pinned nightly Rust
abi/                the system call interface, in ridl (see radix-ipc(7))
kernel/             the Radix kernel workspace
  radix/            the architecture-independent kernel: consoles, panic, kdb,
                    threads, user mode, system calls and IPC objects
  hal/              hardware abstraction layer traits, kernel command line
  arch/x86_64/      boot trampoline, CPU tables, paging, ACPI, APIC, SMP,
                    SYSCALL entry and user copies
  arch/aarch64/     HAL stubs that CI compiles, so x86 cannot leak into radix/
  mm/, cap/, sched/ the memory manager, capability core and scheduling policy
  ipc/              IPC arithmetic: info words, user ranges, ring accounting
  abi/              radix-abi, the Rust bindings generated from abi/
  lib/              host-tested kernel libraries: acpi, bootinfo, elf, fbcon,
                    kdb, klog, ksyms, lockdep, microcode, random, relocs, sync
  targets/          the kernel's Rust target specification
userspace/          the user programs: libradix and the usertests image
image/grub/         GRUB menu template for every image
docs/
  man/              man pages (scdoc sources)
  handbook/         user handbook (mdBook)
  adr/              architecture decision records
  design/           design notes on how the kernel works
  roadmap.md        the implementation plan, phases 0 to 26
tools/xtask/        the Rust program behind ./ob
tools/ridlc/        the interface compiler: abi/ to Rust, C and man(2)
tools/linux-bench/  the Linux baseline for ./ob bench
tools/container/    the pinned build container
```

Later phases add `libs/`, `ports/` and `tests/`, as described in the
roadmap.

## Contributing

Read [CONTRIBUTING.md](CONTRIBUTING.md) and the
[coding standard](docs/coding-standard.md). Every commit is signed off under
the [Developer Certificate of Origin](CONTRIBUTING.md#developer-certificate-of-origin),
and everyone taking part follows the [code of conduct](CODE_OF_CONDUCT.md).
[GOVERNANCE.md](GOVERNANCE.md) says who decides what, and
[SECURITY.md](SECURITY.md) how to report a vulnerability privately.
Decisions that shape the system are recorded as
[architecture decision records](docs/adr/README.md).

Every decision follows the project's
[guiding principle](GOVERNANCE.md#guiding-principle-favor-the-user): at a
fork in the road, Dukovo favors the user. Freedom is at the core of its
philosophy, and the user owns the machine.

## License

Dukovo is licensed under either of

- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE))
- MIT license ([LICENSE-MIT](LICENSE-MIT))

at your option. Unless you explicitly state otherwise, any contribution
intentionally submitted for inclusion in Dukovo by you, as defined in the
Apache-2.0 license, shall be dual licensed as above, without any additional
terms or conditions.

Some files come from other projects under their own licenses, such as the
console fonts (BSD-2-Clause). Every file names its copyright holders and
license, following the [REUSE specification](https://reuse.software/), and
`LICENSES/` holds the license texts. [CREDITS.md](CREDITS.md) thanks the
projects Dukovo is built on.
