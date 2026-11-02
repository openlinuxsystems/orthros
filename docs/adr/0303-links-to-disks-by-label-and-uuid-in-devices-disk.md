<!--
SPDX-FileCopyrightText: 2026 The Dukovo contributors
SPDX-License-Identifier: Apache-2.0 OR MIT
-->

# 0303. Links to disks by label and UUID in /devices/disk

- Status: Accepted
- Date: 2026-11-02

## Context

Phase 14a. Linux's udev keeps `/dev/disk/by-uuid`, `by-label`,
`by-partuuid` and `by-partlabel`, links to each disk and partition by
what is on it, which scripts and settings name disks by, since a disk's
name (`sda`) changes with the order disks are found in. The installer
names Dukovo's root and EFI system partition that way. ADR 0057 names
Radix's disks as Linux does, so the links should be where Linux's are.

- `/devices` is the kernel's tree, which programs write nothing in.
- Finding a filesystem's label and UUID is blkid(8)'s work, in
  systools (`probe.rs`); the kernel has no use for it.

## Decision

- **`/devices/disk` is a link** the kernel puts in its tree, to
  `/system/live/disk`, memory that goes at shutdown. The links are
  there, in `by-uuid`, `by-label`, `by-partuuid` and `by-partlabel`,
  each leading to its device: `/devices/vda1`. A name is written as
  udev writes it, `\x20` for a space and `\x2f` for a slash; where two
  devices share one, the first in the kernel's order has it.
- **Who keeps them**: volumed(8), at its start and whenever disks come
  or go; rdisk(8) once the kernel has read a new table; the mkfs tools,
  mkswap(8) and fsck-fat(8) once they have written. Its service file
  gives volumed `/system/live/disk`.

## Consequences

- `/dev/disk/by-uuid/...` works as on Linux, through `/dev` and
  `/devices/disk`.
- A filesystem changed by a program other than these (a label changed
  by hand) is followed at the next change of disks.
- The links are absolute, where udev's are relative (`../../sda1`):
  both lead to the device.
- The roadmap's `/devices/disk0` and `/devices/disk0p1` are not made:
  disks keep the Linux names ADR 0057 gives them, and the links name
  them by what they hold.
