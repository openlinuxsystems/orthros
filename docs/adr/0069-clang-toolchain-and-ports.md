<!--
SPDX-FileCopyrightText: 2026 The Dukovo contributors
SPDX-License-Identifier: Apache-2.0 OR MIT
-->

# 0069. Software from elsewhere is built by ports: pinned sources, patches and a script, with the host's Clang wrapped for Radix

- Status: Accepted
- Date: 2026-10-22

## Context

From Phase 8 on, much of Dukovo is software from elsewhere: the C
library first, then Lua and the C library's tests, and in Phase 9 the
core utilities, `less`, `nano`, archivers and more. Each needs its
source fetched exactly, patched for Radix, configured and built for a
target that is not the host, with every license recorded (ADR 0021,
ADR 0026). A cross compiler built from source (GCC or LLVM) would add
an hour to a fresh build and a toolchain to maintain; the host's Clang
already targets x86-64 and only needs to be told about Radix.

## Decision

- A **port** is a directory `ports/<name>/` with a `recipe.toml` (name,
  version, summary, SPDX license, home page, a git repository and the
  full commit it is pinned to, extra sources with their own commits and
  licenses, the ports it depends on, the files it puts in the boot
  archive, and its notice files when it has no LICENSE file), patches
  applied in name order, and a `build.sh` run in a fresh copy of the
  patched source (ob(1), PORTS).
- `./ob ports` (and `./ob build` first thing) fetches each source with
  git once, by commit, into `target/ports/`, checks that it is that
  commit, and builds each port after those it needs, again only when its
  directory, the toolchain or a port it depends on changed. A
  **toolchain port** installs into `out/toolchain` and never reaches an
  image. Every port's license and notices go into the image's license
  manifest.
- The **toolchain is the host's Clang 20 or later, LLD and LLVM's
  tools**, wrapped by scripts `ob` writes into `out/toolchain/bin`:
  `x86_64-radix-cc` compiles for Linux's x86-64 ABI (Radix's,
  ADR 0068) with Linux's macros taken away and `__radix__` and
  `__dukovo__` given, against the sysroot's headers only, and links
  against the C library's start files, the C library and compiler-rt's
  builtins. Two Meson cross files describe Radix to Meson, one for the
  C library itself and one for programs. Build scripts get the
  toolchain in `PATH` and the usual variables (`CC`, `AR`, `CFLAGS`...)
  with the host's own flags taken away.
- `ob doctor` checks for every tool the ports need, and CI and the build
  container install them.

## Consequences

- A new program is a recipe, its patches and a short script; Lua 5.4.7
  was the first, and its own test suite passes on Radix.
- Builds use nothing a recipe does not name, so an image's license
  manifest lists everything in it, and a build is the same wherever it
  runs, given the same Clang major version.
- Hosts need Clang 20 (Ubuntu 24.04 and Linux Mint 22 have it as
  `clang-20`); an older host uses `./ob --container`.
- Fetching needs the network on the first build; `target/ports` keeps
  the sources after that, and `ob clean --all` removes them.
