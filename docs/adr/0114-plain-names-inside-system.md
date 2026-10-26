<!--
SPDX-FileCopyrightText: 2026 The Dukovo contributors
SPDX-License-Identifier: Apache-2.0 OR MIT
-->

# 0114. The folders inside /system have plain names, and Unix's names are hidden links to them

- Status: Accepted
- Date: 2026-10-26

## Context

ADR 0113 gave the root's folders plain names and kept Unix's names as
hidden links. Inside `/system` the Unix names were left: `bin` for the
programs, `lib` for the libraries, `share` for what else the programs
read, and `lib/defaults/etc` for the settings the system ships. Each
needs Unix to decode: `bin` is short for binaries, though the folder
holds scripts too; `share` is left over from files shared between
machines of different kinds.

Their paths are compiled into programs: the dynamic linker's path is in
every dynamically linked program, the C library looks for locales in
`/system/lib/locale`, ncurses for terminal descriptions in
`/system/share/terminfo`, mandoc for pages in `/system/share/man`. Every
one of those programs is built from this repository, so the names are
cheap to change now. Once bundles (Phase 13) carry programs built
against the dynamic linker's path, it costs a migration.

## Decision

- **The folders are renamed:**

  | Was | Is | What it holds |
  | --- | --- | --- |
  | `/system/bin` | `/system/commands` | The programs, where `PATH` leads |
  | `/system/lib` | `/system/libraries` | The libraries, the dynamic linker and the locales |
  | `/system/share` | `/system/resources` | What else programs read: manual pages, terminal descriptions, completions, licenses |
  | `/system/lib/defaults/etc` | `/system/resources/default-settings` | The settings the system ships, which Spark copies into `/system/settings` |

  The dynamic linker is `/system/libraries/ld.so`, and the first program
  is `/system/commands/spark`. `PATH` is `/system/commands` for services
  and `/system/commands:/bin:/usr/bin` in the shell; the C library's
  search path without `PATH`, `confstr(_CS_PATH)` and `paths.h` name
  `/system/commands`.
- **Unix's names stay, as links in the boot archive:** `/system/bin ->
  commands`, `/system/lib -> libraries` and `/system/share -> resources`.
  `/bin` and `/sbin` lead to `system/commands` directly, and `/usr`
  still leads to `system`, so `/usr/bin/env`, `/usr/lib/ld.so` and
  `/usr/share/man` work. `/system/.hidden` lists the three links, so
  listings leave them out (ADR 0113). The build tool refuses a file
  under one of the links: it belongs in the folder the link leads to.
- **Ports install into the new folders.** The toolchain's configure
  wrapper gives every GNU configure script `--bindir`, `--sbindir`,
  `--libdir`, `--libexecdir` and `--datarootdir` under the prefix, and
  `--sysconfdir` and `--localstatedir` for `/system/settings` and
  `/system/state`; ports with their own build systems are given the
  same names. The build's sysroot uses the same layout, so the
  compilers link from `/system/libraries` there.
- **Folders that never reach the image keep their names:** the
  sysroot's `/system/include` holds headers for building, and Dukovo
  has no compiler yet. When it does, its headers get a plain name too.

## Consequences

- `ls /system` lists `boot`, `commands`, `libraries`, `live`,
  `resources`, `settings` and `state` (and `tests` in test images).
- Programs and scripts that use `/bin`, `/usr/bin`, `/usr/lib` or
  `/usr/share` keep working through the links, at the cost of one or two
  more steps in the kernel's walk of the path.
- `less` now looks for its system settings in `/system/settings`, where
  the configure wrapper's `--sysconfdir` puts them: it looked in
  `/system/etc`, which ADR 0113 removed.
- A program linked before this change names `/system/lib/ld.so`, which
  still leads to the dynamic linker.
