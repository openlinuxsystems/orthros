<!--
SPDX-FileCopyrightText: 2026 The Dukovo contributors
SPDX-License-Identifier: Apache-2.0 OR MIT
-->

# 0080. Programs in Rust from elsewhere build with crates.io crates patched for Radix

- Status: Accepted
- Date: 2026-10-23

## Context

Phase 9 brings programs in Rust that Dukovo did not write: uutils'
core utilities and findutils now, and more later. Each one builds with
Rust's standard library for Radix (ADR 0070). Each also depends on
crates from crates.io that pick their system calls, types and constants
by `target_os`, and none of them knows `radix`:

- `libc`;
- `nix` and `rustix`;
- `errno` and `getrandom`;
- `mio`;
- uutils' own library, `uucore`.

Each crate is shared by many programs. Patching it inside every program
would repeat the same change in each port. Telling the crates that Radix
is Linux would make them reach for Linux system calls that mlibc does
not have.

The standard library has its own copy of `libc` (0.2.189 in rust-src).
If a program's `libc` were another version, the program would have two
Radix modules, which could disagree.

## Decision

- **One port for the patched crates, `ports/rust-crates`.**
  - Each `[[crate]]` entry in its recipe names a crate, a version, the
    SHA-256 of the `.crate` file (the same one Cargo.lock records) and
    the crate's license.
  - `ob` downloads the crate from crates.io, checks the hash, unpacks it
    and applies `patches/<name>-<version>.patch`.
  - The port installs the patched crate into `out/toolchain/rust/crates`.
    Next to it goes a `[patch.crates-io]` file that gives the patched
    crate in place of crates.io's.
  - The port keeps one version of each semver line, the one programs'
    lock files name.
  - The first crates are:
    - errno 0.3.14;
    - getrandom 0.2.17 and 0.4.3;
    - mio 1.2.4;
    - nix 0.30.1 and 0.31.3;
    - rustix 1.1.5;
    - uucore 0.9.0.
- **Cargo for Radix, `x86_64-radix-cargo`** (`$CARGO_RADIX` in a port's
  build script). It runs Cargo with:
  - the `x86_64-unknown-radix` target and the rust-std sysroot;
  - the toolchain's C compiler as the linker and as the `cc` crate's
    compiler;
  - the patch file of every patched crate whose name the workspace's
    Cargo.lock lists. With no Cargo.lock yet, it passes all of them.
- **One `libc` for everything.** The rust-std port builds the standard
  library with crates.io's `libc` 0.2.191 in place of the version
  rust-src vendors. That one crate carries the Radix module, which `ob`
  checks against mlibc's headers. Programs get the same version, so the
  standard library and the crates share one definition of each type.
- **A port's Cargo.lock names the patched versions.** A port patches
  its program's lock file (as uutils-coreutils'
  `0002-lock-patched-crates.patch` does), so each build uses the same
  crates.
- **Ported programs get a license area of their own, `Port`** (ADR 0021).
  - It allows the kernel area's licenses plus `CC0-1.0`, a public domain
    dedication that uutils' `tail` uses through the `notify` crate.
  - A ported program runs as a process of its own, so what it links
    reaches no Dukovo code.
  - Each port's build script lists, from `cargo tree`, the crates its
    program links (not the procedural macros, which only run at build
    time). The license manifest checks each of them.
  - The image carries the text of every license any of those crates
    names, including the alternatives it was not taken under. That
    added `CC0-1.0`, `MIT-0` and `GPL-2.0-only`.

## Consequences

- Porting another program in Rust is a recipe, a build script and,
  where it needs one, a lock-file patch. Only a crate that has not been
  patched yet needs more: a new `[[crate]]` entry and its patch.
- The patches are written to go upstream, as mlibc's are (Third-party
  code and credits in CONTRIBUTING.md): the `radix` OS in libc, nix,
  rustix, errno, getrandom, mio and uucore.
- Cargo warns of a patch it does not use. That happens when the lock
  file names the same crate at another version (nix 0.30.1 for
  findutils, 0.31.3 for coreutils), or when the crate is part of the
  program's own tree (coreutils has its own `uucore`). The warning is
  harmless.
- A new rust-src may vendor a newer `libc`. When the pinned toolchain
  moves, the rust-std port's swap has to move with it.
