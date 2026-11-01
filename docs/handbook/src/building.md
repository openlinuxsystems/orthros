<!--
SPDX-FileCopyrightText: 2026 The Dukovo contributors
SPDX-License-Identifier: Apache-2.0 OR MIT
-->

# Building Dukovo with ob

One command, `./ob`, builds everything: the kernel, the boot image, the
documentation and the tests. It needs no root access. This chapter walks
through a first build; `ob(1)` documents every option.

## What you need

- An x86-64 computer running Linux. On macOS or Windows, or on a Linux
  distribution `ob` does not know, use the build container (see
  [Building in a container](#building-in-a-container)).
- About 10 GB of free disk space, most of it build output in `target/`.
- An internet connection for the first build, to download the Rust
  toolchain, crates and the sources of the ported software (the C
  library, Lua and the rest, ADR 0069).

## First build

Get the source and let `ob` set up the Rust toolchain:

```sh
git clone https://github.com/openlinuxsystems/orthros
cd orthros
./ob setup
```

`./ob setup` installs the nightly Rust toolchain pinned in
`rust-toolchain.toml` through rustup, with the Rust sources the kernel
build needs and the AArch64 target the lint step checks against, then runs
`./ob doctor`, which checks every other tool, and `./ob crates`, which
fetches the crates from crates.io that the kernel builds with patches of
its own (smoltcp, for now) and patches them into `kernel/patched/`. If rustup itself is
missing, `ob` says so; install it from your distribution (`rustup`
package) or from <https://rustup.rs>.

When something is missing, `./ob doctor` prints the command that installs
it for your distribution, for example on Ubuntu:

```text
Checking the build host for Dukovo 0.11.0

  ok       rustup          rustup 1.28.2
  ok       Rust toolchain  nightly-2026-10-15
  ok       Rust sources    rust-src
  ok       AArch64 target  aarch64-unknown-none
  MISSING  QEMU            runs and tests the image
  MISSING  xorriso         writes the ISO 9660 part of the image
  ...

On Debian or Ubuntu, install the missing packages with:
  sudo apt install qemu-system-x86 xorriso
```

Run the command, then `./ob doctor` again until everything is `ok`. A
`note` is not an error. Without KVM, QEMU still works, only more slowly;
to use KVM, add yourself to the `kvm` group (`sudo usermod -aG kvm $USER`)
and log in again. GDB is only needed for `./ob run --gdb`.

The packages `ob` needs on each distribution are:

| Distribution | Install command |
| --- | --- |
| Debian, Ubuntu, Linux Mint | `sudo apt install build-essential rustup qemu-system-x86 ovmf grub-common grub-pc-bin grub-efi-amd64-bin xorriso mtools dosfstools exfatprogs sleuthkit btrfs-progs e2fsprogs ntfs-3g dislocker libbde-utils udftools udfclient genisoimage scdoc mandoc clang-20 lld-20 llvm-20 libclang-rt-20-dev meson ninja-build cmake git`, then `cargo install mdbook --version 0.4.52 --locked` |
| Fedora | `sudo dnf install gcc rustup qemu-system-x86 edk2-ovmf grub2-tools-extra grub2-pc-modules grub2-efi-x64-modules xorriso mtools dosfstools exfatprogs sleuthkit btrfs-progs e2fsprogs ntfsprogs dislocker udftools genisoimage scdoc mandoc mdbook clang compiler-rt lld llvm meson ninja-build cmake git patch` |
| Arch Linux | `sudo pacman -S --needed base-devel rustup qemu-system-x86 edk2-ovmf grub libisoburn mtools dosfstools exfatprogs sleuthkit btrfs-progs e2fsprogs ntfs-3g udftools scdoc mandoc mdbook clang compiler-rt lld llvm meson ninja cmake git` |

dislocker and libbde, which only the BitLocker tests use, are in the AUR
on Arch Linux, and libbde is not packaged for Fedora: build it from
[its source](https://github.com/libyal/libbde) there.

The C compiler (`build-essential`, `gcc` or `base-devel`) is there because
Rust uses it as the linker for `ob` itself and for mdBook; the kernel links
with `rust-lld`. Clang 20 or later, LLD, LLVM's tools, compiler-rt, Meson
and Ninja build the C library and the other ported software for Dukovo,
through wrappers `ob` writes (ADR 0069); Ubuntu 24.04 and Linux Mint 22
have Clang 20 in their updates. The C.UTF-8 locale is compiled with the
host's `localedef`, which every glibc system has.

btrfs-progs makes the filesystems Radix's Btrfs tests read. Ubuntu 24.04
and Linux Mint 22 have version 6.6, which lacks options the tests use, so
`./ob test` fetches Ubuntu 26.04's package (6.17.1), checks its SHA-256 and
unpacks it into `.cache/btrfs-progs/` without installing it. Fedora and
Arch Linux have a recent enough version.

udftools, UDFclient and genisoimage make and fill the UDF volumes Radix's
UDF tests read. Fedora and Arch Linux have no UDFclient package: build it
from <https://www.13thmonkey.org/udfclient/>, or test in the container.

Debian 12 and Ubuntu 22.04 (Linux Mint 21) have no `rustup` package and
no Clang 20: install rustup from <https://rustup.rs> and Clang 20 from
LLVM's packages at <https://apt.llvm.org>, or build in the container.

## Booting Dukovo in QEMU

```sh
./ob run
```

This builds the kernel and the boot image, then starts QEMU with four CPUs.
The guest's serial port is your terminal, and the QEMU window shows the
screen: you see GRUB's menu, then the kernel's log, which reports what it
found and ends like this:

```text
[    0.000000] Radix 0.11.0 (Dukovo Server 0.11.0)
[    0.000000] Boot loader: GRUB 2.12
[    0.000000] Firmware: UEFI
[    0.000000] Command line:
[    0.000000] Memory: 1017 MiB usable in 7 regions; memory map:
...
[    0.000000] Random: ChaCha20 seeded before KASLR from RDSEED, RDRAND and timing jitter (4096 samples, 14 stuck)
[    0.000000] KASLR: kernel moved by 0x33200000 (one of 505 places, 12304 addresses patched); direct map one of 65533 places, mapping window one of 32257
[    0.000000] Layout: kernel at 0xffffffffb3300000, direct map at 0xffffba4080000000, mapping window at 0xffffc8e500000000
[    0.000000] Paging: W^X holds: none of 1025 MiB of kernel mappings is writable and executable
...
[    0.254500] Time: TSC at 2807.603 MHz (measured against the HPET); APIC timer input 1004.640 MHz; tick 100 Hz
[    0.284715] Time: the real-time clock says 2026-10-22 04:25:30 (taken as UTC)
[    0.319111] Memory: 1006 MiB free of 1006 MiB in the frame allocator's zones:
[    0.351867]   node 0 DMA32  0x000000100000-0x00003fedbfff  1006 MiB managed,  1006 MiB free,   4 MiB reserve
[    0.383220] Scheduler: EEVDF fair class (3 ms slices) and FIFO and round-robin real-time classes; timer APIC one-shot, idle with HLT; extended state XSAVEOPT on every switch: x87, SSE, AVX, PKRU; 2696-byte areas
[    0.419834] Lock checker: off (the lockdep flag turns it on)
[    0.445393] Capabilities: ready; 21 object kinds, 13 rights, spaces of up to 65536 slots, derivations up to 64 deep
[    0.469304] User mode: SYSCALL/SYSRET entry, user half 0x10000-0x7ffffffff000; SMEP on, SMAP on (lifted for user copies only)
[    0.494239] Files: the root is a tmpfs; Spark mounts the rest
...
[    0.613107] CPUs: 4 of 4 online:
[    0.634020]   cpu0: APIC ID 0, boot CPU, online
[    0.655178]   cpu1: APIC ID 1, online
[    0.676009]   cpu2: APIC ID 2, online
[    0.696883]   cpu3: APIC ID 3, online
[    0.720307] Scheduler: 4 CPUs run threads, on 1 NUMA node
[    0.745062] PCI: 0000:00:00.0 8086:29c0 class 060000: host bridge
...
[    0.964452] PCI: 0000:00:1f.3 8086:2930 class 0c0500: SMBus controller
[    0.985756] PCI: 6 functions on 1 segment; 0 virtio disks
[    1.006700] Timer: all 4 CPUs take timer interrupts at 100 Hz (cpu0 27, cpu1 44, cpu2 42, cpu3 41)
[    1.028489] IPI: all 3 other CPUs answered a ping (xAPIC)
[    1.049978] Boot complete.
[    0.866770] Init: the boot archive is 91956 KiB with 1073 entries; starting /system/commands/spark
[    1.583622] Init: /system/commands/spark is process 1
...
[    1.945004] cpu1: spark[1]: spark: built the hierarchy of 9 directories and mounted 9 filesystems
[    1.994464] cpu1: spark[1]: spark: added 7 default settings to /system/settings
[    2.025364] cpu1: spark[1]: spark: listening at /system/live/spark/control
[    2.059082] cpu1: spark[1]: spark: 4 services in /system/settings/spark/services, 4 enabled
[    2.124318] cpu1: spark[1]: spark: started logd (/system/commands/logd) as process 12
[    2.162156] cpu1: spark[1]: spark: started consoled (/system/commands/consoled) as process 13
[    2.166020] cpu1: spark[1]: spark: started volumed (/system/commands/volumed) as process 14
[    2.171904] cpu1: spark[1]: spark: started diskhealth (/system/commands/diskhealth) as process 15
[    2.207236] Framebuffer: a program took the screen; the log stays off it
```

After `Boot complete.` the kernel starts Spark, the first program, which
builds the directory hierarchy, mounts any FAT disks QEMU has at
`/volumes` (see [Files, the directory hierarchy and /volumes](files.md))
and starts the services. The first is `logd`, which keeps the system's
log (see [Logs, monitoring and troubleshooting](logs.md)); then
`consoled` takes the screen from the kernel's log and shows a shell on
it: the
[Nukesh guide](nukesh.md) goes on from there. `dukovo-boot(7)` and
`spark(8)` explain each step. Press **Ctrl-A** then **X** to quit QEMU,
or type `poweroff` in the shell. Useful variations:

| Command | Effect |
| --- | --- |
| `./ob run --bios` | Boot as a legacy BIOS machine instead of UEFI |
| `./ob run --media cdrom` | Attach the image as a CD-ROM instead of a USB stick (`disk` for a hard disk) |
| `./ob run --headless` | No QEMU window, serial console only |
| `./ob run --debug` | Use an unoptimised kernel |
| `./ob run --cmdline kdb` | Pass kernel flags, here to start the kernel debugger after boot |
| `./ob run --gdb` | Wait for GDB; see [Debugging the kernel](debugging.md) |
| `./ob run -- -d int` | Pass extra arguments to QEMU, here to log interrupts |

The kernel flags are listed in `dukovo-boot(7)`. If the kernel panics,
the report fills the screen; press a key and QEMU boots again, showing the
end of the crashed boot. [Debugging the kernel](debugging.md) explains
what to do with it.

## Running the tests

```sh
./ob test
```

This runs the unit tests on your machine, then the boot tests: QEMU boots
a test image thirty-three times, each with a purpose. Four normal boots, under
UEFI and BIOS and from a USB stick and a CD-ROM, check the memory map, the
CPU list and that all four CPUs take timer interrupts. Others check the
boot log on the screen, the panic report after a deliberate page fault,
the crash log across a restart, every `kdb` command, microcode loading,
the memory manager, the scheduler, capabilities, IPC, Spark and its
services, the consoles and the shell on them, disks and files, and the
POSIX layer: the `posix` boot test runs C programs on the
C library, mlibc's and Lua's own test suites, libc-test, a Rust
program on the standard library and the ported utilities, `ls`, `sort`,
`find`, `xargs` and the rest (radix-posix(7)). The tests read the screen by
comparing QEMU screenshots with the kernel's own font, so what you would
see on a laptop is tested too.

Serial logs and screenshots are kept in `out/test/`. `./ob test unit` and
`./ob test boot` run one half, and `./ob test boot --case kdb` runs one
boot test; `./ob help test` lists them. Without KVM the boot tests take a
few minutes.

Before sending a change, also run `./ob lint` and `./ob docs --check`; CI
runs all three. `./ob lint` also checks that the kernel builds for RISC-V,
against placeholders of its hardware layer, and `./ob test riscv` boots a
small RISC-V program on QEMU's `virt` machine (it needs QEMU's RISC-V
emulator, the `qemu-system-misc` package), so that nothing x86-only slips
into the kernel's common code; CI runs that too.

## Building the boot image

```sh
./ob image
```

The result is `out/dukovo-0.11.0-x86_64.img`, a hybrid image that boots on
UEFI and BIOS machines from a USB stick, a hard disk or a CD.
`./ob image --iso` writes the same image as an `.iso` for virtual machines.
[Creating USB install media](usb-media.md) shows how to write it to a stick.

## Configuration

`dukovo.toml` at the top of the repository holds the choices a builder
makes: the build profile, the GRUB menu timeout, kernel flags, microcode
to add to the image, and the virtual machine `./ob run` starts. Each key
is described in `dukovo.toml(5)`. Command-line options override the file
for one command.

## Building in a container

Put `--container` before any command to run it inside the pinned build
container, which has every tool at the version CI uses:

```sh
./ob --container test
./ob --container image
```

`ob` uses Podman if it is installed, otherwise Docker. The first run builds
the container image from `tools/container/Containerfile`, which takes a few
minutes. Output still lands in `out/` in your checkout. On macOS and Windows
this works through Podman Desktop or Docker Desktop.

## Documentation

```sh
./ob docs
man -l out/docs/man/man1/ob.1
```

`./ob docs` compiles the man pages into `out/docs/man/` and this handbook
into `out/docs/handbook/` (open `index.html` in a browser).

## When something goes wrong

- Every error starts with `ob: error:`, and most are followed by an
  `ob: hint:` line saying what to do.
- `./ob --verbose <command>` prints each external command before it runs, so
  you can run it by hand.
- `./ob clean` removes build output; `./ob clean --all` also removes the
  build tool and caches, for a completely fresh build.
- If the build tool itself fails to compile after updating the source, run
  `./ob setup` in case the pinned toolchain changed.
