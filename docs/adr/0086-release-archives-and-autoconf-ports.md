<!--
SPDX-FileCopyrightText: 2026 The Dukovo contributors
SPDX-License-Identifier: Apache-2.0 OR MIT
-->

# 0086. Ports come from release archives or git, and autoconf packages build through one wrapper

- Status: Accepted
- Date: 2026-10-23

## Context

Phase 9 ports the C programs a server's shell needs: ncurses, less,
mandoc for `man`, GNU grep, sed, diffutils and patch, awk and bc, and
later nano, tmux, the compression tools and more. Until now every port's
source was a git commit (ADR 0069).

Many of these projects release more than their repository holds. GNU's
release archives carry the configure scripts, gnulib's code and the
manual pages that their repositories make with autoconf, automake,
gettext and help2man. less's carries files its repository makes with
Perl. Building from git would put all of those tools on every build
host and run a project's own release steps in ours.

Some homes cannot be reached from every place Dukovo is built. The
cloud machines its agents build on cannot reach ftp.gnu.org,
mandoc.bsd.lv or greenwoodsoftware.com. GitHub's CI can. Ubuntu's
source pool can be reached from both, and it keeps each upstream
release archive unchanged as `<name>_<version>.orig.tar.*`.

Most of these packages build with an autoconf `configure` script, which
must be told it is cross compiling, and whose `config.sub` does not
know Radix.

## Decision

A recipe's source is a git commit, as before, or a release archive: its
`url`, its `sha256`, and `mirrors` that serve the same file. `ob` tries
the URL and then each mirror, and uses the first copy whose SHA-256 is
the one pinned. It unpacks the archive once into `target/ports/sources`.
A copy with another checksum is never used, so a mirror can only be
unreachable, never wrong. CI downloads from the projects' own homes, so
it checks every pin against upstream on each run. The archives were
also checked against the maintainers' signatures where Ubuntu keeps
them (grep, diffutils and less).

`ob` copies a source into the build directory with each file's time
kept. Otherwise make would find a generated file older than what it is
made from, and run autoconf or automake again.

The toolchain gains `x86_64-radix-configure`, which runs a package's
`configure` for Radix:

- it puts a script in front of each `config.sub` in the source that
  names `x86_64-radix` and hands every other name to the original;
- it gives the build machine's name as `--build` and `x86_64-radix` as
  `--host`, so configure always knows it is cross compiling. A Radix
  program can start on Linux, whose system calls it would then make;
- it sets `CONFIG_SITE` to `out/toolchain/autotools/config.site`,
  which answers for Radix the questions configure can only answer by
  running a program, starting with gnulib's `strcasecmp` test;
- it sets `CONFIG_SHELL=/bin/sh`, so scripts a package installs, such
  as grep's `egrep`, start with `/bin/sh` rather than the host's bash.

Packages with configure scripts of their own get their answers in their
own way: mandoc's in a `configure.local` written by its `build.sh`,
zlib's through `CHOST`, bc's through `HOSTCC`. Programs that a build
runs on the build machine are built with the host's `cc`: ncurses'
`tic`, which compiles the terminal descriptions, awk's operator table
and bc's library embedder.

The ports added with this are zlib 1.3.2, ncurses 6.6 (with 45 terminal
descriptions in `/system/share/terminfo`), less 668, mandoc 1.14.6, GNU
grep 3.12, sed 4.9, diffutils 3.12 and patch 2.8, the One True Awk
(20260426) and Gavin Howard's bc 7.1.0. GNU's are under the GPL, which
ADR 0087 allows for ports.

## Consequences

- A port can follow its project's releases, whatever tools made them.
  A build needs no autotools on the host. It does need make, curl, xz
  and, for awk's grammar, bison; `ob doctor`, CI and the build container
  install them.
- The license manifest names an archive port by its URL and its
  SHA-256.
- The plan named OpenBSD's patch. GNU patch replaces it: OpenBSD's
  files carry four licenses, two of which SPDX has no identifier for,
  and it cannot apply the renames, copies and mode changes in git's
  patches, which administrators apply most.
- `config.site` grows as ports ask new questions. Each answer says
  which package asked and why the answer holds on Radix.
- ncurses' configure does not know Radix and would ask the C library for
  POSIX 1995 alone; its `build.sh` asks for `_GNU_SOURCE`, as it does on
  Linux, whose interface Radix's C library follows.
- `apropos` and `whatis` need mandoc's database, which `makewhatis`
  writes into `/system/share/man`. The image's read-only `/system` gets
  one when the image is built from all its pages, in a later Phase 9
  change; `man` itself needs none.
