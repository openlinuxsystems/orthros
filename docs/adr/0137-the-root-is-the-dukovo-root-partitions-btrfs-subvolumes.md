<!--
SPDX-FileCopyrightText: 2026 The Dukovo contributors
SPDX-License-Identifier: Apache-2.0 OR MIT
-->

# 0137. The root is the Btrfs subvolumes of a Dukovo root partition

- Status: Accepted
- Date: 2026-10-28

## Context

Phase 10 is done when the root filesystem is Btrfs. Until now Spark
built the hierarchy in memory: `/system` was the boot archive, read-only,
and `/system/settings`, `/system/state` and `/system/live` were tmpfs, so
every boot started from the defaults and nothing written outside
`/volumes` lasted. The roadmap's guided disk layout puts everything but
the EFI system partition and swap on a Btrfs "Dukovo root" partition,
in subvolumes: `@system.a` and `@system.b` (read-only, the update
slots), `@settings`, `@state`, `@applications`, `@users`, `@shared` and
`@snapshots`. The installer that makes that layout comes in Phase 14,
and the update slots in Phase 13.

## Decision

- **The partition.** `./ob image` puts a Dukovo root partition after
  the four of the image grub-mkrescue makes, from the next whole MiB, as
  GPT entry 5, named `Dukovo root`. Its type GUID, which the installer
  and `rdisk` use too, is minted here:
  `90C9A303-3954-4174-8F8B-EE75A92A9EB0`. Both GPT headers, the backup
  entries at the new end and the protective MBR's size are written
  again, with their CRC-32s. An `--iso` image, for optical media, has
  none.
- **What it holds.** 512 MiB of Btrfs, made by the pinned `mkfs.btrfs`
  with data compressed with zstd, labelled `dukovo-root`: `@system.a`
  holds the boot archive's files and is read-only; `@settings`,
  `@state`, `@applications`, `@users`, `@shared` and `@snapshots` start
  empty. `@system.b` comes with updates.
- **Spark.** After mounting the disks' filesystems at `/volumes`, the
  first Btrfs mounted to write whose top holds the subvolumes Spark
  mounts is Dukovo's root. Mounted without compression, it is mounted
  again with it (ADR 0134: the root only, as Fedora does). Spark then
  mounts `@system.a` read-only on `/system`, `@settings` on
  `/system/settings`, `@state` on `/system/state`, `@applications` on
  `/applications`, `@users` on `/users` and `@shared` on `/shared`, and
  tmpfs on `/system/live`. With no such disk, or one that refuses
  writes, `/system` is the boot archive and memory, as before. Default
  settings are copied only where none is yet, so a second boot adds
  none.
- **Bind mounts.** A subvolume is mounted as a directory of the mount
  it is in: the directory interface's `mount` takes the kind `bind`,
  with the directory's capability as its source, and mounts that
  directory at the path, read-only if asked or if its own mount is. The
  disk keeps one filesystem, mounted once.
- **Read-only subvolumes.** A subvolume whose root item has Linux's
  read-only flag (`BTRFS_ROOT_SUBVOL_RDONLY`) refuses every change with
  `EROFS`, as on Linux, though its filesystem is mounted to write.
- **What is not yet.**
  - `/` stays Spark's directory in memory: it holds only the
    directories the rest is mounted on, as `@root` would.
  - The kernel and Spark start from the boot archive, which keeps every
    file: Radix does not see USB disks until Phase 11b, so a machine
    booted from a USB stick has its system there and in memory.
  - Spark knows the root by its subvolumes, not by its partition's type
    GUID, which `/devices` does not show yet.

## Alternatives

- **A disk image of its own for the root.** `./ob image` would make two
  files to write to two places. One image written to a disk boots and
  has its root.
- **Mount each subvolume as a filesystem of its own,** as Linux's
  `subvol=` option looks. Radix mounts a filesystem on a block device,
  and two mounts of one disk would be two filesystems writing it at
  once. Linux shares one superblock between them too.
- **Make `/` the Btrfs top and switch the kernel's root to it.** The
  hierarchy's virtual filesystems and `/volumes` would have to move
  over, and the boot archive could not be the fallback.
- **Load the system from the disk alone.** GRUB and the kernel would
  have to read Btrfs before Spark starts, and Phase 13 keeps the
  kernels and boot archives of both slots on the EFI partition.

## Consequences

- An image written to a disk Radix sees (SATA, NVMe or virtio) keeps
  its settings, state, applications, users' files and shared files
  across boots, and runs programs from Btrfs.
- The image grows by 512 MiB, most of it free space for what is
  written.
- The `root` boot test boots twice with a copy of the test image and
  its partition as a virtio disk. Linux's `btrfs check` checks the
  partition after each boot.
