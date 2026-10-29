<!--
SPDX-FileCopyrightText: 2026 The Dukovo contributors
SPDX-License-Identifier: Apache-2.0 OR MIT
-->

# 0163. volumed mounts the volumes plugged in after boot by their labels

- Status: Accepted
- Date: 2026-10-29

## Context

Phase 11b's last part. Since ADR 0161, Spark mounts each disk found at
boot at `/volumes/<name>`, USB sticks among them, but a stick plugged in
later only appears in `/devices`, and one pulled out leaves its
filesystem mounted, its requests failing. The roadmap's 11b asks for
`volumed`, which mounts inserted volumes at `/volumes/<label>`, grants
access to the logged-in user, and ejects safely; the Phase 11 decisions
say it grants access to the console's owner until Dukovo has users.

On Linux, udev tells udisks2 of each disk that comes or goes, and udisks
mounts removable media at `/media/<user>/<label>`, or at the UUID when
there is no label, appending a number to a label taken. Radix has no
event for a disk that comes or goes yet; `/kernel/block` lists them.

## Decision

- **volumed(8)** is a Spark service, `volumed.service`, run from
  systools. It takes the disks there are when it starts as done, since
  Spark has mounted them, and looks at `/kernel/block` every second.
- A disk or partition that comes, and is a volume (a partition, or a
  disk with none, and no EFI system partition), is mounted at
  `/volumes/<label>`, its filesystem's label as blkid(8) reads it; at
  `/volumes/<name>` when it has none, as Spark does at boot; at
  `/volumes/<label>-<name>` when another volume has the label. A label's
  slashes and control characters become underscores. The filesystems are
  tried in Spark's order, FAT, exFAT, Btrfs and ext4, read-only where the
  disk or Radix cannot write; Btrfs is not compressed.
- A disk or partition that goes has whatever is mounted from it at
  `/volumes`, Spark's mounts at boot among them, unmounted, and the
  directory removed. Nothing outside `/volumes` is touched.
- **eject(1)** unmounts a volume after its filesystem writes out what it
  holds, and removes its directory, as util-linux's eject does with a
  mount point. It takes a label, a path under `/volumes`, or a disk or
  partition, whose name unmounts each of its partitions'. An ejected
  volume's disk stays in `/devices`, and volumed leaves it alone until it
  is pulled out and plugged in again.
- `volumed --list` prints the volumes mounted at `/volumes`.
- Access: until Dukovo has users, every program reaches `/volumes` with
  the root's rights, as it reaches the rest of the root. When users come,
  volumed gives the user logged in at the console each volume it mounts.
- Each change is a line on volumed's output, which Spark puts in the
  kernel log.

## Consequences

- A stick plugged in after boot is a volume at once, by its label, and
  one pulled out no longer leaves a dead mount behind. `volumes`, a new
  boot test, pulls out a stick Spark mounted at boot, checks it is
  unmounted, plugs it in again, has a program find it at its label,
  write a file and eject it, and reads the file back on the host.
- Spark starts three services on a normal boot: consoled, volumed and
  diskhealth.
- Looking every second costs a directory listing a second and a wakeup;
  an event for a disk that comes or goes, when the kernel has one,
  replaces it.
- Ejecting does not yet stop the disk, as Linux's eject does with a SCSI
  START STOP UNIT, or power its USB port off: pulling it out after eject
  is safe, since nothing is left to write.
- Volumes are not mounted by label at boot: Spark keeps `/volumes/<name>`
  for the disks it finds then, which the storage tests and Dukovo's root
  rely on.
