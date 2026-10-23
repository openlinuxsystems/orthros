<!--
SPDX-FileCopyrightText: 2026 The Dukovo contributors
SPDX-License-Identifier: Apache-2.0 OR MIT
-->

# 0078. Dukovo 1.0 is a server release, and Lumen moves to 1.1

- Status: Accepted
- Date: 2026-10-23

## Context

The plan of 2026-10-20 made 1.0 an installable system with the Lumen
compositor in its default image (ADR 0006), and put servers and
containers after 1.0. Phases 0 to 8 built a kernel, storage and POSIX
layer that a server needs as much as a desktop does. Most servers run as
virtual machines, whose devices Radix already drives. A desktop needs GPU,
Wi-Fi, audio and touchpad drivers before it is pleasant to use. A server
is judged on uptime, data safety and security. Those are the foundations
the desktop needs later anyway.

## Decision

- Dukovo 1.0 is a headless server operating system, in the spirit of
  Ubuntu Server, decided by the maintainer on 2026-10-23.
- Phases 0 to 15 keep their numbers. Some of them grow for servers:
  - Phase 9: server-grade Nukesh and the administration command set;
  - Phase 10: Btrfs RAID1 and RAID10, and disk health;
  - Phase 11: the watchdog and machine checks;
  - Phase 12: WireGuard and server NICs;
  - Phase 13: service bundles;
  - Phase 14: cloud images;
  - Phase 15: unattended security updates.
- Phase 16 becomes the server platform and the Dukovo Admin web console,
  and closes 1.0.
- Lumen moves to Phase 17, the first 1.x release, and joins the default
  image there. The 1.0 image is console-only.
- The post-1.0 phases are renumbered in the order they are planned to be
  built:

  | Old | New | Theme |
  | ---: | ---: | --- |
  | 16 | 17 | Lumen |
  | 17 | 34 | AArch64 |
  | 18 | 23 | Platform security |
  | 19 | 27 | Audio, printing and power |
  | 20 | 24 | Hardware breadth |
  | 21 | 19 | UI kit and applications |
  | 22 | 32 | Virtualization host (containers move to 20) |
  | 23 | 36 | Confidential computing |
  | 24 | 35 | Scale and real time |
  | 25 | 33 | Observability and live patching |
  | 26 | 37 | RISC-V |
  | 27 | 22 | WebAssembly app platform |
  | 28 | 26 | Personality shell |
  | 30 | 28 | Local AI assistant |
  | 31 | 21 | Developer tools |
  | 32 | 30 | Mail, Calendar and Contacts |
  | 33 | 31 | Everyday apps |

  Gaming stays Phase 29. The new phases are 18 (Go), 20 (containers and
  Podman) and 25 (server services and fleet management).
- [`docs/roadmap.md`](../roadmap.md) follows the plan document with these
  numbers.

## Consequences

- ADR 0006 is superseded. A console-only image is the default until
  Lumen ships.
- References to post-1.0 phases in code, man pages and ADRs use the new
  numbers. ADR texts keep their decisions; only the phase numbers they
  point to changed.
- Phase 9 is now extra large: Nukesh must also be a POSIX `/bin/sh` that
  passes a conformance suite and matches bash or dash on real scripts,
  and the administration commands come with it.
- Lumen design work can still start at any time, because it needs no
  code.
