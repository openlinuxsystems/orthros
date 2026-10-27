<!--
SPDX-FileCopyrightText: 2026 The Dukovo contributors
SPDX-License-Identifier: Apache-2.0 OR MIT
-->

# Files, the directory hierarchy and /volumes

This chapter describes files in Dukovo as of Phase 10 of the roadmap:
the layout and the rules that the shell and every program see. The full
reference is radix-files(7); `/processes` and `/kernel` are in
radix-proc(5) and radix-sysfs(5).

## The directory hierarchy

Every boot, Spark, the first program, builds the same tree. Each folder
is named for what it holds:

| Path | What it holds |
| --- | --- |
| `/system` | The operating system: programs and the system's own files, read-only |
| `/system/commands` | The programs, where the shell finds commands |
| `/system/libraries` | The libraries, the dynamic linker and the locales |
| `/system/resources` | What else the programs read: manual pages, terminal descriptions, the licenses, the default settings |
| `/system/settings` | The system's settings (in memory for now) |
| `/system/state` | The system's changing data: logs, caches (in memory for now) |
| `/system/live` | The state of running services: sockets, locks, process IDs, in memory |
| `/applications` | Applications |
| `/users` | Each user's home |
| `/shared` | Files that users and services share |
| `/tmp` | Temporary files, in memory, gone at the next boot |
| `/devices` | Devices: `null`, `zero`, `random`, `urandom`, one per disk and partition, and the terminals (`ptmx`, `tty`, `pts/`) |
| `/processes` | The processes, the mounts, memory and uptime, as text |
| `/kernel` | The kernel's view of the hardware and its settings, one value per file |
| `/volumes` | Disks, USB sticks and SD cards, one directory each |

## The Unix names

Unix programs and scripts look for `/bin/sh`, `/usr/bin/env`,
`/dev/null`, `/proc`, `/etc` and `/var/log`, and so do people who know
Unix. Each of those names is a symbolic link in the root to the folder
that holds what it stands for:

| Unix name | Leads to |
| --- | --- |
| `/bin`, `/sbin` | `/system/commands` |
| `/usr` | `/system` |
| `/system/bin`, `/system/lib`, `/system/share` | `/system/commands`, `/system/libraries`, `/system/resources` |
| `/etc` | `/system/settings` |
| `/var` | `/system/state` (and `/var/run` to `/system/live`) |
| `/run` | `/system/live` |
| `/dev` | `/devices` |
| `/proc` | `/processes` |
| `/sys` | `/kernel` |

Everything is still kept in one place, so `/etc/nanorc` and
`/system/settings/nanorc` are the same file. The links are hidden: the
root's `.hidden` file lists them (and `/system/.hidden` those in
`/system`), and `ls /` and the shell's Tab completion leave them out,
as the file manager will. `ls -a /` shows them, and Tab completes one
once its whole name is typed.

The system ships its default settings in
`/system/resources/default-settings`, and Spark copies each one that
`/system/settings` lacks into it at boot, so `/system/settings/nanorc`
is there to edit. A setting already in
`/system/settings` is never replaced. While `/system/settings` is in
memory, each boot starts it afresh from the defaults.

Until the root filesystem lives on a disk (Btrfs, Phase 10),
`/applications`, `/users` and `/shared` are empty and in memory.

## Programs see only what they are given

Dukovo has no global root that every program can reach. A program gets
*directory capabilities*, handles to directories, and can reach only
what lies beneath them: a path that starts with `/`, or whose `..` climbs
above the directory it started from, is refused. A symbolic link cannot
lead out either: one whose target starts with `/` is followed from the
directory the program started from, as if that were the root. Spark
holds the root and decides what each program gets. When bundles arrive
(Phase 13), this is how an application that asked for
`documents: read-write` gets your documents folder and nothing else.

## /volumes

At boot the kernel finds each disk and reads its partition table, MBR or
GPT. Disks and partitions get the names Linux gives them: `vda`, `vdb` for
virtual disks, `sda`, `sdb` for SATA disks, `nvme0n1`, `nvme1n1` for NVMe
drives, `vda1` for the first partition of `vda` and `nvme0n1p1` for the
first of `nvme0n1`. Spark then mounts
the FAT filesystem (FAT12, FAT16 or FAT32) of each partition, or of each
disk with no partition table, at `/volumes/<name>`, read-write, or its
Btrfs filesystem, read-only for now: Dukovo reads Btrfs disks Linux made,
with their subvolumes, and checks every block and every sector of data
against its checksum, and reads files compressed with zlib, LZO or zstd,
but writes them only from Phase 10c. Anything else is left alone.

Changes reach the disk within five seconds, or at once when a program
asks for it, so a power cut loses at most the last five seconds of
writes.

Virtual disks, SATA disks and NVMe drives have drivers: on the Dell, its
two SSDs are `sda` and `sdb`; on the Framework Desktop, its two
NVMe drives are `nvme0n1` and `nvme1n1`. USB sticks need the USB drivers
(Phase 11), which also bring mounting by label, `/volumes/<label>`, and
safe eject.

When the machine is turned off or restarted, every disk's cache is
written out first, and each NVMe drive is told to shut down, as Linux
does, so that it does not count an unsafe shutdown. A read or write a
disk fails shows in the kernel's log (dmesg(1)), with what the disk said.

Disk tools read and write the devices themselves, such as `/devices/sda`, at
any offset, with nothing cached between, and ask their size, sector size
and the rest with Linux's `ioctl` requests (radix-block(2)). A device
written by mistake loses what was on it, as on any system, so write to
one only with a tool made for it.

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

or as an NVMe drive, `/volumes/nvme0n1`:

```sh
./ob run -- -drive file=disk.img,if=none,id=d0,format=raw -device nvme,serial=test,drive=d0
```

The boot log shows the disk and the mount, and the shell on the screen
finds the file there (`ls /volumes/vda`, `cat /volumes/vda/hello.txt`):

```text
Disk: vda is a virtio block device of 32 MiB at 0000:00:03.0, 512 bytes a block, write cache, discards, MSI-X vector 0x50
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

A Btrfs disk works the same way:

```sh
sudo apt install btrfs-progs
mkdir files && echo "Hello from the host" > files/hello.txt
truncate -s 128M btrfs.img
mkfs.btrfs -L MYBTRFS --rootdir files btrfs.img
./ob run -- -drive file=btrfs.img,if=none,id=d0,format=raw -device virtio-blk-pci,drive=d0
```

```text
Files: mounted btrfs from vda on /volumes/vda, read-only, labelled MYBTRFS
spark: mounted the Btrfs filesystem of vda at /volumes/vda, read-only
```

The `btrfs` boot test (`./ob test --case btrfs`) reads every file of two
such disks and checks each byte on the host.
