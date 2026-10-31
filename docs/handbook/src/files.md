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
| `/system/resources` | What else the programs read: manual pages, terminal descriptions, the PCI ID database, the licenses, the default settings |
| `/system/settings` | The system's settings |
| `/system/state` | The system's changing data: logs, caches |
| `/system/live` | The state of running services: sockets, locks, process IDs, in memory |
| `/applications` | Applications |
| `/users` | Each user's home |
| `/shared` | Files that users and services share |
| `/tmp` | Temporary files, in memory, gone at the next boot; each person has their own |
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
`/system/settings` is never replaced, so an update adds new settings
without undoing your edits.

## Dukovo's root on a disk

The image `./ob image` makes ends with a Dukovo root partition, the
image's fifth, holding Btrfs with data compressed with zstd. Each part
of the tree that changes is a subvolume of its own:

| Subvolume | Mounted at |
| --- | --- |
| `@system.a` | `/system`, read-only |
| `@settings` | `/system/settings` |
| `@state` | `/system/state` |
| `@applications` | `/applications` |
| `@users` | `/users` |
| `@shared` | `/shared` |
| `@snapshots` | not mounted |

When Dukovo boots from a disk it can see, a SATA, NVMe, virtio or USB
disk (a USB stick the image was written to, since Phase 11b), Spark
mounts these from it, and settings, files and everything else
written there are kept from one boot to the next. `/processes/mounts`
shows each with its disk:

```text
sda5 /system btrfs ro label=dukovo-root
sda5 /system/settings btrfs rw label=dukovo-root
```

Booted from a CD, or from a disk with no Dukovo root partition,
`/system` is the system the boot loader loaded and the rest is
in memory: each boot starts afresh from the default settings, and
`/applications`, `/users` and `/shared` start empty. An image for
optical media (`./ob image --iso`) has no Dukovo root partition.

The partition's top, with every subvolume in it, is mounted in
`/volumes` too, under the partition's name. `subvolume`(8) makes,
snapshots and removes subvolumes there, as Linux's `btrfs subvolume`
does. A snapshot holds what its subvolume held when it was made, and
shares the disk's blocks with it until either changes them, so it takes
room only for what changes since. A read-only snapshot of `/users`:

```sh
subvolume snapshot -r /users /volumes/sda5/@snapshots/users-monday
```

Phase 15's `snap` makes, lists, restores and prunes snapshots on its
own.

A copy within Btrfs is a clone too: `cp` makes the copy share the
original's blocks, as `cp --reflink=auto` does on Linux, so a copy of a
large file takes no room and no time until one of the two changes.
`cp --reflink=never` copies the bytes, and `--reflink=always` fails
where a clone cannot be made, such as onto another filesystem.

`scrub`(8) reads every block of the root, both copies of its metadata,
checks each against its checksum and mends a bad copy from the good
one, as Linux's `btrfs scrub start -B` does:

```sh
scrub start /
```

To try it, boot the image as a SATA disk. QEMU keeps what is written
only until it stops:

```sh
./ob run --media disk
```

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
the FAT filesystem (FAT12, FAT16 or FAT32) or exFAT filesystem of each
partition, or of each disk with no partition table, at
`/volumes/<name>`, read-write, or its
Btrfs filesystem: Dukovo reads and writes Btrfs disks Linux made, and
checks every block and every sector of data against its checksum. It
reads files compressed with zlib, LZO or zstd and writes them
uncompressed; a Btrfs that uses a feature Dukovo does not write yet
(such as quotas, or one made without `no-holes`) is mounted read-only. Its
ext4 filesystem mounts too, read-write, and so do ext2 and ext3: Dukovo
replays the journal Linux left, checks every checksum, and gives a file
of an ext3 block map extents when it changes it. An ext4 that uses a
feature Dukovo does not write yet (such as quotas, inline data or
encryption) is mounted read-only. NTFS, which Windows formats its disks
with, mounts read-write too: compressed and sparse files, hard links,
and Windows' and WSL's links and junctions read as Windows wrote them,
names are found whatever their case, and a file that is encrypted or
kept in the cloud fails to read rather than reading as zeros. Dukovo
writes files, directories, renames and hard links as ntfs-3g does, with
names Windows can open; compressed and sparse files, links and junctions
are not written yet. An NTFS that Windows hibernated with (Fast
Startup), left dirty or with changes in its journal to replay is
mounted read-only, since writing it would lose Windows' changes: shut
Windows down fully (hold Shift as you choose Shut down) and Dukovo
writes it. Anything else is left alone.

