<!--
SPDX-FileCopyrightText: 2026 The Dukovo contributors
SPDX-License-Identifier: Apache-2.0 OR MIT
-->

# Credits

Dukovo is built on the work of many other projects. This file thanks them,
beyond what their licenses require. The license manifest in every image
(`/system/resources/licenses`, [ADR 0021](docs/adr/0021-license-manifest-in-every-image.md))
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
| [uACPI](https://github.com/uACPI/uACPI) 7.0.0 by Daniil Tatianin and its contributors | The interpreter of the firmware's AML, compiled into the kernel from `kernel/lib/uacpi/vendor/`, unmodified ([ADR 0200](docs/adr/0200-uacpi-runs-the-firmwares-aml-in-the-kernel.md)) | MIT |
| [GNU GRUB](https://www.gnu.org/software/grub/) 2 | The boot loader, as a separate program that loads the kernel | GPL-3.0-or-later |
| [Intel microcode](https://github.com/intel/Intel-Linux-Processor-Microcode-Data-Files) | CPU microcode, only in local builds that ask for it ([ADR 0007](docs/adr/0007-closed-firmware-separate-package.md)) | Intel's redistribution license |
| [mlibc](https://github.com/managarm/mlibc) 7.0.0 by the Managarm project | The C library and dynamic linker (`/system/libraries/libc.so`, `ld.so` and the static `libc.a`), with Radix's system calls added in `ports/mlibc/sysdeps/radix` and the fixes in `ports/mlibc/patches` ([ADR 0068](docs/adr/0068-mlibc-is-the-c-library.md)); its own tests run in the `posix` boot test | MIT |
| [frigg](https://github.com/managarm/frigg) b0dbea66bc19 and [libsmarter](https://github.com/managarm/libsmarter) f7d061bc37d4 by the Managarm project | Template libraries mlibc is built on; frigg's printf and formatting headers as its commit b9ce1199489f has them (commits by lzcunt and no92) | MIT |
| [musl](https://musl.libc.org/) 1.2.5 by Rich Felker and contributors | Exact floating-point formatting and scanning, `strftime` and the calendar conversions of `mktime`, `gmtime` and `localtime`, `scanf`'s conversions and integers, `inet_pton` and `inet_ntop`, the `rand48` family, the message catalogs of `catopen` and the limits `pathconf` gives, in `ports/mlibc/musl` and the patches ([ADR 0072](docs/adr/0072-exact-numbers-and-times-in-the-c-library.md)); its cancellation points, system calls made from a stub whose labels the cancel signal's handler looks for, are the design of ours, with no code taken ([ADR 0104](docs/adr/0104-cancellation-points-as-musl-makes-them.md)) | MIT |
| [GNU C Library](https://www.gnu.org/software/libc/) 2.42 locale data | The `C.UTF-8` locale, compiled from glibc's tables of Unicode 16 ([ADR 0073](docs/adr/0073-c-utf-8-locale.md)) | Unicode-3.0 |
| The [PCI ID Database](https://pci-ids.ucw.cz/) 2026.10.28 by Martin Mares, Albert Pool and the PCI ID Project's contributors | The names of PCI vendors, devices and classes that `lspci` shows, in `/system/resources/hwdata/pci.ids` ([ADR 0144](docs/adr/0144-lspci-lscpu-and-lsmem-with-the-pci-id-database.md)) | BSD-3-Clause (offered as GPL-2.0-or-later OR BSD-3-Clause) |
| The [USB ID Repository](http://www.linux-usb.org/usb-ids.html)'s database 2026.06.26, kept by Stephen J. Gowdy, with its contributors | The names of USB vendors, products and classes that `lsusb` shows, in `/system/resources/hwdata/usb.ids`, with the GPL text its repository carries as its notice ([ADR 0230](docs/adr/0230-sensors-and-lsusb-with-the-usb-id-database.md)) | GPL-2.0-or-later OR BSD-3-Clause |
| [xkeyboard-config](https://gitlab.freedesktop.org/xkeyboard-config/xkeyboard-config) 2.48 by its contributors, compiled with [libxkbcommon](https://xkbcommon.org/) | The consoles' 40 keyboard layouts, made from its layouts by `userspace/programs/keymap/generate.py` and built into consoled and keymap(1), with its COPYING as their notice ([ADR 0149](docs/adr/0149-the-consoles-type-in-layouts-made-from-xkeyboard-configs.md)); the dead keys' letters are Unicode's compositions | MIT and notices like it |
| [compiler-rt](https://compiler-rt.llvm.org/) builtins from LLVM 20 | Helper functions every C program links | Apache-2.0 WITH LLVM-exception |
| [Rust](https://www.rust-lang.org/) `std`, `panic_unwind` and `unwind`, with the crates they use: [libc](https://github.com/rust-lang/libc) 0.2.191 from crates.io (patched for Radix, [ADR 0080](docs/adr/0080-crates-patched-for-radix.md)), [unwinding](https://github.com/nbdd0121/unwinding) 0.2.10 by Gary Guo, [addr2line](https://github.com/gimli-rs/addr2line), [gimli](https://github.com/gimli-rs/gimli), [object](https://github.com/gimli-rs/object), [miniz_oxide](https://github.com/Frommi/miniz_oxide), [adler2](https://github.com/oyvindln/adler2), [hashbrown](https://github.com/rust-lang/hashbrown), [memchr](https://github.com/BurntSushi/memchr) by Andrew Gallant, [rustc-demangle](https://github.com/rust-lang/rustc-demangle), [cfg-if](https://github.com/rust-lang/cfg-if) and `std_detect` | Rust's standard library for programs on Radix ([ADR 0070](docs/adr/0070-rust-standard-library-port.md)) | MIT OR Apache-2.0 (memchr: Unlicense OR MIT; miniz_oxide also Zlib; adler2 also 0BSD) |
| [uutils coreutils](https://github.com/uutils/coreutils) 0.9.0 by the uutils developers | The core utilities: one program, `/system/commands/coreutils`, with a link by each utility's name, and their manual pages, which its own uudoc writes; the changes for Radix are in `ports/uutils-coreutils/patches` | MIT |
| [uutils findutils](https://github.com/uutils/findutils) 0.9.1 by the uutils developers | `find` and `xargs` | MIT |
| [errno](https://github.com/lambda-fairy/rust-errno), [filetime](https://github.com/alexcrichton/filetime) by Alex Crichton, [getrandom](https://github.com/rust-random/getrandom), [mio](https://github.com/tokio-rs/mio), [nix](https://github.com/nix-rust/nix), [rustix](https://github.com/bytecodealliance/rustix) and [uucore](https://github.com/uutils/coreutils) from crates.io | Patched for Radix in `ports/rust-crates` ([ADR 0080](docs/adr/0080-crates-patched-for-radix.md)), for the programs above | MIT OR Apache-2.0 (nix, mio, uucore: MIT; rustix also Apache-2.0 WITH LLVM-exception) |
| The other crates the uutils programs link, from crates.io, among them [clap](https://github.com/clap-rs/clap), [regex](https://github.com/rust-lang/regex), [jiff](https://github.com/BurntSushi/jiff), [memchr](https://github.com/BurntSushi/memchr) and [bstr](https://github.com/BurntSushi/bstr) by Andrew Gallant, [chrono](https://github.com/chronotope/chrono), [ICU4X](https://github.com/unicode-org/icu4x) by the Unicode Consortium, [Fluent](https://github.com/projectfluent/fluent-rs) by Mozilla, the [RustCrypto](https://github.com/RustCrypto) hashes, [BLAKE3](https://github.com/BLAKE3-team/BLAKE3), [num-bigint](https://github.com/rust-num/num-bigint), [rayon](https://github.com/rayon-rs/rayon), [crossterm](https://github.com/crossterm-rs/crossterm), [indicatif](https://github.com/console-rs/indicatif), [notify](https://github.com/notify-rs/notify) and [Oniguruma](https://github.com/kkos/oniguruma) by K. Kosako, built by onig_sys for `expr` | What the utilities are made of; every one, with its version and license, is in the image's license manifest | MIT, Apache-2.0, Unlicense, Unicode-3.0, BSD-2-Clause, BSD-3-Clause, Zlib, CC0-1.0 (each as the manifest gives it) |
| [reedline](https://github.com/nushell/reedline) 0.52.1 by the Nushell project, with [crossterm](https://github.com/crossterm-rs/crossterm) 0.29, [signal-hook](https://github.com/vorner/signal-hook) and signal-hook-mio by Michal Vaner, [nu-ansi-term](https://github.com/nushell/nu-ansi-term), [vte](https://github.com/alacritty/vte) by the Alacritty project, [strip-ansi-escapes](https://github.com/luser/strip-ansi-escapes), [unicode-width](https://github.com/unicode-rs/unicode-width), [unicode-segmentation](https://github.com/unicode-rs/unicode-segmentation), [chrono](https://github.com/chronotope/chrono), [parking_lot](https://github.com/Amanieu/parking_lot) by Amanieu d'Antras, [strum](https://github.com/Peternator7/strum), [derive_more](https://github.com/JelteF/derive_more), [indexmap](https://github.com/indexmap-rs/indexmap) and the crates they use, from crates.io | Nukesh's line editor and the terminal handling under it ([ADR 0083](docs/adr/0083-nukesh-the-shell.md)), and the ordered tables of its variables and functions; every one, with its version and license, is in the image's license manifest | MIT, Apache-2.0 OR MIT (each as the manifest gives it; unicode-ident also Unicode-3.0, memchr Unlicense OR MIT) |
| [vte](https://github.com/alacritty/vte) 0.14.1 by Joe Wilm and Christian Duerr of the Alacritty project, after Paul Williams' state machine of DEC's terminals, with [memchr](https://github.com/BurntSushi/memchr) by Andrew Gallant, from crates.io | The escape-sequence parser of consoled's virtual consoles ([ADR 0100](docs/adr/0100-consoled-virtual-consoles-in-a-program.md)) | Apache-2.0 OR MIT (memchr: Unlicense OR MIT) |
| [serde](https://github.com/serde-rs/serde) 1.0.229 and [serde_json](https://github.com/serde-rs/json) 1.0.154 by Erick Tryzelaar and David Tolnay, with [itoa](https://github.com/dtolnay/itoa) and [zmij](https://github.com/dtolnay/zmij) by David Tolnay and [memchr](https://github.com/BurntSushi/memchr) by Andrew Gallant, from crates.io | The system log's records and requests as lines of JSON, in logd, log and journalctl ([ADR 0210](docs/adr/0210-logd-keeps-the-kernel-log-as-the-system-log.md)) | MIT OR Apache-2.0 (zmij: MIT; memchr: Unlicense OR MIT) |
| [toml](https://github.com/toml-rs/toml) 0.9.12 by the toml-rs contributors, with toml_parser 1.1.5, toml_datetime 0.7.5 and serde_spanned 1.1.2, and [winnow](https://github.com/winnow-rs/winnow) 0.7.15 and 1.0.4 by Ed Page, from crates.io | Service files (service.toml(5)), in Spark and the `service` command ([ADR 0216](docs/adr/0216-service-files-in-toml.md)) | MIT OR Apache-2.0 (winnow: MIT) |
| [argon2](https://github.com/RustCrypto/password-hashes) 0.5.3, [blake2](https://github.com/RustCrypto/hashes) 0.10.6, [password-hash](https://github.com/RustCrypto/traits) 0.5.0, [digest](https://github.com/RustCrypto/traits) 0.10.7, [crypto-common](https://github.com/RustCrypto/traits) 0.1.7, [block-buffer](https://github.com/RustCrypto/utils) 0.10.4, [cpufeatures](https://github.com/RustCrypto/utils) 0.2.17 and [base64ct](https://github.com/RustCrypto/formats) 1.8.3 by the RustCrypto developers, with [subtle](https://github.com/dalek-cryptography/subtle) 2.6.1 by Isis Lovecruft and Henry de Valence, [generic-array](https://github.com/fizyk20/generic-array) 0.14.7 by Bartłomiej Kamiński and Aaron Trent, [typenum](https://github.com/paholg/typenum) 1.20.1 by Paho Lurie-Gregg and its contributors, and [rand_core](https://github.com/rust-random/rand) 0.6.4 by the Rand project, from crates.io | Password hashes, Argon2id (RFC 9106), in authd(8) ([ADR 0218](docs/adr/0218-authd-keeps-the-accounts-and-passwords-are-argon2id-hashes.md)) | MIT OR Apache-2.0 (generic-array: MIT; subtle: BSD-3-Clause) |
| [smoltcp](https://github.com/smoltcp-rs/smoltcp) 0.14.0 by whitequark and the smoltcp contributors (patched, [ADR 0182](docs/adr/0182-the-kernel-patches-smoltcp-so-socket-memory-comes-back.md)), with [heapless](https://github.com/rust-embedded/heapless) by Jorge Aparicio, Per Lindgren, Emil Fresk and its contributors, [hash32](https://github.com/japaric/hash32) by Jorge Aparicio and [stable_deref_trait](https://github.com/storyyeller/stable_deref_trait) by Robert Grosse, [managed](https://github.com/m-labs/rust-managed) by whitequark, [byteorder](https://github.com/BurntSushi/byteorder) by Andrew Gallant, [bitflags](https://github.com/bitflags/bitflags) 1 and [cfg-if](https://github.com/rust-lang/cfg-if), from crates.io | The kernel's TCP/IP stack, in `kernel/net` ([ADR 0180](docs/adr/0180-the-tcp-ip-stack-runs-in-the-kernel-on-smoltcp.md)) | 0BSD (smoltcp, managed); MIT OR Apache-2.0 (heapless, hash32, stable_deref_trait, bitflags, cfg-if); Unlicense OR MIT (byteorder) |
| [ncurses](https://invisible-island.net/ncurses/) 6.6 by Thomas E. Dickey, and before him Zeyd M. Ben-Halim, Eric S. Raymond, Juergen Pfeifer and the Free Software Foundation | The terminal library less is built on, the terminal descriptions in `/system/resources/terminfo`, and `tput`, `tset`, `reset`, `clear`, `tic` and `infocmp` ([ADR 0086](docs/adr/0086-release-archives-and-autoconf-ports.md)) | X11-distribute-modifications-variant |
| [zlib](https://zlib.net) 1.3.2 by Jean-loup Gailly and Mark Adler | Compression, for `man` to read compressed manual pages | Zlib |
| [less](https://www.greenwoodsoftware.com/less/) 668 by Mark Nudelman | The pager | GPL-3.0-or-later OR BSD-2-Clause (the Less License, which Dukovo takes it under) |
| [mandoc](https://mandoc.bsd.lv/) 1.14.6 by Ingo Schwarze, Kristaps Dzonsons and contributors | `man`, `apropos`, `whatis` and `makewhatis`, and the formatter behind them | ISC, BSD-2-Clause, BSD-3-Clause |
| [GNU grep](https://www.gnu.org/software/grep/) 3.12, [GNU sed](https://www.gnu.org/software/sed/) 4.9, [GNU diffutils](https://www.gnu.org/software/diffutils/) 3.12 and [GNU patch](https://www.gnu.org/software/patch/) 2.8 by the Free Software Foundation and their maintainers, Jim Meyering, Paul Eggert, Assaf Gordon, Andreas Grünbacher and many others, with [gnulib](https://www.gnu.org/software/gnulib/) | `grep`, `egrep`, `fgrep`, `sed`, `diff`, `cmp`, `diff3`, `sdiff` and `patch`, as separate programs ([ADR 0087](docs/adr/0087-gpl-programs-among-the-ports.md)) | GPL-3.0-or-later |
| [The One True Awk](https://github.com/onetrueawk/awk) 20260426 by Alfred Aho, Peter Weinberger and Brian Kernighan, kept up by Brian Kernighan, Arnold Robbins and others | `awk` | Lucent's license (LicenseRef-onetrueawk) |
| [bc](https://git.gavinhoward.com/gavin/bc) 7.1.0 by Gavin D. Howard, with the PCG random number generator by Melissa O'Neill | `bc` and `dc` | BSD-2-Clause, MIT |
| [GNU gzip](https://www.gnu.org/software/gzip/) 1.14 by Jean-loup Gailly and Mark Adler, kept up by Jim Meyering and Paul Eggert, with [gnulib](https://www.gnu.org/software/gnulib/) | `gzip`, `gunzip`, `zcat` and the scripts that read .gz files: `zgrep`, `zless`, `zmore`, `zdiff`, `zcmp`, `zforce`, `znew` and `gzexe` | GPL-3.0-or-later |
| [bzip2](https://sourceware.org/bzip2/) 1.0.8 by Julian Seward, released by Mark Wielaard, with the `bzgrep`, `bzmore` and `bzdiff` scripts adapted from gzip's by Philippe Troin | `bzip2`, `bunzip2`, `bzcat`, `bzip2recover` and those scripts, and libbz2 for libarchive, Zip and file | bzip2-1.0.6 |
| [XZ Utils](https://tukaani.org/xz/) 5.8.4 by Lasse Collin, on Igor Pavlov's LZMA SDK, with the scripts adapted from gzip's by Jean-loup Gailly, Charles Levert, Paul Eggert and Andrew Dudman, and the work of the others its AUTHORS file names | `xz`, `unxz`, `xzcat`, the `lzma` names, `lzmainfo`, `xzgrep`, `xzdiff`, `xzless` and `xzmore`, and liblzma for libarchive, zstd and file | 0BSD (the scripts: GPL-2.0-or-later) |
| [LZ4](https://lz4.org) 1.10.0 by Yann Collet and contributors | `lz4`, `unlz4` and `lz4cat`, and liblz4 for libarchive and zstd | BSD-2-Clause (the `lz4` program: GPL-2.0-or-later) |
| [Zstandard](https://facebook.github.io/zstd/) 1.5.7 by Yann Collet, Nick Terrell, Przemysław Skibiński, W. Felix Handte and others at Meta | `zstd`, `unzstd`, `zstdcat`, `zstdmt`, `zstdgrep` and `zstdless`, and libzstd for libarchive and file | BSD-3-Clause (offered as BSD-3-Clause OR GPL-2.0-only; Dukovo takes BSD-3-Clause) |
| [libarchive](https://www.libarchive.org) 3.8.10 by Tim Kientzle, Martin Matuška and contributors, with BLAKE2 by Samuel Neves | `tar`, `cpio` and `unzip` (bsdtar, bsdcpio and bsdunzip) and `bsdcat` | BSD-2-Clause, BSD-3-Clause (from the University of California), CC0-1.0 OR Apache-2.0 (BLAKE2); its date parser is in the public domain |
| [Zip](https://infozip.sourceforge.net/Zip.html) 3.0 by Info-ZIP (Onno van der Linden, Christian Spieler, Ed Gordon, Steven M. Schweda and the others its license names), with the fixes Debian carries by Lior Kaplan, Christian Spieler, Santiago Vila, Shengjing Zhu, Florent 'Skia' Jacquet, Marcin Owsiany and Paul Marquess | `zip`, `zipcloak`, `zipnote` and `zipsplit` | Info-ZIP |
| [file](https://www.darwinsys.com/file/) 5.47 by Ian F. Darwin and Christos Zoulas, with the magic patterns of many contributors | `file` and the magic database it reads | BSD-2-Clause |
| [jq](https://jqlang.org) 1.8.2 by Stephen Dolan, kept up by David Tolnay, Emanuele Torre, Leonid S. Usov, Mattias Wadman, Muh Muhten, Nicolas Williams, Owen Ou, William Langford, itchyny and pkoppstein, with David M. Gay's dtoa, IBM's decNumber by Mike Cowlishaw, NetBSD's strptime and [Oniguruma](https://github.com/kkos/oniguruma) by K. Kosako | `jq` | MIT, dtoa, ICU (decNumber), BSD-2-Clause (Oniguruma, strptime), CC-BY-3.0 (its manual) |
| [GNU nano](https://www.nano-editor.org/) 9.2 by Chris Allegretta, kept up by Benno Schulenberg, with David Lawrence Ramsey and the other contributors | `nano` and `rnano`, the system's editor, and the syntax highlighting files in `/system/resources/nano` ([ADR 0090](docs/adr/0090-editors-cxx-and-default-settings.md)) | GPL-3.0-or-later |
| [nvi2](https://github.com/lichray/nvi2) 2.2.2 by Zhihao Yuan and contributors, from Keith Bostic's nvi of 4.4BSD, with its regular expressions by Henry Spencer | `vi`, `ex` and `view` | BSD-3-Clause |
| The Berkeley DB 1.85 library of 4.4BSD by Mike Olson, Margo Seltzer and Keith Bostic, as Debian's [db1-compat](https://tracker.debian.org/pkg/db1-compat) 2.1.3 keeps it | The database `vi` keeps the lines it edits in | BSD-4-Clause-UC (the advertising clause rescinded by the University of California in 1999) |
| [LLVM](https://llvm.org)'s [libc++](https://libcxx.llvm.org), libc++abi and libunwind 20.1.8, by the LLVM Project's contributors | The C++ standard library, run-time support and unwinder C++ programs link ([ADR 0090](docs/adr/0090-editors-cxx-and-default-settings.md)) | Apache-2.0 WITH LLVM-exception |
| [7-Zip](https://www.7-zip.org) 26.02 by Igor Pavlov, from Debian's copy without the RAR code, with Debian's fix by YOKOTA Hiroshi that hides typed passwords, and the LZFSE, Zstandard and xxHash decoders it carries by Apple, Meta and Yann Collet | `7zz`, and `7z`, a link to it ([ADR 0090](docs/adr/0090-editors-cxx-and-default-settings.md)) | LGPL-2.1-or-later, BSD-3-Clause, BSD-2-Clause |
| [libevent](https://libevent.org/) 2.1.13 by Niels Provos, Nick Mathewson, Azat Khuzhin and contributors, with arc4random by David Mazieres and Damien Miller | The event loop tmux is built on | BSD-3-Clause, ISC (arc4random) |
| [tmux](https://github.com/tmux/tmux) 3.7c by Nicholas Marriott and contributors, with code from OpenBSD by Todd C. Miller, Niels Provos and others, Tatu Ylonen's allocation wrappers, and base64 code by the Internet Software Consortium and IBM | `tmux`, the terminal multiplexer ([ADR 0091](docs/adr/0091-named-sockets-and-passed-descriptors.md)) | ISC, BSD-2-Clause, BSD-3-Clause, SSH-short, IBM's notice (LicenseRef-IBM-base64) |
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
| [argon2](https://github.com/RustCrypto/password-hashes) (with blake2, password-hash, digest, crypto-common, block-buffer, cpufeatures and base64ct by the RustCrypto developers, [subtle](https://github.com/dalek-cryptography/subtle), [generic-array](https://github.com/fizyk20/generic-array), [typenum](https://github.com/paholg/typenum), rand_core and [version_check](https://github.com/SergioBenitez/version_check)) | `ob image --user`, to hash an account's password | MIT OR Apache-2.0 (generic-array: MIT; subtle: BSD-3-Clause) |
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
| [exfatprogs](https://github.com/exfatprogs/exfatprogs) (`mkfs.exfat`, `fsck.exfat`) | Making the exFAT volumes Radix's exFAT tests and the `exfat-write` boot test use, and checking what Radix wrote to them | GPL-2.0-or-later |
| [The Sleuth Kit](https://www.sleuthkit.org/sleuthkit/) (`fls`, `icat`), by Brian Carrier and contributors | Reading back, as an exFAT reader of its own, what Radix wrote to the exFAT tests' volumes | IPL-1.0, CPL-1.0 and GPL-2.0-or-later (parts) |
| [exfat-fuse](https://github.com/relan/exfat) 1.4.0, by Andrew Nayenko | Writing, once, the exFAT volume Radix's exFAT tests read (`kernel/fs/exfat/src/fixtures`); none of its code is in the repository | GPL-2.0-or-later |
| [e2fsprogs](https://e2fsprogs.sourceforge.net/), by Theodore Ts'o and contributors (`mkfs.ext4`, `e2fsck`, `debugfs`, `tune2fs`) | Making the ext4, ext3 and ext2 volumes Radix's ext4 tests and the `ext4-write` and `ext4-power-cut` boot tests use, checking what Radix wrote to them, replaying their journals and reading their files back | GPL-2.0-only (its libraries: LGPL-2.0-only, BSD-3-Clause and MIT) |
| [Linux](https://kernel.org/)'s ext4, 6.18 | Writing, once, the ext4 volume with transactions left in its journal that Radix's ext4 tests replay (`kernel/fs/ext4/src/fixtures`); none of its code is in the repository | GPL-2.0-only |
| [btrfs-progs](https://github.com/kdave/btrfs-progs) (`mkfs.btrfs`) | Making the Btrfs filesystems Radix's Btrfs tests read; run as a host tool, the system's or Ubuntu 26.04's package, which `ob test` unpacks into `.cache/btrfs-progs` when the system's is too old, never built into Dukovo | GPL-2.0-only |
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
| [Python](https://www.python.org/) `hashlib` and `zlib`, [OpenSSL](https://www.openssl.org/) and [zlib](https://zlib.net/) by Jean-loup Gailly and Mark Adler | Reference values for the BLAKE2s and ChaCha20 tests, and the zlib streams the Btrfs decoder's tests inflate | PSF-2.0, Apache-2.0, Zlib |
| The [Zstandard](https://facebook.github.io/zstd/) program, Ubuntu 24.04's 1.5.5 | Making the zstd frames the Btrfs decoder's tests keep (`kernel/fs/btrfs/src/tests/zstd`) | BSD-3-Clause OR GPL-2.0-only |
| [Eclipse Temurin](https://adoptium.net/) | The Java runtime for TLC in CI | GPL-2.0-only WITH Classpath-exception-2.0 |
| [Ubuntu](https://ubuntu.com/) 24.04 | The build container's base image and CI's runners | Various free licenses |
| [Podman](https://podman.io/) and [Docker Engine](https://github.com/moby/moby) | Running the build container | Apache-2.0 |
| [GitHub Actions](https://github.com/features/actions): [checkout](https://github.com/actions/checkout), [cache](https://github.com/actions/cache), [upload-artifact](https://github.com/actions/upload-artifact), [setup-java](https://github.com/actions/setup-java) | CI | MIT |
| [Swatinem/rust-cache](https://github.com/Swatinem/rust-cache) | Caching builds in CI | LGPL-3.0-only |
| [taiki-e/install-action](https://github.com/taiki-e/install-action) | Installing cargo-deny in CI | Apache-2.0 OR MIT |
| [ntfs-3g](https://github.com/tuxera/ntfs-3g) 2022.10.3, by Tuxera and contributors (`mkntfs`, `ntfscp`, `ntfscat`) | Making the NTFS volumes Radix's NTFS tests and the `ntfs` boot test read, writing files into them and reading back their bitmaps | GPL-2.0-or-later |
| ntfs-3g's FUSE driver 2022.10.3, with `setfattr` from [attr](https://savannah.nongnu.org/projects/attr) | Writing, once, the NTFS volume Radix's NTFS tests and the `ntfs` boot test read (`kernel/fs/ntfs/src/fixtures`); none of its code is in the repository | GPL-2.0-or-later |

## The shell corpus

`ob test corpus` ([ADR 0103](docs/adr/0103-the-shell-corpus.md)) fetches
these projects, each at the commit `userspace/programs/nukesh/tests/corpus.toml`
pins, and runs their tests and scripts in a sandbox, under Nukesh and under
dash or bash, to check that Nukesh gives what those shells give. None of
their files is in the repository or in an image.

| Project | What we use it for | License |
| --- | --- | --- |
| [smoosh](https://github.com/mgree/smoosh), the executable POSIX shell semantics by Michael Greenberg and Austin J. Blatt | Its tests of the POSIX shell, which Nukesh as `sh` must pass, and their C helpers | MIT |
| [nvm](https://github.com/nvm-sh/nvm) by Tim Caswell, Jordan Harband and contributors | Its fast tests, in POSIX `sh`, with their mocks | MIT |
| [shUnit2](https://github.com/kward/shunit2) by Kate Ward | Its own tests, in POSIX `sh` | Apache-2.0 |
| [Bats](https://github.com/bats-core/bats-core) by Sam Stephenson and the bats-core contributors | Running rbenv's and pyenv's tests | MIT |
| [rbenv](https://github.com/rbenv/rbenv) by Sam Stephenson and contributors, and [pyenv](https://github.com/pyenv/pyenv), by Yamashita Yuu and contributors, which began as rbenv | Their commands, in bash, and their Bats tests | MIT |
| The installers of [Starship](https://github.com/starship/starship) (ISC), [Deno](https://github.com/denoland/deno_install) (MIT), [Docker](https://github.com/docker/docker-install) (Apache-2.0), [rustup](https://github.com/rust-lang/rustup) (MIT OR Apache-2.0), [Helm](https://github.com/helm/helm) (Apache-2.0), [K3s](https://github.com/k3s-io/k3s) (Apache-2.0), [Homebrew](https://github.com/Homebrew/install) (BSD-2-Clause) and [Oh My Zsh](https://github.com/ohmyzsh/ohmyzsh) (MIT), each by its project's contributors | Run as far as they go without a network | As listed |
| The entrypoints of the official [PostgreSQL](https://github.com/docker-library/postgres) (MIT), [Redis](https://github.com/docker-library/redis) (BSD-3-Clause), [MongoDB](https://github.com/docker-library/mongo) (Apache-2.0) and [RabbitMQ](https://github.com/docker-library/rabbitmq) (MIT) container images, by their maintainers at Docker, and of [NGINX](https://github.com/nginxinc/docker-nginx)'s (BSD-2-Clause), by F5 | Run as a container runs them, with stubs for the servers they start | As listed |
| [neofetch](https://github.com/dylanaraps/neofetch) 7.1.0 by Dylan Araps | A large program in bash | MIT |
| [dehydrated](https://github.com/dehydrated-io/dehydrated) by Lukas Schauer | An ACME client in bash | MIT |
| [Oils](https://github.com/oils-for-unix/oils) by Andy Chu and contributors | Its `configure` script, in POSIX `sh`, and that script's tests | Apache-2.0 |
| [dash](http://gondor.apana.org.au/~herbert/dash/) by Herbert Xu, from Kenneth Almquist's ash, and [GNU Bash](https://www.gnu.org/software/bash/) 5.2 by Chet Ramey | The shells Nukesh is compared with, as the build machine has them; Bats runs under bash | BSD-3-Clause (dash; one of its files GPL-2.0-or-later), GPL-3.0-or-later (bash) |
| [util-linux](https://github.com/util-linux/util-linux)'s `unshare`, `prlimit` and `setpriv` | The sandbox | GPL-2.0-or-later |

## Specifications and papers

We implement these from their published descriptions. No code is copied.

- ChaCha20, from [RFC 8439](https://www.rfc-editor.org/rfc/rfc8439), by
  Daniel J. Bernstein, specified by Yoav Nir and Adam Langley. Its test
  vectors check our implementation.
- BLAKE2s, from [RFC 7693](https://www.rfc-editor.org/rfc/rfc7693), by
  Jean-Philippe Aumasson, Samuel Neves, Zooko Wilcox-O'Hearn and Christian
  Winnerlein, specified by Markku-Juhani O. Saarinen and Jean-Philippe
  Aumasson.
- DEFLATE, by Phil Katz, and zlib's format, from
  [RFC 1951](https://www.rfc-editor.org/rfc/rfc1951) and
  [RFC 1950](https://www.rfc-editor.org/rfc/rfc1950), specified by L. Peter
  Deutsch and Jean-loup Gailly: the Btrfs zlib decoder.
- Zstandard, by Yann Collet, from
  [RFC 8878](https://www.rfc-editor.org/rfc/rfc8878), specified by Yann
  Collet and Murray Kucherawy, and XXH64, from Yann Collet's
  [xxHash specification](https://github.com/Cyan4973/xxHash/blob/dev/doc/xxhash_spec.md):
  the Btrfs zstd decoder and one of Btrfs's checksums.
- LZO1X, by Markus F. X. J. Oberhumer, as Linux's
  [`Documentation/staging/lzo.rst`](https://docs.kernel.org/staging/lzo.html)
  describes its format: the Btrfs LZO decoder. We use no LZO code.
- Btrfs's on-disk format, from the
  [Btrfs documentation](https://btrfs.readthedocs.io/) and what Linux's
  `mkfs.btrfs` writes.
- ext4's on-disk format and its journal's (jbd2), as Linux's
  ["ext4 Data Structures and Algorithms"](https://docs.kernel.org/filesystems/ext4/)
  describes them, and what Linux's `mkfs.ext4` writes: `radix-ext4`
  ([ADR 0141](docs/adr/0141-ext4-from-its-documentation-with-a-journal-of-whole-blocks.md)).
  We use no code of Linux's ext4 or of e2fsprogs.
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
- Microsoft's
  ["exFAT File System Specification"](https://learn.microsoft.com/en-us/windows/win32/fileio/exfat-specification)
  (2019), from which `radix-exfat` implements exFAT
  ([ADR 0140](docs/adr/0140-exfat-from-the-specification.md)).
- Intel's
  ["eXtensible Host Controller Interface for Universal Serial Bus (xHCI)"](https://www.intel.com/content/www/us/en/products/docs/io/universal-serial-bus/extensible-host-controler-interface-usb-xhci.html),
  revision 1.2 (2019), and the USB-IF's Universal Serial Bus 2.0 and 3.2
  specifications, from which `radix-xhci` and `radix-usb` drive USB host
  controllers and hubs and read devices' descriptors
  ([ADR 0146](docs/adr/0146-xhci-controllers-are-driven-by-a-kernel-thread-each.md),
  [ADR 0147](docs/adr/0147-usb-hubs-and-every-device-configured-by-its-controllers-thread.md)).
  We use no code of Linux's or the BSDs' USB stacks, or of QEMU's
  controller, which the boot tests run against.
- The USB-IF's "Device Class Definition for Human Interface Devices
  (HID)", version 1.11, and its "HID Usage Tables", version 1.5
  ([usb.org/hid](https://www.usb.org/hid)), from which `radix-hid` lays
  out report descriptors and decodes keyboards', mice's and tablets'
  reports
  ([ADR 0148](docs/adr/0148-usb-keyboards-and-mice-are-read-through-their-hid-reports.md)).
  Its keys are radix-input(2)'s, which are Linux's key codes; we use no
  code of Linux's `hid-core` or `hid-input`, or of the BSDs' HID drivers.
- Howard Hinnant's
  ["chrono-Compatible Low-Level Date Algorithms"](https://howardhinnant.github.io/date_algorithms.html),
  which FAT's dates are converted with.
- The OASIS Virtual I/O Device (virtio) specification, version 1.2: its
  modern PCI transport, split virtqueues, block device
  ([ADR 0053](docs/adr/0053-modern-virtio-only.md)) and network device
  ([ADR 0181](docs/adr/0181-virtio-net-copies-frames-through-slots-of-its-own.md)).
- Intel's Serial ATA Advanced Host Controller Interface (AHCI)
  specification, revision 1.3.1, the INCITS ATA/ATAPI Command Set
  (ACS-4) and the Serial ATA Revision 3 specification (SATA-IO), from
  which `radix-ahci` drives SATA disks: native command queuing, TRIM, the
  NCQ Command Error log and error recovery
  ([ADR 0112](docs/adr/0112-ahci-from-the-specification.md)).
- The NVM Express Base Specification, revision 2.0, and the NVM Command
  Set Specification, revision 1.0 (NVM Express, Inc.), from which
  `radix-nvme` drives NVMe controllers: queue pairs, PRP lists, Identify,
  Dataset Management, controller reset and shutdown
  ([ADR 0115](docs/adr/0115-nvme-from-the-specification.md)).
- The DMTF's "System Management BIOS (SMBIOS) Reference Specification",
  DSP0134, version 3.7.0 (2023), from which `radix-smbios` reads the
  firmware's tables for the kernel and `hwinfo`, and Linux's
  `/sys/class/dmi/id`, whose files and boot line Radix takes
  ([ADR 0231](docs/adr/0231-the-firmwares-smbios-tables-and-hwinfo.md)).
  We use no code of Linux's `dmi_scan` or of dmidecode.
- JEDEC's SPD layouts (JESD21-C Annex K for DDR3 and Annex L for DDR4,
  JESD400-5 for DDR5), its EE1004-v and JESD300-5 (SPD5118) for the
  EEPROMs that hold them, and JEP106's makers' codes, from which
  `radix-spd` reads and decodes the memory modules' SPD data; and
  Linux's `i2c_register_spd`, whose way of finding the EEPROMs by the
  SMBIOS memory kind, and whose `/sys/bus/i2c/devices` files, Radix
  takes ([ADR 0232](docs/adr/0232-memory-modules-spd-eeproms-over-the-smbus.md)).
  We use no code of Linux's `ee1004`, `at24` or `spd5118` drivers, or of
  i2c-tools' `decode-dimms`.
- AMD's Processor Programming References ("GPIO") and Intel's *100
  Series Chipset Family PCH Datasheet*, volume 2 ("GPIO"), from which
  `radix-gpio` drives AMD's and Sunrise Point-LP's GPIO controllers; the
  ACPI Specification's resource descriptors and `_AEI`, from which
  `radix-acpi` reads resource templates; and Linux's `pinctrl-amd`,
  `pinctrl-intel` and `gpiolib-acpi`, whose way of setting the
  controllers up and of running the firmware's GPIO events Radix takes
  ([ADR 0233](docs/adr/0233-gpio-controllers-and-the-firmwares-gpio-events.md)).
  We use no code of Linux's.
- The block device `ioctl` requests of Linux's user API (`<linux/fs.h>`),
  BLKGETSIZE64 to BLKDISCARD, whose numbers and meanings Radix's block
  devices take ([ADR 0111](docs/adr/0111-one-queue-for-every-disk.md)).
- The PCI Local Bus, PCI Express Base and PCI Firmware specifications
  (PCI-SIG): configuration space through ECAM and the ACPI MCFG table,
  base address registers, and MSI and MSI-X
  ([ADR 0052](docs/adr/0052-message-signalled-interrupts-only.md)).
- Intel's "Virtualization Technology for Directed I/O Architecture
  Specification", revision 4.1, and the DMAR table it defines, from which
  `radix-vtd` and `radix-acpi` drive VT-d remapping units: root and
  context tables, second-level page tables, queued and register-based
  invalidation and fault recording
  ([ADR 0164](docs/adr/0164-each-device-reaches-only-its-own-memory-behind-vt-d.md)),
  and interrupt remapping
  ([ADR 0166](docs/adr/0166-each-device-raises-only-its-own-interrupts.md)).
  We use no code of Linux's or the BSDs' IOMMU drivers, or of QEMU's
  unit, which the `iommu` boot test runs against.
- AMD's "AMD I/O Virtualization Technology (IOMMU) Specification",
  48882, revision 3.10, and the IVRS table it defines, from which
  `radix-amdvi` and `radix-acpi` drive AMD IOMMUs: the device table, the
  host page tables, the command buffer and the event log
  ([ADR 0165](docs/adr/0165-each-device-reaches-only-its-own-memory-behind-amd-vi.md)),
  and interrupt remapping (ADR 0166).
  `radix-iopt` holds the page-table code both IOMMUs share. The register
  offsets were checked against FreeBSD's `sys/x86/iommu/amd_reg.h`
  (BSD-2-Clause), from which no code is taken; nor is any from Linux's
  driver or QEMU's IOMMU, which the `iommu-amd` boot test runs against.
- The Intel 6300ESB I/O Controller Hub and ICH9 datasheets, from which
  `radix-watchdog` drives the i6300esb and the TCO timer
  ([ADR 0143](docs/adr/0143-spark-pets-the-chipsets-hardware-watchdog.md)).
  Where newer Intel chipsets moved the TCO timer and its NO_REBOOT bit,
  and the registers of AMD's FCH watchdog, come from FreeBSD's ichwd(4)
  and amdsbwd(4) (BSD-2-Clause) and from Intel's and AMD's descriptions
  of their patches to Linux
  ([ADR 0167](docs/adr/0167-newer-intel-chipsets-and-amds-fch-have-their-watchdog-too.md)).
  No code is taken from any of them.
- The rules by which `radix-mca` grades machine checks, and which end
  only a program, follow those of Linux's
  `arch/x86/kernel/cpu/mce/severity.c` (GPL-2.0), with the status bits
  of Intel's SDM, volume 3, chapter 17, and AMD's APM, volume 2, chapter 9
  ([ADR 0168](docs/adr/0168-bad-memory-ends-only-the-program-that-read-it.md)).
  No code is taken from it.
- How `radix-mca` claims banks for Intel's CMCI, starts AMD's error
  counters and stops their interrupts in a storm follows Linux's
  `arch/x86/kernel/cpu/mce/intel.c` and `amd.c` (GPL-2.0): the threshold
  of 1, the limit of 4095, 15 interrupts a second and vector 0xF9 are
  theirs. The registers are those of Intel's SDM, volume 3, chapter 17,
  and of AMD's BIOS and Kernel Developer's Guides and Processor
  Programming References (MCA_MISC0). The EDAC files under
  `/kernel/devices/system/edac` are laid out as Linux's
  (`Documentation/ABI/testing/sysfs-devices-edac`)
  ([ADR 0169](docs/adr/0169-corrected-errors-interrupt-and-memory-errors-are-counted.md)).
  No code is taken from them.
- The embedded controller's protocol, the battery's `_BIX`, `_BIF` and
  `_BST`, the AC adapter's `_PSR` and the lid's `_LID` are those of the
  ACPI specification 6.5 (chapters 10 and 12). How `radix-uacpi` reads a
  battery's status, capacity and level, and the files of
  `/kernel/class/power_supply` and `/processes/acpi/button/lid`, follow
  Linux's `drivers/acpi/battery.c`, `ac.c`, `button.c` and `ec.c`
  (GPL-2.0) and its `Documentation/ABI/testing/sysfs-class-power`: the
  one-second cache of a battery's state and the 500 ms an embedded
  controller has for each step are theirs
  ([ADR 0201](docs/adr/0201-batteries-adapters-and-lids-through-uacpi.md)).
  No code is taken from them.
- Intel's hardware P-states and AMD's CPPC registers are those of
  Intel's SDM, volume 3, section 15.4, and AMD's Processor Programming
  Reference for family 19h (MSRC001_02B0 to MSRC001_02B3). How
  `radix-pstate` asks for levels at boot, its energy preferences' names
  and values, its frequencies (100 MHz a level on Intel, the ranked AMD
  cores' levels 166 and 196) and the files of
  `/kernel/devices/system/cpu/cpufreq` follow Linux's
  `drivers/cpufreq/intel_pstate.c` and `amd-pstate.c` (GPL-2.0) and its
  `Documentation/admin-guide/pm/intel_pstate.rst` and `amd-pstate.rst`
  ([ADR 0202](docs/adr/0202-cpu-frequency-through-hwp-and-cppc.md)).
  No code is taken from them.
- Zen's scalable machine-check registers (MCA_CONFIG, MCA_IPID,
  MCA_SYND, MCA_DESTAT and MCA_DEADDR) and the hardware IDs and types of
  its banks' blocks are those of AMD's Processor Programming Reference
  for family 19h and 1Ah. Setting `McaX`, reading a memory controller's
  channel from its instance ID and clearing MCA_DESTAT follow Linux's
  `arch/x86/kernel/cpu/mce/amd.c` and `drivers/edac/amd64_edac.c`
  (GPL-2.0) ([ADR 0204](docs/adr/0204-zens-scalable-machine-check-banks.md)).
  No code is taken from them.
- Thermal zones, their trip points and the passive cooling's trend are
  those of the ACPI specification 6.5 (chapter 11). How `radix-uacpi`
  reads a zone, which trip points it leaves out as nonsense, the
  step-wise steps of passive cooling, the fifth of the CPUs' highest
  level each step takes off, the hardware protection's 30 seconds before
  a forced power-off, and the files of `/kernel/class/thermal` follow
  Linux's `drivers/acpi/thermal.c`, `drivers/thermal/gov_step_wise.c`,
  `drivers/acpi/processor_thermal.c` and `kernel/reboot.c` (GPL-2.0) and
  its `Documentation/driver-api/thermal/sysfs-api.rst`
  ([ADR 0203](docs/adr/0203-thermal-zones-through-uacpi.md)).
  No code is taken from them.
- The CPUs' temperature sensors: AMD's System Management Network and the
  SMU's thermal block, from AMD's Processor Programming Reference for
  families 17h, 19h and 1Ah, and Intel's digital thermal sensors, from
  Intel's SDM (volume 3, section 15.8). Which of AMD's models have which
  die registers, the data fabric devices to look for, the root
  complex's SMN index and data registers, TjMax's default and the files
  of `/kernel/class/hwmon` follow Linux's `drivers/hwmon/k10temp.c`,
  `drivers/hwmon/coretemp.c` and `arch/x86/kernel/amd_node.c` (GPL-2.0)
  and its `Documentation/hwmon/sysfs-interface.rst`; `k10temp`'s die
  registers come from the zenpower project's experiments
  ([ADR 0205](docs/adr/0205-the-cpus-temperature-sensors.md)).
  No code is taken from them.
- The chipset's SMBus controller: Intel's, from the ICH9 and PCH
  datasheets, and that of AMD's FCH, from AMD's BIOS and Kernel
  Developer's Guides and Processor Programming References. The FCH's
  port selection and host semaphore, the adapters' names and leaving
  the controller to the firmware's AML follow Linux's
  `drivers/i2c/busses/i2c-piix4.c` and `i2c-i801.c` (GPL-2.0)
  ([ADR 0206](docs/adr/0206-the-chipsets-smbus-controller.md)).
  No code is taken from them.
- Framework's embedded controller: the Chromium EC's host command
  protocol, its memory map and its commands, from `include/ec_commands.h`
  of the Chromium EC (BSD-3-Clause, The ChromiumOS Authors), and
  Framework's fan configuration command and the Framework Desktop's fans,
  LEDs and ARGB output, from Framework's fork of it
  (github.com/FrameworkComputer/EmbeddedController, BSD-3-Clause). Where
  each machine has the memory map and the files of `cros_ec`'s hwmon
  device follow Linux's `drivers/platform/chrome/cros_ec_lpc.c` and
  `drivers/hwmon/cros_ec_hwmon.c` (GPL-2.0)
  ([ADR 0207](docs/adr/0207-framework-embedded-controller.md)).
  No code is taken from them.
- `ectool`: its commands' words, help and output follow those of the
  Chromium EC's `util/ectool.cc` (BSD-3-Clause, The ChromiumOS Authors),
  and what the kernel checks before a fan curve reaches the EC follows
  how Framework's fork uses one
  ([ADR 0208](docs/adr/0208-programs-set-the-fans-leds-and-argb-header-through-the-platform-capability.md)).
  No code is taken from them.
- `sensors` and `lsusb`: their output and options follow those of
  [lm-sensors](https://github.com/lm-sensors/lm-sensors)' sensors
  (GPL-2.0-or-later), with libsensors' names for chips and adapters, and
  of [usbutils](https://github.com/gregkh/usbutils)' lsusb
  (GPL-2.0-or-later); the files of `/kernel/bus/usb` and the way devices
  are numbered follow Linux's `/sys/bus/usb` and
  `Documentation/ABI/stable/sysfs-bus-usb`
  ([ADR 0230](docs/adr/0230-sensors-and-lsusb-with-the-usb-id-database.md)).
  No code is taken from them.
- The MBR partition table, and the GUID partition table of the UEFI
  specification with its CRC32 checks.
- The System V ABI's ELF format and the x86-64 psABI, by Michael Matz,
  Jan Hubička, Andreas Jaeger and Mark Mitchell and maintained by H.J. Lu
  and others, which `radix-elf` reads, with its `R_X86_64_RELATIVE`
  relocations.
- Synopsys's DesignWare DW_apb_i2c registers, as Intel's *100 Series
  Chipset Family PCH Datasheet* (LPSS) and AMD's Processor Programming
  References give them, from which `radix-i2c` drives the I2C
  controllers; NXP's *I2C-bus specification and user manual* (UM10204),
  whose SCL times its counts are computed from; the ACPI Specification's
  I2C serial bus connection; and Linux's `i2c-designware`, `intel-lpss`
  and `i2c-core-acpi`, whose way of setting the controllers up, of
  choosing the bus's speed and of naming the buses and devices in
  `/sys/bus/i2c` Radix takes
  ([ADR 0234](docs/adr/0234-designware-i2c-controllers-and-the-devices-acpi-names-on-them.md)).
  We use no code of Linux's.
- Microsoft's *HID over I2C Protocol Specification*, version 1.00, from
  which `radix-i2c` reads a touchpad's HID descriptor, resets it and
  reads its reports, with the ACPI `_DSM` that gives the descriptor's
  register; and Linux's `i2c-hid-acpi` and `i2c-hid-core`, whose order
  of setting a device up Radix takes
  ([ADR 0235](docs/adr/0235-hid-over-i2c-touchpads-are-read-as-mice.md)).
  We use no code of Linux's.
- NTFS's on-disk format, as the Linux-NTFS project's
  ["NTFS Documentation"](https://flatcap.github.io/linux-ntfs/ntfs/), by
  Richard Russon, Yuval Fledel and others, describes it, with Microsoft's
  [[MS-FSCC]](https://learn.microsoft.com/en-us/openspecs/windows_protocols/ms-fscc/)
  for reparse points and file attributes and
  [[MS-XCA]](https://learn.microsoft.com/en-us/openspecs/windows_protocols/ms-xca/)
  for LZNT1 compression, and what ntfs-3g writes: `radix-ntfs`
  ([ADR 0236](docs/adr/0236-ntfs-is-our-own-implementation-read-only-first.md)),
  which writes NTFS as ntfs-3g does, emptying Windows' log before its
  first change and refusing names Windows could not open
  ([ADR 0237](docs/adr/0237-ntfs-is-written-as-ntfs-3g-writes-it-its-log-emptied.md)).
  We use no code of Linux's ntfs3, of ntfs-3g or of the `ntfs` crate.

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
  Linux's `calc_load`, with its constants and rounding, is how Radix's
  load averages are kept, and its sampling of user and system time at
  the tick how CPU time is split
  ([ADR 0092](docs/adr/0092-cpu-time-split-and-load-averages.md)); its
  `sched_setaffinity` and its I/O priority classes, with their numbers,
  are Radix's ([ADR 0093](docs/adr/0093-cpu-affinity-and-io-classes.md));
  its `/dev/kmsg` record format is Radix's
  ([ADR 0094](docs/adr/0094-proc-for-the-native-tools-kmsg-and-opened-paths.md)).
  No code is taken.
- The Linux administration tools whose names, options and output
  Dukovo's native tools take, so that an administrator's habits carry
  over ([ADR 0096](docs/adr/0096-native-administration-tools-as-one-program.md)):
  [procps-ng](https://gitlab.com/procps-ng/procps) 4.0 (ps, top, kill,
  pgrep, pkill, free, vmstat, w, uptime, watch),
  [util-linux](https://github.com/util-linux/util-linux) 2.39 (renice,
  ionice, taskset, dmesg, script, column with libsmartcols' tables, and
  hexdump, whose format strings come from 4.4BSD's hexdump), GNU
  [coreutils](https://www.gnu.org/software/coreutils/) (chmod, chown,
  chgrp, df, who, users), Sébastien Godard's
  [sysstat](https://github.com/sysstat/sysstat) (pidstat), Vic Abell's
  [lsof](https://github.com/lsof-org/lsof), now kept by the lsof-org
  team, [strace](https://strace.io/) (rtrace, and its `-c` table), xxd
  by Juergen Weigert, as [Vim](https://www.vim.org/) ships it, and
  [tree](https://oldmanprogrammer.net/source.php?dir=projects/tree) 2.1
  by Steve Baker and its other authors. Their output was compared with
  ours on Linux while ours were written. No code is taken: ours are
  written from their manual pages and behaviour.
- Linux's console, whose escape sequences (console_codes(4)), `linux`
  terminfo entry, key sequences and Alt+F1 to Alt+F6 switching are what
  consoled's terminals do
  ([ADR 0100](docs/adr/0100-consoled-virtual-consoles-in-a-program.md));
  its input event codes, which `/dev/kbd` gives
  ([ADR 0097](docs/adr/0097-keyboard-events-and-the-framebuffer-for-programs.md));
  and the OSDev wiki's pages on the 8042 PS/2 controller and the PS/2
  keyboard, which the i8042 driver follows. systemd's `shutdown`,
  `reboot`, `poweroff` and `halt`, and Debian's `service`, whose command
  lines ours take, and runit, s6 and systemd as points of comparison for
  Spark's supervision
  ([ADR 0101](docs/adr/0101-spark-supervises-services.md)). No code is
  taken.
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
- [GNOME Files](https://apps.gnome.org/Nautilus/) and
  [Nemo](https://github.com/linuxmint/nemo): a directory's `.hidden`
  file, whose names a listing leaves out, which keeps the root's Unix
  names out of `ls /` ([ADR 0113](docs/adr/0113-plain-names-for-the-root-folders.md)).

The roadmap names more projects whose ideas later phases will follow, such as
macOS, Windows, Ubuntu, GNOME, Hyprland and TMOG. They are added here when
that work lands.
