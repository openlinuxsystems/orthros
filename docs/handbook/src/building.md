# Building OrthrOS with ob

One command, `./ob`, builds everything: the kernel, the boot image, the
documentation and the tests. It needs no root access. This chapter walks
through a first build; `ob(1)` documents every option.

## What you need

- An x86-64 computer running Linux. On macOS or Windows, or on a Linux
  distribution `ob` does not know, use the build container (see
  [Building in a container](#building-in-a-container)).
- About 2 GB of free disk space for Phase 0.
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
`rust-toolchain.toml` through rustup, then runs `./ob doctor`, which checks
every other tool. If rustup itself is missing, `ob` says so; install it from
your distribution (`rustup` package) or from <https://rustup.rs>.

When something is missing, `./ob doctor` prints the command that installs
it for your distribution, for example on Ubuntu:

```text
Checking the build host for OrthrOS 0.0.1

  ok       rustup          rustup 1.29.1
  ok       Rust toolchain  nightly-2026-10-15
  ok       kernel target   x86_64-unknown-none
  MISSING  QEMU            runs and tests the image
  MISSING  xorriso         writes the ISO 9660 part of the image
  ...

On Debian or Ubuntu, install the missing packages with:
  sudo apt install qemu-system-x86 xorriso
```

Run the command, then `./ob doctor` again until everything is `ok`. A
`note` about KVM is not an error: without KVM, QEMU still works, only more
slowly. To use KVM, add yourself to the `kvm` group
(`sudo usermod -aG kvm $USER`) and log in again.

The packages `ob` needs on each distribution are:

| Distribution | Install command |
| --- | --- |
| Debian, Ubuntu | `sudo apt install rustup qemu-system-x86 ovmf grub-common grub-pc-bin grub-efi-amd64-bin xorriso mtools scdoc mandoc`, then `cargo install mdbook --version 0.4.52 --locked` |
| Fedora | `sudo dnf install rustup qemu-system-x86 edk2-ovmf grub2-tools-extra grub2-pc-modules grub2-efi-x64-modules xorriso mtools scdoc mandoc mdbook` |
| Arch Linux | `sudo pacman -S --needed rustup qemu-system-x86 edk2-ovmf grub libisoburn mtools scdoc mandoc mdbook` |

## Booting OrthrOS in QEMU

```sh
./ob run
```

This builds the kernel and the boot image, then starts QEMU. The guest's
serial port is your terminal: you see GRUB's menu, then the kernel's
report.

```text
Radix 0.0.1 (OrthrOS Phase 0 stub kernel)
Boot loader: GRUB 2.12
Firmware: UEFI
Command line:
Memory: 1017 MiB usable in 7 regions
Boot complete.
```

Press **Ctrl-A** then **X** to quit QEMU. Useful variations:

| Command | Effect |
| --- | --- |
| `./ob run --bios` | Boot as a legacy BIOS machine instead of UEFI |
| `./ob run --media cdrom` | Attach the image as a CD-ROM instead of a USB stick (`disk` for a hard disk) |
| `./ob run --headless` | No QEMU window, serial console only |
| `./ob run --debug` | Use an unoptimised kernel |
| `./ob run -- -s -S` | Pass extra arguments to QEMU, here to wait for a debugger |

Under UEFI, GRUB prints "no suitable video mode found" before starting the
kernel. That is expected in Phase 0; see `orthros-boot(7)`.

## Running the tests

```sh
./ob test
```

This runs the unit tests on your machine, then boots a test image four
times in QEMU: under UEFI and BIOS, from a USB stick and from a CD-ROM. Each
boot passes when the kernel reports the right firmware and finishes. Serial
logs are kept in `out/test/`. `./ob test unit` and `./ob test boot` run one
half.

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
makes: the build profile, the GRUB menu timeout, extra kernel flags, and the
virtual machine `./ob run` starts. Each key is described in
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