Changes reach a FAT, exFAT or ext4 disk within five seconds, and a Btrfs
disk within 30 seconds, as on Linux, or at once when a program asks for
it, so a power cut loses at most the last five, or 30, seconds of
writes. A Btrfs disk is whole after a power cut at any moment: it holds
either a commit or the one before. So is an ext4 disk with a journal, as
`mkfs.ext4` makes them: Dukovo writes each commit to the journal first,
and the next mount, on Dukovo or Linux, replays it.

Virtual disks, SATA disks and NVMe drives have drivers: on the Dell, its
two SSDs are `sda` and `sdb`; on the Framework Desktop, its two
NVMe drives are `nvme0n1` and `nvme1n1`. USB sticks, card readers and
USB disks are disks too, named after the SATA ones (`sdc` and on, on the
Dell), whether they speak the Bulk-Only Transport or, as USB 3 disks do,
USB Attached SCSI. Those plugged in at boot are mounted as the others
are. One plugged in later is mounted by its filesystem's label, at
`/volumes/<label>`, by volumed(8), which also unmounts the volumes of a
stick pulled out. Before pulling one out, eject it, so that what it holds
is written first:

```
# ls /volumes
DUKOVOUSB  sda5
# eject DUKOVOUSB
```

When the machine is turned off or restarted, every disk's cache is
written out first, and each NVMe drive is told to shut down, as Linux
does, so that it does not count an unsafe shutdown. A read or write a
disk fails shows in the kernel's log (dmesg(1)), with what the disk said.

Disk tools read and write the devices themselves, such as `/devices/sda`, at
any offset, with nothing cached between, and ask their size, sector size
and the rest with Linux's `ioctl` requests (radix-block(2)). A device
written by mistake loses what was on it, as on any system, so write to
one only with a tool made for it.

