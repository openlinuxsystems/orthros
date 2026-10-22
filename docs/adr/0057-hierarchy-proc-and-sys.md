<!--
SPDX-FileCopyrightText: 2026 The Dukovo contributors
SPDX-License-Identifier: Apache-2.0 OR MIT
-->

# 0057. Spark builds the directory hierarchy; /proc and /sys are plain text, with Linux's layout where both have a thing

- Status: Accepted
- Date: 2026-10-22

## Context

The plan's spec 7.1 lays out Dukovo's tree: `/system` (read-only, from
the image), `/applications`, `/users`, `/data`, `/tmp`, `/run`, `/dev`,
`/proc`, `/sys` and `/volumes`. Someone has to build it at boot, and the
kernel's own trees (`/proc`, `/sys`, `/dev`) need a format. Linux's
`/proc` mixes formats, and its `/sys` holds one value per file in a
layout that tools such as `lspci`, `lsblk` and udev depend on. Plan 9 and
Linux both show that plain text read with ordinary file calls is enough
for most kernel state.

## Decision

- The kernel makes only an empty tmpfs root. Spark (spark(8)) builds the
  hierarchy on it and mounts: the boot archive read-only on `/system`,
  tmpfs on `/system/etc`, `/system/var`, `/tmp` and `/run`, and the
  `dev`, `proc` and `sys` trees. Each disk's FAT filesystem is mounted at
  `/volumes/<device>` (`/volumes/vda1`, or `/volumes/vdb` for a disk with
  no partitions), read-write, or read-only when the disk is.
- Disks and partitions are named as Linux names them (`vda`, `vda1`; and
  later `sda`, `nvme0n1p1`), so that what a user reads in Linux guides
  applies.
- `/proc` is text of `key: value` lines, generated when a file is opened
  so that a reader sees one snapshot (radix-proc(5)): `/proc/<n>/status`
  per process, `/proc/mounts`, `/proc/meminfo`, `/proc/uptime`.
- `/sys` holds one value per file, ending in a newline, in Linux's layout
  wherever both have a thing (radix-sysfs(5)): `/sys/kernel/version`,
  `/sys/block/<name>/size` and `ro`, `/sys/bus/pci/devices/<address>/`
  `vendor`, `device` and `class`. It is read-only.
- `/dev` is one fixed tree: `null`, `zero`, `random`, `urandom` and a
  block device per disk and partition.

## Consequences

- Policy (what goes where) lives in Spark, not the kernel, and changes
  with it; the kernel's trees are small and generated.
- Ported tools find `/sys` where they look, and `/proc` is easy to read
  from any language. Formats that are not Linux's are documented in
  section 5 and checked by the boot tests.
- Mounting disks by device name at boot is a Phase 7 stand-in: Phase
  11's `volumed` mounts inserted volumes at `/volumes/<label>` and
  ejects them safely.
