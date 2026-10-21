<!--
SPDX-FileCopyrightText: 2026 The OrthrOS contributors
SPDX-License-Identifier: Apache-2.0 OR MIT
-->

# 0014. Early microcode comes from a boot module added at image build time

- Status: Accepted
- Date: 2026-10-20

## Context

Phase 1 must load CPU microcode early "when the optional firmware package
is installed". ADR 0007 keeps closed firmware out of the base system, in a
separate optional package. Until the package system exists (Phase 14 and
later), there is no installed system to take the package from: Phase 1
images are built by `ob` from the source tree. Linux solves the same
problem with a CPIO archive prepended to the initramfs, which Radix does not
have.

## Decision

The microcode file is a separate GRUB module. `ob image` and `ob run` add
one when given `--microcode PATH` or `image.microcode` in `orthros.toml`:
a file or directory of Intel microcode updates, such as
`/lib/firmware/intel-ucode` from the build host's `intel-microcode`
package. `ob` validates every update (header, size, checksums) and
concatenates the valid ones into `/boot/firmware/intel-ucode.bin`, which
`grub.cfg` loads with `module2 ... microcode`. The kernel recognises the
module by that word, reserves it, and on every CPU, before anything else of
note, writes the newest matching update if it is newer than the running
revision. The parser lives in `kernel/lib/microcode`, without `unsafe`.

Only Intel's format is supported; AMD's comes with AMD hardware in testing.

## Consequences

- Default images contain no closed firmware, as ADR 0007 requires; a
  builder opts in explicitly, with firmware from their own distribution.
- The same module mechanism will carry the optional firmware package's
  microcode once OrthrOS installs to disk: the installer or `sysupdate`
  adds the module line when the package is installed.
- QEMU ignores microcode writes, so the boot test checks matching and
  validation only; applying an update is confirmed on the reference laptop.
- Revisit if GRUB is replaced, or when the package system defines where
  firmware lives on an installed system.