A disk or partition can also be swap, as on Linux: `mkswap /devices/vdb`
makes a swap area of it (one Linux made works too), `swapon
/devices/vdb` turns it on, and `swapoff` turns it off again, reading
back what was swapped out first. While it is on, programs' memory moves
out to it when memory runs short, and `free` and `swapon --show` show how
much is there; the disk itself cannot be written or mounted then.
Dukovo does not turn swap on at boot yet: the installer's swap partition
(Phase 14) brings that.

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
Files: mounted btrfs from vda on /volumes/vda, labelled MYBTRFS
spark: mounted the Btrfs filesystem of vda at /volumes/vda
```

An exFAT disk, as SD cards and large USB sticks come formatted, works
the same way too. No tool on Linux copies a file onto one without
mounting it, so make it on the host with `sudo mount`, or let Dukovo
write it:

```sh
sudo apt install exfatprogs
truncate -s 64M exfat.img
mkfs.exfat -L MYEXFAT exfat.img
./ob run -- -drive file=exfat.img,if=none,id=d0,format=raw -device virtio-blk-pci,drive=d0
```

```text
Files: mounted exfat from vda on /volumes/vda, labelled MYEXFAT
spark: mounted the exFAT filesystem of vda at /volumes/vda
```

After QEMU stops, `fsck.exfat -n exfat.img` checks what Dukovo wrote.
The `exfat-write` boot test writes two such disks, checks them with
`fsck.exfat`, reads them back on the host with The Sleuth Kit and after a
reboot. A volume with two tables (TexFAT) mounts read-only.

The `btrfs` boot test (`./ob test --case btrfs`) reads every file of two
such disks and checks each byte on the host; `btrfs-write` writes two
such disks, then checks them with Linux's `btrfs check` and reads them
back on the host and after a reboot, `btrfs-power-cut` cuts the
power while a disk is being written, `btrfs-log` mounts a disk Linux
left with changes in its log of fsyncs alone, which the mount replays as
Linux does, and `btrfs-raid` writes a RAID1 of two disks, then pulls one
out, reads everything back from the other, and puts a third disk in its
place with `replace`; `btrfs-raid-pull` fails a disk at a random write
while it is written, then brings it back and scrubs; and `btrfs-stress`,
the long stress run, has four workers make random changes to two disks at
once for two minutes, then checks the disks with `btrfs check` and every
file after a reboot. Dukovo writes the log of
fsyncs too: an `fsync` of a file writes only what changed of it, as
Linux does, and Linux replays Dukovo's log after a power cut.

A Btrfs of several disks mounts from any of them, once, at
`/volumes/<first disk>`. With a disk missing it still mounts if the
others hold a copy of all of it, and the kernel log warns:

```text
Btrfs: vda: 1 of its 2 disks missing; mounted from the copies on the others. Replace the missing, or a disk more lost may lose data
```

A disk that fails while the filesystem is mounted is left out, and the
log says so within seconds. When it comes back, the mount warns that it
missed changes; it is read only after the others until `scrub start
/volumes/vda` catches it up.

`replace`(8) puts a new disk in the place of a missing one, or of one
that is failing, and rebuilds onto it from the copies on the others, as
Linux's `btrfs replace start -B` does. A missing disk is named by its
device ID, 2 here for the second disk:

```sh
replace start 2 /devices/vdc /volumes/vda
```

`device`(8) adds a disk to the filesystem or takes one out, and
`balance`(8) moves its chunks, as Linux's `btrfs device` and `btrfs
balance` do. A filesystem of one disk becomes a RAID1 of two with:

```sh
device add /devices/vdb /volumes/vda
balance start -dconvert=raid1 -mconvert=raid1 /volumes/vda
```

A disk is taken out, its chunks moved to the others first, with
`device remove 2 /volumes/vda`. Unlike Linux, a balance gives each
snapshot its own copy of the blocks it moves (ADR 0159).

`quota`(8) keeps Btrfs's simple quotas, as Linux's `btrfs quota enable
--simple` does (ADR 0170): each subvolume's quota group counts what it
wrote, and a limit stops writes past it with `EDQUOT`:

```sh
quota enable /volumes/vda
quota limit 10g 0/256 /volumes/vda
quota show -r /volumes/vda
```

An ext4 disk, the kind most Linux machines use, is made the same way:

```sh
sudo apt install e2fsprogs
mkdir files && echo "Hello from the host" > files/hello.txt
truncate -s 64M ext4.img
mkfs.ext4 -L MYEXT4 -d files ext4.img
./ob run -- -drive file=ext4.img,if=none,id=d0,format=raw -device virtio-blk-pci,drive=d0
```

```text
Files: mounted ext4 from vda on /volumes/vda, labelled MYEXT4
spark: mounted the ext4 filesystem of vda at /volumes/vda
```

After QEMU stops, `e2fsck -fn ext4.img` checks what Dukovo wrote, and
`debugfs -R "ls -l /" ext4.img` lists it. The `ext4-write` boot test
writes two such disks, checks them with `e2fsck`, reads them back on the
host with `debugfs` and after a reboot, and `ext4-power-cut` cuts the
power while a disk is being written, then checks that `e2fsck`'s replay
of the journal leaves a whole volume.

An NTFS disk is made with ntfs-3g's tools, which copy files in without
mounting it:

```sh
sudo apt install ntfs-3g
truncate -s 64M ntfs.img
mkntfs -q -F -Q -L MYNTFS ntfs.img
echo "Hello from the host" > hello.txt && ntfscp ntfs.img hello.txt /hello.txt
./ob run -- -drive file=ntfs.img,if=none,id=d0,format=raw -device virtio-blk-pci,drive=d0
```

```text
Files: mounted ntfs from vda on /volumes/vda, labelled MYNTFS
spark: mounted the NTFS filesystem of vda at /volumes/vda
```

The `ntfs` boot test reads two such disks back, one of them filled
through ntfs-3g with links, junctions, hard links and compressed and
sparse files. `ntfs-write` writes two, and checks them on the host with
ntfs-3g's `ntfsresize --info` and `ntfsfix -n` and reads them back with
The Sleuth Kit and after a reboot.
