# Building OrthrOS with ob

One command, `./ob`, builds everything: the kernel, the boot image, the
documentation and the tests. It needs no root access. This chapter walks
through a first build; `ob(1)` documents every option.

## What you need

- An x86-64 computer running Linux. On macOS or Windows, or on a Linux
  distribution `ob` does not know, use the build container (see
  [Building in a container](#building-in-a-container)).
- About 3 GB of free disk space for Phase 1.
- An internet connection for the first build, to download the Rust
  toolchain and crates.

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
`./ob doctor`, which checks every other tool. If rustup itself is missing, `ob` says so; install it from
your distribution (`rustup` package) or from <https://rustup.rs>.

When something is missing, `./ob doctor` prints the command that installs
it for your distribution, for example on Ubuntu:

```text
Checking the build host for OrthrOS 0.0.1

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
| Debian, Ubuntu, Linux Mint | `sudo apt install build-essential rustup qemu-system-x86 ovmf grub-common grub-pc-bin grub-efi-amd64-bin xorriso mtools scdoc mandoc`, then `cargo install mdbook --version 0.4.52 --locked` |
| Fedora | `sudo dnf install gcc rustup qemu-system-x86 edk2-ovmf grub2-tools-extra grub2-pc-modules grub2-efi-x64-modules xorriso mtools scdoc mandoc mdbook` |
| Arch Linux | `sudo pacman -S --needed base-devel rustup qemu-system-x86 edk2-ovmf grub libisoburn mtools scdoc mandoc mdbook` |

The C compiler (`build-essential`, `gcc` or `base-devel`) is there because
Rust uses it as the linker for `ob` itself and for mdBook; the kernel links
with `rust-lld`. Debian 12 and Ubuntu 22.04 (Linux Mint 21) have no
`rustup` package: install rustup from <https://rustup.rs> instead, and
drop it from the command.

## Booting OrthrOS in QEMU

```sh
./ob run
```

This builds the kernel and the boot image, then starts QEMU with four CPUs.
The guest's serial port is your terminal, and the QEMU window shows the
screen: you see GRUB's menu, then the kernel's log, which reports what it
found and ends like this:

```text
[    0.000000] Radix 0.0.1 (OrthrOS Phase 2)
[    0.000000] Boot loader: GRUB 2.12
[    0.000000] Firmware: UEFI
[    0.000000] Command line:
[    0.000000] Memory: 1017 MiB usable in 7 regions; memory map:
...
[    0.000000] Random: ChaCha20 seeded before KASLR from RDSEED, RDRAND and timing jitter (4096 samples, 37 stuck)
[    0.000000] KASLR: kernel moved by 0x20200000 (one of 509 places, 2612 addresses patched); direct map one of 65533 places, mapping window one of 32257
[    0.000000] Layout: kernel at 0xffffffffa0300000, direct map at 0xffff962340000000, mapping window at 0xffffd0de40000000
[    0.000000] Paging: W^X holds: none of 1025 MiB of kernel mappings is writable and executable
...
[    0.341869] Time: TSC at 2807.482 MHz (measured against the HPET); APIC timer input 1005.301 MHz; tick 100 Hz
[    0.343077] Memory: 1009 MiB free of 1009 MiB in the frame allocator's zones:
[    0.344198]   node 0 DMA32  0x000000100000-0x00003fedbfff  1009 MiB managed,  1009 MiB free,   4 MiB reserve
[    0.409872] CPUs: 4 of 4 online:
[    0.425233]   cpu0: APIC ID 0, boot CPU, online
[    0.456580]   cpu1: APIC ID 1, online
[    0.485571]   cpu2: APIC ID 2, online
[    0.510992]   cpu3: APIC ID 3, online
[    0.539507] Timer: all 4 CPUs take timer interrupts at 100 Hz (cpu0 9, cpu1 16, cpu2 15, cpu3 14)
[    0.558527] IPI: all 3 other CPUs answered a ping (xAPIC)
[    0.572421] Boot complete.
```

The kernel then idles. `orthros-boot(7)` explains each step. Press
**Ctrl-A** then **X** to quit QEMU. Useful variations:

| Command | Effect |
| --- | --- |
| `./ob run --bios` | Boot as a legacy BIOS machine instead of UEFI |
| `./ob run --media cdrom` | Attach the image as a CD-ROM instead of a USB stick (`disk` for a hard disk) |
| `./ob run --headless` | No QEMU window, serial console only |
| `./ob run --debug` | Use an unoptimised kernel |
| `./ob run --cmdline kdb` | Pass kernel flags, here to start the kernel debugger after boot |
| `./ob run --gdb` | Wait for GDB; see [Debugging the kernel](debugging.md) |
| `./ob run -- -d int` | Pass extra arguments to QEMU, here to log interrupts |

The kernel flags are listed in `orthros-boot(7)`. If the kernel panics,
the report fills the screen; press a key and QEMU boots again, showing the
end of the crashed boot. [Debugging the kernel](debugging.md) explains
what to do with it.

## Running the tests

```sh
./ob test
```

This runs the unit tests on your machine, then the boot tests: QEMU boots
a test image ten times, each with a purpose. Four normal boots, under UEFI
and BIOS and from a USB stick and a CD-ROM, check the memory map, the CPU
list and that all four CPUs take timer interrupts. Others check the boot
log on the screen, the panic report after a deliberate page fault, the
crash log across a restart, every `kdb` command, and microcode loading.
The tests read the screen by comparing QEMU screenshots with the kernel's
own font, so what you would see on a laptop is tested too.

Serial logs and screenshots are kept in `out/test/`. `./ob test unit` and
`./ob test boot` run one half, and `./ob test boot --case kdb` runs one
boot test; `./ob help test` lists them. Without KVM the boot tests take a
few minutes.

Before sending a change, also run `./ob lint` and `./ob docs --check`; CI
runs all three.

## Building the boot image

```sh
./ob image
```

The result is `out/orthros-0.0.1-x86_64.img`, a hybrid image that boots on
UEFI and BIOS machines from a USB stick, a hard disk or a CD.
`./ob image --iso` writes the same image as an `.iso` for virtual machines.
[Creating USB install media](usb-media.md) shows how to write it to a stick.

## Configuration

`orthros.toml` at the top of the repository holds the choices a builder
makes: the build profile, the GRUB menu timeout, kernel flags, microcode
to add to the image, and the virtual machine `./ob run` starts. Each key is described in
`orthros.toml(5)`. Command-line options override the file for one command.

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
