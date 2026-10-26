<!--
SPDX-FileCopyrightText: 2026 The Dukovo contributors
SPDX-License-Identifier: Apache-2.0 OR MIT
-->

# 0073. Programs start in C.UTF-8, compiled from glibc's locale data; named locales wait for Phase 12

- Status: Accepted; the locales are in `/system/libraries/locale` since [0114](0114-plain-names-inside-system.md)
- Date: 2026-10-22

## Context

Text on Dukovo is UTF-8: file names, terminals, the shell and every
program of Phase 9 on. In the C locale a C program's characters are
bytes, so `mbrtowc`, `wcwidth`, `iswalpha` and the wide-character
functions see no letters beyond ASCII, and programs that call
`setlocale(LC_ALL, "")` expect the environment to name a UTF-8 locale.
glibc and musl both have C.UTF-8 for this. mlibc reads locales in
glibc's compiled format, but has none of its own. Named locales (dates,
numbers and collation per language) need CLDR data and belong to
Phase 12, with time zones.

## Decision

- A **`locales` port** compiles **C.UTF-8** from glibc 2.42's locale
  sources (`localedata/locales/C` and the UTF-8 character map, whose
  character classes and case mappings are Unicode 16.0's) with the build
  host's `localedef` into `/system/lib/locale/C.utf8`. Only data is
  taken, no code: the sources carry no copyright, and their tables are
  made from the Unicode Character Database under the Unicode License v3,
  whose text goes into the image's license manifest.
- mlibc looks for compiled locales in `LOCPATH`, then
  `/system/lib/locale` (there is no `/usr`), names a locale as it was
  asked, as glibc does, and has `MB_CUR_MAX` 4 in a UTF-8 locale and 1
  in C (patch 0009).
- Spark gives every POSIX program **`LANG=C.UTF-8`**. A program that
  calls `setlocale(LC_ALL, "")` gets UTF-8 characters with Unicode's
  classes; one that does not stays in the C locale, as POSIX says.
- Asking for any other locale fails, so programs fall back to C as they
  would on a system without it. The `format` C test checks both.

## Consequences

- Wide characters, `printf`'s `%lc` and `%ls`, and character classes
  work for all of Unicode in every program from the start, as Phase 9's
  shell and utilities need.
- The locale costs 412 KiB in each image.
- The host's `localedef` (glibc's `libc-bin`) is one more build tool,
  which `ob doctor` checks for. Its output format has not changed in
  many glibc releases, and the `format` test would show if it did.
- Phase 12 adds named locales from CLDR and decides whether they stay in
  glibc's format or a format of our own.
