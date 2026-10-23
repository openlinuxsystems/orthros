<!--
SPDX-FileCopyrightText: 2026 The Dukovo contributors
SPDX-License-Identifier: Apache-2.0 OR MIT
-->

# 0072. The C library writes and reads numbers and times as glibc does, with musl's code where mlibc's was wrong

- Status: Accepted
- Date: 2026-10-22

## Context

Ported programs and their test suites expect `printf`, `strtod`,
`scanf`, `strftime` and the time conversions to give exactly what glibc
and musl give: the true digits of a floating-point number at any
precision, every conversion and flag, the right day of the year. Lua's
test suite, the first ported program's, found mlibc 7.0.0 short of that
in many places: `printf("%f", 1e20)` and `strtod("1e308")` came out
wrong (its floating point went through `uint64_t` and `pow()`),
`strftime` ended the program on `%w`, `mktime` ignored the time zone,
31 December was day 366 of every year, and several `printf` flags failed
assertions. Writing our own exact floating-point conversion would be
weeks of work and risk; musl's is small, MIT licensed and has been right
for years.

## Decision

- mlibc's **floating-point conversions** (`printf`'s `%e`, `%f`, `%g`
  and `%a`, `strtod` and its kin, `scanf`'s floating-point conversions),
  **`strftime` and `wcsftime`**, and its **calendar code** (`gmtime`,
  `localtime`, `mktime`, `timegm`) are musl 1.2.5's, kept as headers in
  `ports/mlibc/musl/` with musl's copyright lines and made templates
  over mlibc's sinks, character types and locales (patches 0004, 0006
  and 0007). The locale's decimal point and `%'f`'s grouping, which
  musl does not do, are added.
- frigg's `printf` is brought to the commit mlibc's main branch pins
  (patch 0002), and patch 0003 makes the rest behave as glibc's: `%p`
  with widths and flags, `%c` and `%s` ignoring flags they cannot use, a
  zero at precision 0 keeping its sign, `%C` and `%S`, positional
  arguments, and `EINVAL` rather than an ended program for an unknown
  conversion.
- Other gaps the suites found are fixed the way glibc and musl behave:
  stdio's errors (patch 0005), random temporary-file names from
  `getentropy` (0008), `getpriority`'s failure value (0010), `preadv`
  and `pwritev` (0011). Each patch says what it changes and why, and is
  meant for upstream.
- **libc-test**, the musl project's tests of C libraries, runs in the
  `posix` boot test too, and what it found is fixed the same way
  (patches 0013 to 0022). Among them, `scanf` and `wscanf` are rebuilt
  on musl's `vfscanf`, with its integer scanner (`int-scan.hpp`):
  mlibc's took no field width for numbers, ended `%c` early and lost
  `%*2d`'s width. `inet_pton` and `inet_ntop` are musl's
  (`inet.hpp`), and so are `lrand48`, `nrand48` and `lcong48`.
- The `format` C test holds the C library to **glibc 2.39's answers**
  for 1,428 cases: integer and floating-point conversions with every
  flag, width and precision, `strtod`, `strtof`, `strtold` and `scanf`,
  `strftime`, and broken-down times either side of 1970 and across leap
  years. `expected/format-cases.c` generates the answers on a glibc
  system, and its output, `format-cases.h`, is checked in.

## Consequences

- C and Rust programs that print or parse numbers get the same text as
  on Linux; Lua's test suite, mlibc's own tests and libc-test's pass.
- About 2,000 lines of musl live in the port, under musl's MIT license
  and credited in CREDITS.md; a musl release with a fix in them is
  merged by hand, and the `format` test shows whether anything moved.
- Where glibc and musl disagree, glibc's answer is the one tested,
  since most ported software is written and tested against it.
