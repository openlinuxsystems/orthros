<!--
SPDX-FileCopyrightText: 2026 The Dukovo contributors
SPDX-License-Identifier: Apache-2.0 OR MIT
-->

# 0087. GPL programs may be ports in /system, each a program of its own

- Status: Accepted
- Date: 2026-10-23

## Context

The plan's userspace table ships GNU grep and sed in `/system`, and
nano later, as separate programs under the GPL. GNU diffutils and patch
join them in Phase 9 (ADR 0086), and lz4's command-line tool is under
GPL-2.0-or-later. ADR 0021 kept the GPL out of everything but GRUB
until the ports had a license list of their own.

That list, the Port area of the license manifest, allowed what the
kernel area allows plus CC0-1.0, so `ob image` refused GNU grep.

## Decision

The Port area also allows GPL-2.0-or-later and GPL-3.0-or-later, and
two permissive licenses the new ports use:
X11-distribute-modifications-variant (ncurses) and
LicenseRef-onetrueawk, Lucent's license for the One True Awk, which SPDX
has no identifier for. Their texts are in `image/licenses/`. The GPL
texts come from the SPDX license list.

The GPL is allowed in the Port area on these terms:

- a GPL port is a program of its own, which other programs only start;
  no Dukovo code and no other port links it;
- a library under the GPL is never linked into a program under another
  license, and a library under the LGPL is never linked statically into
  `/system` (ADR 0021);
- the kernel area, and Dukovo's own programs, stay permissive only;
- the manifest names each GPL port's exact source: its release archive
  and SHA-256, or its git commit. `ob release` (Phase 14) publishes those
  sources beside each release.

Where a project offers a choice, Dukovo takes the permissive license:
less is under the GPL or the Less License, a BSD-2-Clause license, and
its recipe says `GPL-3.0-or-later OR BSD-2-Clause`.

## Consequences

- `ob image` accepts grep, sed, diffutils and patch, and would still
  refuse a GPL crate in the kernel or in a Dukovo program.
- A GPL port pulls nothing else under the GPL: the C library (MIT) and
  compiler-rt (Apache-2.0 WITH LLVM-exception) it links are compatible
  with it and keep their own licenses.
- Bash, zsh, Vim and other big GPL tools still come from the catalog,
  as the plan says; this ADR is about what is already in the plan's
  `/system` lists.
