<!--
SPDX-FileCopyrightText: 2026 The Dukovo contributors
SPDX-License-Identifier: Apache-2.0 OR MIT
-->

# Credits

Dukovo is built on the work of many other projects. This file thanks them,
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
| [Rust](https://www.rust-lang.org/) standard library | `core`, `alloc` and `compiler_builtins`, built from rust-src into the kernel and the user programs | MIT OR Apache-2.0 |
| [GNU GRUB](https://www.gnu.org/software/grub/) 2 | The boot loader, as a separate program that loads the kernel | GPL-3.0-or-later |
| [Intel microcode](https://github.com/intel/Intel-Linux-Processor-Microcode-Data-Files) | CPU microcode, only in local builds that ask for it ([ADR 0007](docs/adr/0007-closed-firmware-separate-package.md)) | Intel's redistribution license |
| [mlibc](https://github.com/managarm/mlibc) 7.0.0 by the Managarm project | The C library and dynamic linker (`/system/lib/libc.so`, `ld.so` and the static `libc.a`), with Radix's system calls added in `ports/mlibc/sysdeps/radix` and the fixes in `ports/mlibc/patches` ([ADR 0068](docs/adr/0068-mlibc-is-the-c-library.md)); its own tests run in the `posix` boot test | MIT |
| [frigg](https://github.com/managarm/frigg) b0dbea66bc19 and [libsmarter](https://github.com/managarm/libsmarter) f7d061bc37d4 by the Managarm project | Template libraries mlibc is built on; frigg's printf and formatting headers as its commit b9ce1199489f has them (commits by lzcunt and no92) | MIT |
| [musl](https://musl.libc.org/) 1.2.5 by Rich Felker and contributors | Exact floating-point formatting and scanning, `strftime` and the calendar conversions of `mktime`, `gmtime` and `localtime`, `scanf`'s conversions and integers, `inet_pton` and `inet_ntop`, and the `rand48` family, in `ports/mlibc/musl` and the patches ([ADR 0072](docs/adr/0072-exact-numbers-and-times-in-the-c-library.md)) | MIT |
| [GNU C Library](https://www.gnu.org/software/libc/) 2.42 locale data | The `C.UTF-8` locale, compiled from glibc's tables of Unicode 16 ([ADR 0073](docs/adr/0073-c-utf-8-locale.md)) | Unicode-3.0 |
| [compiler-rt](https://compiler-rt.llvm.org/) builtins from LLVM 20 | Helper functions every C program links | Apache-2.0 WITH LLVM-exception |
| [Rust](https://www.rust-lang.org/) `std`, `panic_unwind` and `unwind`, with the crates they use: [libc](https://github.com/rust-lang/libc) 0.2.191 from crates.io (patched for Radix, [ADR 0080](docs/adr/0080-crates-patched-for-radix.md)), [unwinding](https://github.com/nbdd0121/unwinding) 0.2.10 by Gary Guo, [addr2line](https://github.com/gimli-rs/addr2line), [gimli](https://github.com/gimli-rs/gimli), [object](https://github.com/gimli-rs/object), [miniz_oxide](https://github.com/Frommi/miniz_oxide), [adler2](https://github.com/oyvindln/adler2), [hashbrown](https://github.com/rust-lang/hashbrown), [memchr](https://github.com/BurntSushi/memchr) by Andrew Gallant, [rustc-demangle](https://github.com/rust-lang/rustc-demangle), [cfg-if](https://github.com/rust-lang/cfg-if) and `std_detect` | Rust's standard library for programs on Radix ([ADR 0070](docs/adr/0070-rust-standard-library-port.md)) | MIT OR Apache-2.0 (memchr: Unlicense OR MIT; miniz_oxide also Zlib; adler2 also 0BSD) |
| [uutils coreutils](https://github.com/uutils/coreutils) 0.9.0 by the uutils developers | The core utilities: one program, `/system/bin/coreutils`, with a link by each utility's name, and their manual pages, which its own uudoc writes; the changes for Radix are in `ports/uutils-coreutils/patches` | MIT |
| [uutils findutils](https://github.com/uutils/findutils) 0.9.1 by the uutils developers | `find` and `xargs` | MIT |
| [errno](https://github.com/lambda-fairy/rust-errno), [getrandom](https://github.com/rust-random/getrandom), [mio](https://github.com/tokio-rs/mio), [nix](https://github.com/nix-rust/nix), [rustix](https://github.com/bytecodealliance/rustix) and [uucore](https://github.com/uutils/coreutils) from crates.io | Patched for Radix in `ports/rust-crates` ([ADR 0080](docs/adr/0080-crates-patched-for-radix.md)), for the programs above | MIT OR Apache-2.0 (nix, mio, uucore: MIT; rustix also Apache-2.0 WITH LLVM-exception) |
| The other crates the uutils programs link, from crates.io, among them [clap](https://github.com/clap-rs/clap), [regex](https://github.com/rust-lang/regex), [jiff](https://github.com/BurntSushi/jiff), [memchr](https://github.com/BurntSushi/memchr) and [bstr](https://github.com/BurntSushi/bstr) by Andrew Gallant, [chrono](https://github.com/chronotope/chrono), [ICU4X](https://github.com/unicode-org/icu4x) by the Unicode Consortium, [Fluent](https://github.com/projectfluent/fluent-rs) by Mozilla, the [RustCrypto](https://github.com/RustCrypto) hashes, [BLAKE3](https://github.com/BLAKE3-team/BLAKE3), [num-bigint](https://github.com/rust-num/num-bigint), [rayon](https://github.com/rayon-rs/rayon), [crossterm](https://github.com/crossterm-rs/crossterm), [indicatif](https://github.com/console-rs/indicatif), [notify](https://github.com/notify-rs/notify) and [Oniguruma](https://github.com/kkos/oniguruma) by K. Kosako, built by onig_sys for `expr` | What the utilities are made of; every one, with its version and license, is in the image's license manifest | MIT, Apache-2.0, Unlicense, Unicode-3.0, BSD-2-Clause, BSD-3-Clause, Zlib, CC0-1.0 (each as the manifest gives it) |
| [reedline](https://github.com/nushell/reedline) 0.52.1 by the Nushell project, with [crossterm](https://github.com/crossterm-rs/crossterm) 0.29, [signal-hook](https://github.com/vorner/signal-hook) and signal-hook-mio by Michal Vaner, [nu-ansi-term](https://github.com/nushell/nu-ansi-term), [vte](https://github.com/alacritty/vte) by the Alacritty project, [strip-ansi-escapes](https://github.com/luser/strip-ansi-escapes), [unicode-width](https://github.com/unicode-rs/unicode-width), [unicode-segmentation](https://github.com/unicode-rs/unicode-segmentation), [chrono](https://github.com/chronotope/chrono), [parking_lot](https://github.com/Amanieu/parking_lot) by Amanieu d'Antras, [strum](https://github.com/Peternator7/strum), [derive_more](https://github.com/JelteF/derive_more), [indexmap](https://github.com/indexmap-rs/indexmap) and the crates they use, from crates.io | Nukesh's line editor and the terminal handling under it ([ADR 0083](docs/adr/0083-nukesh-the-shell.md)), and the ordered tables of its variables and functions; every one, with its version and license, is in the image's license manifest | MIT, Apache-2.0 OR MIT (each as the manifest gives it; unicode-ident also Unicode-3.0, memchr Unlicense OR MIT) |
| [Lua](https://www.lua.org/) 5.4.7 and its test suite, by Roberto Ierusalimschy, Luiz Henrique de Figueiredo and Waldemar Celes at PUC-Rio | A real C program to test the C library with, in the `posix` boot test | MIT |
| [libc-test](https://wiki.musl-libc.org/libc-test.html) b7ec467969a5 by Rich Felker, Szabolcs Nagy, Kirill Ternovsky, John Spencer, Jens Gustedt, Alexander Monakov and Julien Ramseier, from Khem Raj's mirror (github.com/kraj/libc-test) | The functional and regression tests of C libraries, which run in the `posix` boot test and found the fixes in `ports/mlibc/patches` 0013 to 0022; its math tests, whose vectors from crlibm are GPL-2.0, are left out | MIT |

## Rust crates

The build tool (`tools/xtask`), the Linux benchmark (`tools/linux-bench`)
and the kernel's host tests use these crates from
[crates.io](https://crates.io/). None of them is linked into the kernel or
the user programs, except as Rust's standard library uses them (above).

| Crate | Used by | License |
| --- | --- | --- |
| [toml](https://github.com/toml-rs/toml) (with toml_parser, toml_datetime, toml_writer, serde_spanned) | `ob`, to read `dukovo.toml` and Cargo files | MIT OR Apache-2.0 |
| [serde](https://github.com/serde-rs/serde) (with serde_core, serde_derive) | `ob` | MIT OR Apache-2.0 |
| [indexmap](https://github.com/indexmap-rs/indexmap), [equivalent](https://github.com/indexmap-rs/equivalent) | `ob`, through toml | Apache-2.0 OR MIT |
| [hashbrown](https://github.com/rust-lang/hashbrown) | `ob`, through indexmap | MIT OR Apache-2.0 |
| [winnow](https://github.com/winnow-rs/winnow) | `ob`, through toml | MIT |
| [syn](https://github.com/dtolnay/syn), [quote](https://github.com/dtolnay/quote), [proc-macro2](https://github.com/dtolnay/proc-macro2), [unicode-ident](https://github.com/dtolnay/unicode-ident) by David Tolnay | `ob`, through serde_derive | MIT OR Apache-2.0 (unicode-ident also Unicode-3.0) |
| [rustc-demangle](https://github.com/rust-lang/rustc-demangle) | `ob`, to name kernel symbols | MIT OR Apache-2.0 |
| [proptest](https://github.com/proptest-rs/proptest) | Property tests of radix-mm, radix-cap, radix-random and radix-sched | MIT OR Apache-2.0 |
| [rand](https://github.com/rust-random/rand), [rand_core](https://github.com/rust-random/rand_core), [rand_xorshift](https://github.com/rust-random/rngs), [getrandom](https://github.com/rust-random/getrandom) | proptest | MIT OR Apache-2.0 |
| [chacha20](https://github.com/RustCrypto/stream-ciphers), [cpufeatures](https://github.com/RustCrypto/utils) by the RustCrypto developers | proptest, through rand | MIT OR Apache-2.0 |
| [libc](https://github.com/rust-lang/libc) 0.2 | `linux-bench`, for Linux's system calls (`getrandom`, `eventfd`, futexes, CPU affinity); proptest's dependencies | MIT OR Apache-2.0 |
| [regex-syntax](https://github.com/rust-lang/regex), [bitflags](https://github.com/bitflags/bitflags), [num-traits](https://github.com/rust-num/num-traits), [unarray](https://github.com/cameron1024/unarray), [cfg-if](https://github.com/rust-lang/cfg-if), [autocfg](https://github.com/cuviper/autocfg), [core_detect](https://github.com/thomcc/core_detect) | proptest and its dependencies | MIT OR Apache-2.0 |
| [r-efi](https://github.com/r-efi/r-efi) | getrandom, on UEFI hosts | MIT OR Apache-2.0 OR LGPL-2.1-or-later (we use it under MIT OR Apache-2.0) |

## Build, test and documentation tools

These run on the build machine or in CI and are never part of an image.

| Project | What we use it for | License |
| --- | --- | --- |
| [Rust](https://www.rust-lang.org/) toolchain (rustc, Cargo, rustfmt, Clippy, LLVM tools), nightly-2026-10-15 | Building and checking everything | MIT OR Apache-2.0 (LLVM: Apache-2.0 WITH LLVM-exception) |
| [QEMU](https://www.qemu.org/) | Running Dukovo and the boot tests, and counting instructions for `ob bench --instructions` (`-icount`) | GPL-2.0-only |
| [OVMF](https://github.com/tianocore/tianocore.github.io/wiki/OVMF) from [EDK II](https://github.com/tianocore/edk2) | UEFI firmware for QEMU | BSD-2-Clause-Patent |
| [GNU GRUB](https://www.gnu.org/software/grub/) (grub-mkrescue) | Building boot images | GPL-3.0-or-later |
| [GNU xorriso](https://www.gnu.org/software/xorriso/) | Writing the ISO 9660 image for grub-mkrescue | GPL-3.0-or-later |
| [GNU mtools](https://www.gnu.org/software/mtools/) | Writing the EFI system partition for grub-mkrescue, and reading and writing the FAT disks of the `storage` boot test on the host | GPL-3.0-or-later |
| [dosfstools](https://github.com/dosfstools/dosfstools) (`mkfs.fat`, `fsck.fat`) | Making the FAT volumes Radix's FAT tests and the `storage` boot test use, and checking what Radix wrote to them | GPL-3.0-or-later |
| [Clang, LLD and LLVM](https://llvm.org/) 20 | Building the C library, its ports and C programs for Radix ([ADR 0069](docs/adr/0069-clang-toolchain-and-ports.md)) | Apache-2.0 WITH LLVM-exception |
| [Meson](https://mesonbuild.com/) and [Ninja](https://ninja-build.org/) | Building mlibc | Apache-2.0 |
| [freestnd-cxx-hdrs](https://github.com/osdev0/freestnd-cxx-hdrs) 14.3.0 (libstdc++'s freestanding headers from GCC 14), by the osdev0 contributors and the GCC project | The C++ headers mlibc is compiled with; nothing of them reaches an image | GPL-3.0-or-later WITH GCC-exception-3.1 |
| The [GNU C Library](https://www.gnu.org/software/libc/)'s `localedef` on the build machine, and glibc 2.39's `printf` | Compiling the `C.UTF-8` locale, and the expected results of the C library's `printf` tests | LGPL-2.1-or-later |
| [Kani](https://github.com/model-checking/kani) 0.68.0, with [CBMC](https://github.com/diffblue/cbmc) | Proofs of the memory manager, capability store, scheduling policy and IPC arithmetic | Apache-2.0 OR MIT (CBMC: BSD-4-Clause) |
| [TLA+ tools](https://github.com/tlaplus/tlaplus) 1.7.4 (TLC), by Leslie Lamport and contributors | Checking the capability, scheduler, idle wake-up, endpoint, channel and FAT cluster models | MIT |
| [mdBook](https://github.com/rust-lang/mdBook) 0.4.52 | The handbook | MPL-2.0 |
| [scdoc](https://git.sr.ht/~sircmpwn/scdoc) | Man pages | MIT |
| [mandoc](https://mandoc.bsd.lv/) | Checking man pages | ISC |
| [Linux](https://kernel.org/) on the build machine and CI's runners | The baseline `ob bench` compares Radix with: the runner's own kernel, booted unchanged in a virtual machine, and the host itself | GPL-2.0-only |
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
  the design of the kernel's random number generator and its per-CPU
  generators, which work a batch of blocks at a time as the post
  describes and as OpenBSD's `arc4random` and Linux's `vgetrandom` do
  ([ADR 0042](docs/adr/0042-batched-random-and-lighter-system-calls.md)).
- Ion Stoica and Hussein Abdel-Wahab,
  "Earliest Eligible Virtual Deadline First: A Flexible and Accurate
  Mechanism for Proportional Share Resource Allocation" (Old Dominion
  University, TR-95-22, 1995), the fair class's scheduling algorithm.
- Jeff Bonwick and Jonathan Adams,
  ["Magazines and Vmem"](https://www.usenix.org/conference/2001-usenix-annual-technical-conference/magazines-and-vmem-extending-slab-allocator-many),
  USENIX 2001, the design of the heap's per-CPU caches.
- Jochen Liedtke, "Improving IPC by Kernel Design" (SOSP 1993) and "On
  µ-Kernel Construction" (SOSP 1995): synchronous IPC with messages in
  registers, the basis of the endpoints' fast path.
- Larry McVoy and Carl Staelin, "lmbench: Portable Tools for Performance
  Analysis" (USENIX 1996): the method of `ob bench`, after `lat_syscall`,
  `lat_ctx`, `lat_pipe` and `lat_unix`. We use no lmbench code.
- Jens Axboe, ["Efficient IO with io_uring"](https://kernel.dk/io_uring.pdf)
  (2019): the design of rings.
- George Marsaglia, "Xorshift RNGs" (Journal of Statistical Software,
  2003), and Sebastiano Vigna's xorshift64\* and
  [SplitMix64](https://prng.di.unimi.it/): the small generators of the
  kernel's stress tests and the system-call fuzzer.
- The system-call fuzzers of Dave Jones's
  [Trinity](https://github.com/kernelslacker/trinity) and Google's
  [syzkaller](https://github.com/google/syzkaller): the idea of the `fuzz`
  boot test, arguments drawn from values at the edges of what the kernel
  accepts. We use no code from either.
- The PC Screen Font format, as described by Andries Brouwer for the
  [kbd project](https://www.win.tue.nl/~aeb/linux/kbd/font-formats-1.html).
- The Multiboot2 specification (GNU), the UEFI and ACPI specifications (UEFI
  Forum), and the Intel and AMD architecture manuals.
- [SPDX](https://spdx.dev/) and the [REUSE specification](https://reuse.software/)
  for license information.
- The ustar interchange format of POSIX.1-1988 (IEEE Std 1003.1, the
  `pax` utility's description in later editions), the format of the boot
  archive ([ADR 0049](docs/adr/0049-boot-archive.md)).
- Microsoft's "FAT: General Overview of On-Disk Format", version 1.03
  (2000), and its long file names, from which `radix-fat` implements
  FAT12, FAT16 and FAT32
  ([ADR 0056](docs/adr/0056-own-fat-implementation.md)).
- Howard Hinnant's
  ["chrono-Compatible Low-Level Date Algorithms"](https://howardhinnant.github.io/date_algorithms.html),
  which FAT's dates are converted with.
- The OASIS Virtual I/O Device (virtio) specification, version 1.2: its
  modern PCI transport, split virtqueues and block device
  ([ADR 0053](docs/adr/0053-modern-virtio-only.md)).
- The PCI Local Bus, PCI Express Base and PCI Firmware specifications
  (PCI-SIG): configuration space through ECAM and the ACPI MCFG table,
  base address registers, and MSI and MSI-X
  ([ADR 0052](docs/adr/0052-message-signalled-interrupts-only.md)).
- The MBR partition table, and the GUID partition table of the UEFI
  specification with its CRC32 checks.
- The System V ABI's ELF format and the x86-64 psABI, by Michael Matz,
  Jan Hubička, Andreas Jaeger and Mark Mitchell and maintained by H.J. Lu
  and others, which `radix-elf` reads, with its `R_X86_64_RELATIVE`
  relocations.

## Projects whose ideas we follow

We use no code from these projects, but our design draws on them, and we
are grateful.

- [seL4](https://sel4.systems/): badges set once on derivation, and memory
  that becomes other objects, as seL4's untyped memory does
  ([ADR 0023](docs/adr/0023-capability-model.md)); endpoints with call and
  reply-and-receive, notifications as words of bits, and capabilities
  passed in messages into a receive window
  ([ADR 0032](docs/adr/0032-endpoints-notifications-and-channels.md));
  its root task, which the kernel starts with every resource, and its
  untyped memory, whose space comes back only when everything made from
  it has gone, which is why Spark gives each program a memory object of
  its own ([ADR 0050](docs/adr/0050-spark-is-started-by-the-kernel-from-the-boot-archive.md)).
- The L4 family of microkernels, after Jochen Liedtke: messages in
  registers and IPC as the kernel's central, fastest path
  ([ADR 0032](docs/adr/0032-endpoints-notifications-and-channels.md)).
- [Fuchsia](https://fuchsia.dev/) (Zircon handles): the duplicate and
  transfer rights of capabilities. Its FIDL, with protocol buffers and Cap'n
  Proto, was a point of comparison for ridl
  ([ADR 0033](docs/adr/0033-ridl-interface-definitions.md)). Zircon's
  processes, built from outside by whoever holds their handle and started
  with `zx_process_start`, its bootfs, and `userboot`, the one program its
  kernel starts, shaped our processes, boot archive and Spark's start
  ([ADR 0047](docs/adr/0047-processes.md),
  [ADR 0050](docs/adr/0050-spark-is-started-by-the-kernel-from-the-boot-archive.md)).
- [CHERI](https://www.cl.cam.ac.uk/research/security/ctsrd/cheri/): a point
  of comparison for the capability model.
- [Capsicum](https://www.cl.cam.ac.uk/research/security/capsicum/), by
  Robert N. M. Watson, Jonathan Anderson, Ben Laurie and Kris Kennaway
  (USENIX Security 2010), with its `openat`-relative lookups that never
  climb above their directory, and [WASI](https://wasi.dev/)'s preopened
  directories: how files are reached only through directory capabilities
  ([ADR 0054](docs/adr/0054-files-through-directory-capabilities.md)).
- [Plan 9 from Bell Labs](https://9p.io/plan9/): kernel state as plain
  text read with ordinary file calls, as `/proc` does
  ([ADR 0057](docs/adr/0057-hierarchy-proc-and-sys.md)).
- [Linux](https://kernel.org/) and the BSDs: allocation context flags in the
  style of Linux's GFP flags
  ([ADR 0017](docs/adr/0017-explicit-allocation-context.md)), a random
  number generator with fast key erasure
  ([ADR 0022](docs/adr/0022-kernel-random-number-generator.md)), and Linux's
  KASLR, which ours is compared with
  ([ADR 0018](docs/adr/0018-kaslr-with-a-post-link-relocation-table.md)).
  Linux's EEVDF fair class (since 6.6) and its scheduling domains, its
  lockdep lock-order checker, its quiescent-state-based RCU and cgroups v2
  shaped our scheduler, lock checker, RCU and resource domains
  ([ADR 0027](docs/adr/0027-scheduler-design.md),
  [ADR 0028](docs/adr/0028-lock-checker-in-debug-builds.md),
  [ADR 0029](docs/adr/0029-resource-domains-and-out-of-memory-policy.md)).
  Linux's io_uring, with its `IORING_OP_MSG_RING` ring-to-ring messages,
  shaped radix-ring ([ADR 0034](docs/adr/0034-radix-ring.md)), and its
  `getrandom(2)` the random capability's `read`
  ([ADR 0031](docs/adr/0031-system-call-interface.md)).
  Linux's cpuidle-haltpoll governor (`Documentation/virt/guest-halt-polling.rst`)
  is the policy idle virtual CPUs poll by, with its defaults
  ([ADR 0037](docs/adr/0037-lazy-timer-and-guest-halt-polling.md)); its
  futexes, which only enter the kernel when a waiter has said it is there,
  shaped how channels wake each other
  ([ADR 0038](docs/adr/0038-channel-wake-ups-per-direction.md)); its
  per-CPU random generators keyed from a base generator shaped ours; and
  its workaround for AMD's SYSRET erratum (`X86_BUG_SYSRET_SS_ATTRS`)
  showed us why to reload SS at a context switch. KVM's paravirtual end
  of interrupt, as Linux's documentation describes it
  (`Documentation/virt/kvm/x86/msr.rst`) and Linux guests use it, is how
  Radix acknowledges interrupts under KVM
  ([ADR 0040](docs/adr/0040-hot-paths-without-hidden-costs.md)); no code
  is taken. Linux's EEVDF yield, which since Linux 6.18 gives up the rest
  of the slice ("sched/fair: Forfeit vruntime on yield", by Fernand Sieber
  after a suggestion by Peter Zijlstra), is how a Radix yield works, and
  Linux's handlers of interprocessor interrupts, which end the interrupt
  before their work, set the order of ours
  ([ADR 0041](docs/adr/0041-yield-forfeits-its-slice-and-ipis-end-first.md));
  no code is taken. Linux's file-descriptor lookups, which take no shared
  lock, were the bar for finding IPC objects without one
  ([ADR 0044](docs/adr/0044-objects-found-without-a-shared-lock.md)), and
  the memory clobbers on its `sti`, `cli` and `hlt` wrappers showed which
  of our interrupt instructions must be compiler barriers.
  Linux's `init=` boot parameter and the words after `--` that it passes
  to init, its initramfs, and its address space layout randomization with
  `vm.mmap_rnd_bits` were the models for Spark's start, the boot archive
  and user ASLR
  ([ADR 0048](docs/adr/0048-static-pie-programs-and-user-aslr.md),
  [ADR 0050](docs/adr/0050-spark-is-started-by-the-kernel-from-the-boot-archive.md));
  `-static-pie` as GCC, glibc and musl build it, and LLVM's lld, which
  links our programs, set what a static position-independent program
  is. No code is taken.
  Linux's page cache, its XArray (by Matthew Wilcox), which the page
  index follows with nodes of 64 slots, its flusher threads, its tmpfs,
  with half of memory as its default size, its names for disks and
  partitions, the format of its `/proc/mounts` and the layout of its
  `/sys/block` and `/sys/bus/pci/devices` shaped Radix's storage
  ([ADR 0055](docs/adr/0055-page-cache-and-writeback.md),
  [ADR 0057](docs/adr/0057-hierarchy-proc-and-sys.md)); its
  `-Wframe-larger-than` limit on stack frames is the idea of the kernel's
  frame budget ([ADR 0058](docs/adr/0058-kernel-stack-frame-budget.md));
  and its `DCACHE_MOUNTED` flag, which spares a path walk the mount table
  at a directory nothing is mounted on, is the idea of each mount's
  filter of mount points, a Bloom filter (Burton H. Bloom, 1970) with
  one hash, by Fibonacci hashing as Knuth describes it
  ([ADR 0059](docs/adr/0059-file-operations-fast-paths.md)).
  No code is taken.
  Linux's btrfs-progs will check the images our own Btrfs code writes.
  Radix's POSIX layer follows Linux's interface wherever programs see
  it, so that the C library's and Rust's Linux code serves: its signal
  numbers and x86-64 signal frame, its `struct termios`, terminal
  `ioctl` numbers and the N_TTY line discipline's behaviour, its clocks,
  resource limits with their defaults, nice levels and status words,
  and its open file description locks, with its limit on how far
  deadlock detection follows waiting owners
  ([ADR 0062](docs/adr/0062-signals-in-the-kernel.md) to
  [ADR 0075](docs/adr/0075-advisory-file-locks.md)). No code is taken:
  the kernel's code is written from Linux's documentation and man pages
  and checked against Linux by the C tests.
- Rust's `std` for the GNU Hurd, whose `stat` fields ours takes, and for
  Xous, which unwinds with the `unwinding` crate as ours does
  ([ADR 0070](docs/adr/0070-rust-standard-library-port.md)).
- The Rust compiler's performance suite
  ([rustc-perf](https://github.com/rust-lang/rustc-perf)) and
  [SQLite](https://sqlite.org/cpu.html), which track performance by
  counting instructions because the counts hold still where timings do
  not: the idea of CI's instruction budgets
  ([ADR 0045](docs/adr/0045-instruction-budgets-in-ci.md)).
- Python's [`timeit`](https://docs.python.org/3/library/timeit.html),
  whose documentation advises taking the fastest of several runs, since
  the slower ones are slowed by other processes rather than by the code
  measured: the idea of the performance gate's fastest of three runs
  ([ADR 0046](docs/adr/0046-performance-gate-on-the-fastest-of-three-runs.md)).
- [Redox](https://www.redox-os.org/), [SerenityOS](https://serenityos.org/)
  and [Managarm](https://managarm.org/): their histories informed the
  roadmap's sizing. Managarm's mlibc is our C library, and its system
  layers for other kernels were the model for Radix's.
- [GNU Bash](https://www.gnu.org/software/bash/) by Chet Ramey and the
  Free Software Foundation, and [dash](http://gondor.apana.org.au/~herbert/dash/)
  by Herbert Xu: what Nukesh does is what they do. Its test scripts'
  expected output was recorded from bash 5.2, and from dash for POSIX
  mode, and its functions in the environment take bash's form
  ([ADR 0083](docs/adr/0083-nukesh-the-shell.md)). Nukesh has none of
  their code.
- FreeBSD's fdescfs: opening `/dev/fd/N` copies the descriptor, as
  `dup` does ([ADR 0084](docs/adr/0084-dev-fd-in-the-c-library.md)).

The roadmap names more projects whose ideas later phases will follow, such as
macOS, Windows, Ubuntu, GNOME, Hyprland and TMOG. They are added here when
that work lands.
