# OrthrOS

OrthrOS is an operating system built around **Radix**, a capability-based
kernel written in Rust. The goal is a complete, installable system: the
kernel, a shell and core utilities, Btrfs storage, networking, users and
logging, atomic updates, the **Lumen** Wayland compositor, an installer with
the `rdisk` partitioner, and a USB install image. Every command and file
format comes with a man page.

OrthrOS is at **Phase 4** of its [roadmap](docs/roadmap.md): the Radix
kernel boots through GRUB under UEFI and BIOS at a random address, chosen
by a ChaCha20 generator seeded from RDSEED, RDRAND and timing jitter, manages
memory with a frame allocator per NUMA node and a slab heap with per-CPU
caches (with Kani proofs of their invariants), protects itself with W^X,
guard pages, SMEP and SMAP, starts every CPU, reads the ACPI tables, takes
timer interrupts and reports all of it on the serial port and the screen.
Its capability core holds the capabilities through which every resource
will be reached, with delegation, badges, sealing and revocation; Kani
proofs, a TLA+ model and an in-kernel test suite check that no capability
can be forged or have its rights widened. It runs threads on every CPU with
an EEVDF fair class and FIFO and round-robin real-time classes, priority
inheritance, and load balancing that keeps threads near their caches and
NUMA nodes; groups them into resource domains with CPU weights and memory
limits; and, when memory runs out, stops whole app domains before services
and never the system's own. A lock checker reports locking bugs the first
time they run, and RCU lets readers go without locks.
A panic fills the screen with a symbolized backtrace and is kept in memory
for the next boot; a serial kernel debugger, GDB through QEMU and trace
points help with the rest. There is nothing to install yet.

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
| `./ob image` | Build the hybrid USB/disk image `out/orthros-<version>-x86_64.img` (`--iso` for an ISO, `--cmdline`, `--microcode`) |
| `./ob run` | Boot the image in QEMU with the serial console in your terminal (`--bios`, `--media usb\|cdrom\|disk`, `--gdb`, `--cmdline`) |
| `./ob test` | Run unit tests, then the boot tests in QEMU: UEFI and BIOS, USB and CD-ROM, 4 CPUs, panics, the crash log, `kdb`, the screen, the memory manager, capabilities and the scheduler (`kani` for the Kani proofs, `tla` for the TLA+ models instead) |
| `./ob docs` | Build the man pages and the handbook into `out/docs/` (`--check` for completeness checks) |
| `./ob lint` / `./ob fmt` | Check or apply formatting, and run Clippy with warnings denied |
| `./ob signoff` | Check that your new commits carry a Developer Certificate of Origin sign-off |
| `./ob clean` | Remove build output |

Settings live in [`orthros.toml`](orthros.toml). The full manual is
[`ob(1)`](docs/man/ob.1.scd); after `./ob docs`, read it with
`man -l out/docs/man/man1/ob.1`.

## Writing the image to a USB stick

```sh
./ob image
lsblk -d -o NAME,SIZE,MODEL,TRAN          # find your stick, for example sdb
sudo dd if=out/orthros-0.0.1-x86_64.img of=/dev/sdX bs=4M conv=fsync status=progress
```

Everything on the target device is erased. balenaEtcher, GNOME Disks
("Restore Disk Image") and Rufus in DD mode write the same image. The
handbook chapter [Creating USB install media](docs/handbook/src/usb-media.md)
covers booting it on real hardware.

## Repository layout

```
ob                  build entry point (shell wrapper around tools/xtask)
orthros.toml        build configuration, see orthros.toml(5)
rust-toolchain.toml pinned nightly Rust
kernel/             the Radix kernel workspace
  radix/            the architecture-independent kernel: consoles, panic, kdb
  hal/              hardware abstraction layer traits, kernel command line
  arch/x86_64/      boot trampoline, CPU tables, paging, ACPI, APIC, SMP
  arch/aarch64/     HAL stubs that CI compiles, so x86 cannot leak into radix/
  lib/              host-tested kernel libraries: acpi, bootinfo, fbcon,
                    kdb, klog, ksyms, microcode, sync
  targets/          the kernel's Rust target specification
image/grub/         GRUB menu template for every image
docs/
  man/              man pages (scdoc sources)
  handbook/         user handbook (mdBook)
  adr/              architecture decision records
  design/           design notes on how the kernel works
  roadmap.md        the implementation plan, phases 0 to 26
tools/xtask/        the Rust program behind ./ob
tools/container/    the pinned build container
```

Later phases add `abi/`, `userspace/`, `libs/`, `ports/` and `tests/`, as
described in the roadmap.

## Contributing

Read [CONTRIBUTING.md](CONTRIBUTING.md) and the
[coding standard](docs/coding-standard.md). Every commit is signed off under
the [Developer Certificate of Origin](CONTRIBUTING.md#developer-certificate-of-origin),
and everyone taking part follows the [code of conduct](CODE_OF_CONDUCT.md).
[GOVERNANCE.md](GOVERNANCE.md) says who decides what, and
[SECURITY.md](SECURITY.md) how to report a vulnerability privately.
Decisions that shape the system are recorded as
[architecture decision records](docs/adr/README.md).

## License

OrthrOS is licensed under either of

- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE))
- MIT license ([LICENSE-MIT](LICENSE-MIT))

at your option. Unless you explicitly state otherwise, any contribution
intentionally submitted for inclusion in OrthrOS by you, as defined in the
Apache-2.0 license, shall be dual licensed as above, without any additional
terms or conditions.
