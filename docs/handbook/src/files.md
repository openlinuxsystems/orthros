<!--
SPDX-FileCopyrightText: 2026 The Dukovo contributors
SPDX-License-Identifier: Apache-2.0 OR MIT
-->

# Files, the directory hierarchy and /volumes

This chapter describes files in Dukovo as of Phase 7 of the roadmap. There
is no shell yet (Nukesh comes in Phase 9), so for now files are used by
programs and checked by the boot tests; the layout and the rules below
are the ones the shell and every later program will see. The full
reference is radix-files(7); `/proc` and `/sys` are in radix-proc(5) and
radix-sysfs(5).

## The directory hierarchy

Every boot, Spark, the first program, builds the same tree:

| Path | What it holds |
| --- | --- |
| `/system` | The operating system: programs and the system's own files, read-only |
| `/system/etc`, `/system/var` | The system's settings and changing data (in memory for now) |
| `/applications` | Applications |
| `/users` | Each user's home |
| `/data` | Data shared between users |
| `/tmp` | Temporary files, in memory, gone at the next boot |
| `/run` | The state of running services, in memory |
| `/dev` | Devices: `null`, `zero`, `random`, `urandom` and one per disk and partition |
| `/proc` | The processes, the mounts, memory and uptime, as text |
| `/sys` | The kernel's devices and settings, one value per file |
| `/volumes` | Disks, USB sticks and SD cards, one directory each |

Until the root filesystem lives on a disk (Btrfs, Phase 10),
`/applications`, `/users` and `/data` are empty and in memory.

## Programs see only what they are given

Dukovo has no global root that every program can reach. A program gets
*directory capabilities*, handles to directories, and can reach only
what lies beneath them: a path that starts with `/`, or whose `..` climbs
above the directory it started from, is refused. Spark holds the root and
decides what each program gets. When bundles arrive (Phase 13), this is
how an application that asked for `documents: read-write` gets your
documents folder and nothing else.

## /volumes

At boot the kernel finds each disk and reads its partition table, MBR or
GPT. Disks and partitions get the names Linux gives them: `vda`, `vdb` for
virtual disks, `vda1` for the first partition of `vda`. Spark then mounts
the FAT filesystem (FAT12, FAT16 or FAT32) of each partition, or of each
disk with no partition table, at `/volumes/<name>`, read-write. Anything
that is not FAT is left alone.

Changes reach the disk within five seconds, or at once when a program
asks for it, so a power cut loses at most the last five seconds of
writes.

Only virtual disks have a driver in Phase 7. On the Dell, `/volumes` stays
empty: its SATA drives need the AHCI driver (Phase 10), and USB sticks
need the USB drivers (Phase 11), which also bring mounting by label,
`/volumes/<label>`, and safe eject.

## Trying it in QEMU

Make a FAT disk image with a file on it, on Linux Mint or Ubuntu:

```sh
sudo apt install dosfstools mtools
mkfs.fat -C -n MYDISK disk.img 32768
echo "Hello from the host" > hello.txt
mcopy -i disk.img hello.txt ::/hello.txt
```

Boot Dukovo with it as a virtio disk:

```sh
./ob run -- -drive file=disk.img,if=none,id=d0,format=raw -device virtio-blk-pci,drive=d0
```

The boot log shows the disk and the mount:

```text
Disk: vda is a virtio block device of 32 MiB at 0000:00:03.0, 512 bytes a block, write cache, MSI-X vector 0x50
Disk: vda has no partition table; it is used whole
Files: mounted fat from vda on /volumes/vda, labelled MYDISK
spark: mounted the FAT filesystem of vda at /volumes/vda
```

After QEMU stops, check what Dukovo wrote with the host's tools:

```sh
fsck.fat -n disk.img
mdir -i disk.img ::/
```

The `storage` boot test (`./ob test --case storage`) does all of this
with four disks: it writes files from a Dukovo program, checks them with
`fsck.fat` and mtools, writes one more from the host, reboots, and checks
that every byte reads back.
