<!--
SPDX-FileCopyrightText: 2026 The Dukovo contributors
SPDX-License-Identifier: Apache-2.0 OR MIT
-->

# 0021. Every image carries a checked license manifest

- Status: Accepted; the licenses are in `/system/resources/licenses` since [0114](0114-plain-names-inside-system.md)
- Date: 2026-10-20

## Context

Dukovo is `Apache-2.0 OR MIT` ([0002](0002-license-apache-2.0-or-mit.md)),
but its images already hold code under other terms: the Spleen fonts in
the kernel (BSD-2-Clause, [0015](0015-console-font-spleen.md)), the Rust
standard library crates that `-Zbuild-std` compiles into the kernel
(compiler_builtins includes `Apache-2.0 WITH LLVM-exception` code), GRUB
(GPL-3.0-or-later, [0010](0010-phase-0-boot-image.md)), and, in local
builds only, closed Intel microcode ([0007](0007-closed-firmware-separate-package.md)).
Later phases add ported C programs and bundles with private libraries.
The project wants compliance enforced by the build, not by memory.

Until now one `deny.toml` covered both workspaces. cargo-deny reads the
file in the directory it runs from, so the kernel was checked against the
build tool's list, and nothing checked the standard library crates or
GRUB, or put their notices into the image.

## Decision

- Licenses are checked per area. The kernel area, and from Phase 6 the
  Rust code in `/system`, allows permissive licenses only; its list is
  `kernel/deny.toml`, which CI passes to cargo-deny with `--config`. A
  unit test fails if that list ever names a copyleft license. GPL and
  LGPL are allowed only for programs shipped separately (today GRUB) and,
  later, for ports and bundle-private libraries, each with its own list;
  LGPL is never linked statically into `/system`.
- `ob image` builds the manifest. It lists every component of the image:
  the kernel's crates from `cargo tree` (normal dependencies only), the
  crates named by `-Zbuild-std` from rust-src, GRUB with the host's
  version, and microcode when present. It parses each SPDX expression,
  checks it against the area's list (the kernel list is read from
  `kernel/deny.toml`, so CI and `ob image` cannot drift), and fails on a
  missing or disallowed license.
- The image carries `/system/share/licenses/` with `dukovo.spdx` (SPDX
  2.3 tag-value), a `README` summary, `texts/` with every license and
  exception the manifest names, and `notices/<component>-<version>/` with
  the component's own license and copyright files. A crate declares extra
  notice files under `[package.metadata.dukovo] notices`. A copy of the
  document sits next to the image, and CI validates it with spdx-tools.
- License texts that are not Dukovo's own are kept verbatim from the
  SPDX license list in `image/licenses/<id>.txt`; a unit test checks that
  every allowed license has one.
- `radix-fbcon` declares `(Apache-2.0 OR MIT) AND BSD-2-Clause`, since it
  embeds the fonts.
- grub-mkrescue no longer adds its Unicode font, translations or themes,
  which the text-mode menu never used, so the image's GRUB files are
  GRUB's own GPL-3.0-or-later code.

## Consequences

- A dependency with an unknown or copyleft license cannot reach the kernel
  without a reviewed change to `kernel/deny.toml` and a failing unit test.
- The manifest is deterministic: its timestamp comes from
  `SOURCE_DATE_EPOCH` or the commit time, and its namespace from the
  commit.
- GRUB's binaries come from the build host, so the manifest names the
  host's GRUB package version and points to that distribution's source
  package. Publishing the exact GPL source next to each release, and the
  per-port SPDX declarations, come with the port recipes (Phase 8) and
  `ob release` (Phase 14).
- The ISO root keeps grub-mkrescue's `/System` directory for Apple
  firmware beside `/system`; Phase 14's own disk image removes it.
