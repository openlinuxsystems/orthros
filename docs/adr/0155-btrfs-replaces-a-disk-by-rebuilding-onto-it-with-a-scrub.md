<!--
SPDX-FileCopyrightText: 2026 The Dukovo contributors
SPDX-License-Identifier: Apache-2.0 OR MIT
-->

# 0155. Btrfs replaces a disk by rebuilding onto it with a scrub

- Status: Accepted
- Date: 2026-10-29

## Context

Roadmap 10e.1 asks for a two-disk RAID1 that survives losing either
disk and rebuilds onto a new one. ADR 0153 mounts a Btrfs with a disk
missing, and catches up a disk that comes back with a scrub. What was
left is putting a new disk in the place of the lost one.

Linux's `btrfs replace start` copies the old disk's dev extents onto
the new one, through the scrub code. Writes made meanwhile go to both
disks. The state is kept in a dev-replace item, so a replace resumes
after a reboot. At the end, the new disk takes the old one's device ID
and UUID, and the old disk's superblocks are wiped. Linux's
`btrfs device add` and `remove` change the set of disks; `remove`
moves the disk's chunks with a balance.

## Decision

- **The end is Linux's.** The new disk takes the old one's device ID
  and UUID, so the chunk tree and the device item stay as they are, and
  Linux mounts the result as its own replace leaves it. A larger new
  disk keeps the old device item's size, as Linux's does until a
  resize.
- **A new disk is a disk that missed every commit.** The replace first
  commits, so nothing the running transaction wrote is only on the old
  disks. Then the new disk takes the old one's place in the volume,
  marked behind (ADR 0153): it is read last, and written as the others
  are. A scrub then writes it every copy it should hold, from the other
  disks, and gives it its superblocks. Until then commits leave it
  without one.
- **No resume.** A replace stopped by a power cut leaves the new disk
  with no superblock, wiped at the start. The next mount finds the old
  disk missing still, as before the replace, and the replace is started
  again. Linux resumes from its dev-replace item. Dukovo writes no such
  item, so Linux sees a Dukovo replace that stopped as no replace.
- **From the other disks only.** The rebuild reads the copies on the
  other disks, even when the old disk is there. So a filesystem whose
  chunks have no copy on another disk is refused with `NotSupported`:
  one disk, or RAID0, single or DUP chunks on the disk. Linux copies
  those from the old disk while it is there.
- **The old disk is wiped.** Once the new disk holds everything, the
  old disk, if it was there, has its superblocks written over with
  zeros, as Linux's replace does. If the new disk fails during the
  rebuild, it is left out. The old disk keeps its superblock, and comes
  back at the next mount, behind.
- **Refused targets.** The kernel refuses, with `InUse`, a disk of a
  mounted Btrfs, a disk that holds a mount, and a swap area. A target
  smaller than the device item says fails with `NoSpace`, as Linux's
  does.
- **The interface.** radix-directory(2) gains `replace_disk` (devid, a
  block device's slot), which needs the control right on the directory
  and the write right on the device, and returns what the rebuild's
  scrub counted. `replace`(8) is `btrfs replace start -B`. It takes a
  devid or the old disk's device, refuses a target that holds a Btrfs
  unless given `-f`, and prints Linux's `status` line when done.

## Alternatives

- **Copy dev extents from the old disk, as Linux does.** That is
  faster when the old disk is there and healthy, and needs no other
  copy. But the replace is for a disk that is lost, or failing. The
  scrub's rebuild already checks each copy it writes against its
  checksum, and the disk that missed commits is ADR 0153's case.
- **A dev-replace item, and resume.** A rebuild that starts again is
  correct, and the scrub is as fast as a copy. Resume can come with
  `status` and `cancel`.
- **`device add` and `remove` first.** A replace needs neither. `remove`
  needs a balance to move chunks off the disk, which is its own work.

## Consequences

- A two-disk RAID1 that lost a disk is whole again after `replace
  start 2 /devices/vdc /volumes/vda`. The `btrfs-raid` boot test does
  this through the directory interface and reads every file back. Then
  Linux's `btrfs check` reads every checksum from each disk alone, the
  new one too.
- During the rebuild a RAID1 of two disks has one good copy of what
  the new disk lacks. A bad sector there is an uncorrectable error. The
  new disk then gets no superblock, and `replace` exits 1.
- `btrfs device add`, `remove`, balance and resize are not done yet.
