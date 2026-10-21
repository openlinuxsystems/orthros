<!--
SPDX-FileCopyrightText: 2026 The OrthrOS contributors
SPDX-License-Identifier: Apache-2.0 OR MIT
-->

# Credits

OrthrOS is built on the work of many other projects. This file thanks them,
beyond what their licenses require. The license manifest in every image
(`/system/share/licenses`, [ADR 0021](docs/adr/0021-license-manifest-in-every-image.md))
is the legal record of what an image contains; this is the human one, and it
also names the tools we build with and the projects whose ideas we follow.

When a pull request starts using a new project, or draws on another
project's design, it adds the project here (see
[CONTRIBUTING.md](CONTRIBUTING.md#third-party-code-and-credits)).

## In the repository

| Project | What we use | License |
| --- | --- | --- |
| [Spleen](https://github.com/fcambus/spleen) 2.1.0 by Frederic Cambus | The kernel's console fonts, unmodified, in `kernel/lib/fbcon/fonts/` | BSD-2-Clause |
| [Contributor Covenant](https://www.contributor-covenant.org/) 2.1 | `CODE_OF_CONDUCT.md`, with our contact details | CC-BY-4.0 |
| [Developer Certificate of Origin](https://developercertificate.org/) 1.1 by the Linux Foundation | `DCO`, verbatim | Verbatim copying permitted |

## In every image

| Project | What we use | License |
| --- | --- | --- |
| [Rust](https://www.rust-lang.org/) standard library | `core`, `alloc` and `compiler_builtins`, built from rust-src into the kernel | MIT OR Apache-2.0 |
| [GNU GRUB](https://www.gnu.org/software/grub/) 2 | The boot loader, as a separate program that loads the kernel | GPL-3.0-or-later |
| [Intel microcode](https://github.com/intel/Intel-Linux-Processor-Microcode-Data-Files) | CPU microcode, only in local builds that ask for it ([ADR 0007](docs/adr/0007-closed-firmware-separate-package.md)) | Intel's redistribution license |

## Rust crates

The build tool (`tools/xtask`) and the kernel's host tests use these crates
from [crates.io](https://crates.io/). None of them is linked into the
kernel.

| Crate | Used by | License |
| --- | --- | --- |
| [toml](https://github.com/toml-rs/toml) (with toml_parser, toml_datetime, toml_writer, serde_spanned) | `ob`, to read `orthros.toml` and Cargo files | MIT OR Apache-2.0 |
| [serde](https://github.com/serde-rs/serde) (with serde_core, serde_derive) | `ob` | MIT OR Apache-2.0 |
| [indexmap](https://github.com/indexmap-rs/indexmap), [equivalent](https://github.com/indexmap-rs/equivalent) | `ob`, through toml | Apache-2.0 OR MIT |
| [hashbrown](https://github.com/rust-lang/hashbrown) | `ob`, through indexmap | MIT OR Apache-2.0 |
| [winnow](https://github.com/winnow-rs/winnow) | `ob`, through toml | MIT |
| [syn](https://github.com/dtolnay/syn), [quote](https://github.com/dtolnay/quote), [proc-macro2](https://github.com/dtolnay/proc-macro2), [unicode-ident](https://github.com/dtolnay/unicode-ident) by David Tolnay | `ob`, through serde_derive | MIT OR Apache-2.0 (unicode-ident also Unicode-3.0) |
| [rustc-demangle](https://github.com/rust-lang/rustc-demangle) | `ob`, to name kernel symbols | MIT OR Apache-2.0 |
| [proptest](https://github.com/proptest-rs/proptest) | Property tests of radix-mm, radix-cap and radix-random | MIT OR Apache-2.0 |
| [rand](https://github.com/rust-random/rand), [rand_core](https://github.com/rust-random/rand_core), [rand_xorshift](https://github.com/rust-random/rngs), [getrandom](https://github.com/rust-random/getrandom) | proptest | MIT OR Apache-2.0 |
| [chacha20](https://github.com/RustCrypto/stream-ciphers), [cpufeatures](https://github.com/RustCrypto/utils) by the RustCrypto developers | proptest, through rand | MIT OR Apache-2.0 |
| [regex-syntax](https://github.com/rust-lang/regex), [bitflags](https://github.com/bitflags/bitflags), [num-traits](https://github.com/rust-num/num-traits), [unarray](https://github.com/cameron1024/unarray), [cfg-if](https://github.com/rust-lang/cfg-if), [libc](https://github.com/rust-lang/libc), [autocfg](https://github.com/cuviper/autocfg), [core_detect](https://github.com/thomcc/core_detect) | proptest and its dependencies | MIT OR Apache-2.0 |
| [r-efi](https://github.com/r-efi/r-efi) | getrandom, on UEFI hosts | MIT OR Apache-2.0 OR LGPL-2.1-or-later (we use it under MIT OR Apache-2.0) |

## Build, test and documentation tools

These run on the build machine or in CI and are never part of an image.

| Project | What we use it for | License |
| --- | --- | --- |
| [Rust](https://www.rust-lang.org/) toolchain (rustc, Cargo, rustfmt, Clippy, LLVM tools), nightly-2026-10-15 | Building and checking everything | MIT OR Apache-2.0 (LLVM: Apache-2.0 WITH LLVM-exception) |
| [QEMU](https://www.qemu.org/) | Running OrthrOS and the boot tests | GPL-2.0-only |
| [OVMF](https://github.com/tianocore/tianocore.github.io/wiki/OVMF) from [EDK II](https://github.com/tianocore/edk2) | UEFI firmware for QEMU | BSD-2-Clause-Patent |
| [GNU GRUB](https://www.gnu.org/software/grub/) (grub-mkrescue) | Building boot images | GPL-3.0-or-later |
| [GNU xorriso](https://www.gnu.org/software/xorriso/) | Writing the ISO 9660 image for grub-mkrescue | GPL-3.0-or-later |
| [GNU mtools](https://www.gnu.org/software/mtools/) | Writing the EFI system partition for grub-mkrescue | GPL-3.0-or-later |
| [Kani](https://github.com/model-checking/kani) 0.68.0, with [CBMC](https://github.com/diffblue/cbmc) | Proofs of the memory manager and capability store | Apache-2.0 OR MIT (CBMC: BSD-4-Clause) |
| [TLA+ tools](https://github.com/tlaplus/tlaplus) 1.7.4 (TLC), by Leslie Lamport and contributors | Checking the capability model | MIT |
| [mdBook](https://github.com/rust-lang/mdBook) 0.4.52 | The handbook | MPL-2.0 |
| [scdoc](https://git.sr.ht/~sircmpwn/scdoc) | Man pages | MIT |
| [mandoc](https://mandoc.bsd.lv/) | Checking man pages | ISC |
| [cargo-deny](https://github.com/EmbarkStudios/cargo-deny) | License and advisory checks of crates | MIT OR Apache-2.0 |
| [RustSec advisory database](https://github.com/rustsec/advisory-db) | Advisories for cargo-deny | CC0-1.0 |
| [spdx-tools](https://github.com/spdx/tools-python) 0.8.3 | Validating each image's license manifest | Apache-2.0 |
| [REUSE tool](https://codeberg.org/fsfe/reuse-tool) 6.2.0, by the FSFE | Checking that every file has copyright and license information | GPL-3.0-or-later |
| [Python](https://www.python.org/) `hashlib` and [OpenSSL](https://www.openssl.org/) | Reference values for the BLAKE2s and ChaCha20 tests | PSF-2.0, Apache-2.0 |
| [Eclipse Temurin](https://adoptium.net/) | The Java runtime for TLC in CI | GPL-2.0-only WITH Classpath-exception-2.0 |
| [Ubuntu](https://ubuntu.com/) 24.04 | The build container's base image and CI's runners | Various free licenses |
| [Podman](https://podman.io/) and [Docker Engine](https://github.com/moby/moby) | Running the build container | Apache-2.0 |
| [GitHub Actions](https://github.com/features/actions): [checkout](https://github.com/actions/checkout), [cache](https://github.com/actions/cache), [upload-artifact](https://github.com/actions/upload-artifact), [setup-java](https://github.com/actions/setup-java) | CI | MIT |
| [Swatinem/rust-cache](https://github.com/Swatinem/rust-cache) | Caching builds in CI | LGPL-3.0-only |
| [taiki-e/install-action](https://github.com/taiki-e/install-action) | Installing cargo-deny in CI | Apache-2.0 OR MIT |

## Specifications and papers

We implement these from their published descriptions. No code is copied.

- ChaCha20, from [RFC 8439](https://www.rfc-editor.org/rfc/rfc8439), by
  Daniel J. Bernstein, specified by Yoav Nir and Adam Langley. Its test
  vectors check our implementation.
- BLAKE2s, from [RFC 7693](https://www.rfc-editor.org/rfc/rfc7693), by
  Jean-Philippe Aumasson, Samuel Neves, Zooko Wilcox-O'Hearn and Christian
  Winnerlein, specified by Markku-Juhani O. Saarinen and Jean-Philippe
  Aumasson.
- Daniel J. Bernstein's
  ["Fast-key-erasure random-number generators"](https://blog.cr.yp.to/20170723-random.html),
  the design of the kernel's random number generator.
- Jeff Bonwick and Jonathan Adams,
  ["Magazines and Vmem"](https://www.usenix.org/conference/2001-usenix-annual-technical-conference/magazines-and-vmem-extending-slab-allocator-many),
  USENIX 2001, the design of the heap's per-CPU caches.
- The PC Screen Font format, as described by Andries Brouwer for the
  [kbd project](https://www.win.tue.nl/~aeb/linux/kbd/font-formats-1.html).
- The Multiboot2 specification (GNU), the UEFI and ACPI specifications (UEFI
  Forum), and the Intel and AMD architecture manuals.
- [SPDX](https://spdx.dev/) and the [REUSE specification](https://reuse.software/)
  for license information.

## Projects whose ideas we follow

We use no code from these projects, but our design draws on them, and we
are grateful.

- [seL4](https://sel4.systems/): badges set once on derivation, and memory
  that becomes other objects, as seL4's untyped memory does
  ([ADR 0023](docs/adr/0023-capability-model.md)).
- [Fuchsia](https://fuchsia.dev/) (Zircon handles): the duplicate and
  transfer rights of capabilities.
- [CHERI](https://www.cl.cam.ac.uk/research/security/ctsrd/cheri/): a point
  of comparison for the capability model.
- [Linux](https://kernel.org/) and the BSDs: allocation context flags in the
  style of Linux's GFP flags
  ([ADR 0017](docs/adr/0017-explicit-allocation-context.md)), a random
  number generator with fast key erasure
  ([ADR 0022](docs/adr/0022-kernel-random-number-generator.md)), and Linux's
  KASLR, which ours is compared with
  ([ADR 0018](docs/adr/0018-kaslr-with-a-post-link-relocation-table.md)).
  Linux's btrfs-progs will check the images our own Btrfs code writes.
- [Redox](https://www.redox-os.org/), [SerenityOS](https://serenityos.org/)
  and [Managarm](https://managarm.org/): their histories informed the
  roadmap's sizing.

The roadmap names more projects whose ideas later phases will follow, such as
macOS, Windows, Ubuntu, GNOME, Hyprland and TMOG. They are added here when
that work lands.
