<!--
SPDX-FileCopyrightText: 2026 The Dukovo contributors
SPDX-License-Identifier: Apache-2.0 OR MIT
-->

# 0161. USB sticks are disks over the Bulk-Only Transport, served by a thread each

- Status: Accepted
- Date: 2026-10-29

## Context

Phase 11b makes USB sticks, card readers and USB disks usable (roadmap
11b; the Phase 11 exit has a FAT or exFAT stick appear under
`/volumes`). Such a device has a *mass storage* interface (class 8)
that carries SCSI commands (subclass 6), in one of two ways: the
*Bulk-Only Transport* (BOT, protocol 50h, 1999), which every stick
speaks, one command at a time, or *USB Attached SCSI* (UAS, protocol
62h), which USB 3 disks add, with commands queued on four pipes. QEMU's
`usb-storage`, the device every boot test boots from, is Bulk-Only.

Linux drives BOT with `usb-storage` and the SCSI disk driver `sd` on
top: a kernel thread for each interface sends one command at a time and
waits for it. Its years with real sticks are in what it bears with: a
status block with the wrong signature, a residue that is wrong, a stick
that fails to read its last blocks with others, or that dies when asked
for a mode page it lacks. The kernel's other disks (virtio, AHCI, NVMe)
share one request queue (`fs::disk::queue`, ADR 0156); only the
controller's thread drives an xHCI controller (ADRs 0146 and 0147).

## Decision

- `radix-usb::storage` runs the Bulk-Only Transport over pipes the host
  controller's driver gives it: the Command Block Wrapper, the data, the
  Command Status Wrapper, REQUEST SENSE after a failed command, a stalled
  pipe's halt cleared, and a reset (Bulk-Only Mass Storage Reset, then
  both halts cleared) after anything the specification does not allow.
  As Linux does without being told of the stick, it takes a status
  signature that is not "USBS" if every status has the same one, and it
  stops believing the residue of a stick that reported one for an
  INQUIRY or READ CAPACITY that moved all its data. It is tested against
  a simulated stick, with each fault.
- A new library, `radix-scsi`, has the commands, sense data and what
  they mean, and the probe Linux's SCSI scan and `sd` do: INQUIRY, TEST
  UNIT READY (with START STOP UNIT once if the unit asks, and up to
  100 s for one becoming ready), READ CAPACITY(10), and (16) past 32
  bits, and MODE SENSE(6) of every page, 192 bytes as `usb-storage` asks,
  then of page 0: the write-protect bit, and the write cache from the
  caching page if the answer holds it. A device that gives no caching
  page is taken to write through, as on Linux, which asks USB devices
  for no page alone.
- `radix-xhci` takes a transfer of many buffers as one chained TD, up to
  `MAX_TD_BUFFERS`, and clears a halt as Linux's `xhci_endpoint_reset`
  does: the endpoint stopped, dropped and added again by Configure
  Endpoint, so that both sides start the pipe over whatever state it was
  in, then CLEAR_FEATURE(ENDPOINT_HALT) to the device.
- The kernel starts a thread, `usb-storage`, for the first mass storage
  interface of each device, as Linux has one. It asks the controller's
  thread for each stage through a mailbox (a bulk transfer straight to
  or from the request's pages, a halt cleared, a control transfer, or a
  transfer stopped) and waits for its answer. It asks for the number of
  logical units (Get Max LUN), probes each, and gives each disk the
  lowest `sd` letter free, the one IDA hands out on Linux, after the
  SATA disks the PCI scan found; at boot, sticks take letters in the
  order the controllers found them, so names are the same each boot.
  Each disk and its partitions go in `/devices` and `/kernel/block`;
  the thread reads the partition table itself, since it serves the disk.
- A stick's disk goes through the shared queue, one request at a time:
  READ(10) and WRITE(10), or (16) past 32 bits; the last eight blocks
  one a command, as `sd` does for USB (card readers fail to read the
  last block with others); SYNCHRONIZE CACHE for a flush if it has a
  write cache. A UNIT ATTENTION, an abort, a hardware error or a broken
  transport has the command go again; a disk found stopped is started
  first. Requests are 120 KiB at most below SuperSpeed and 512 KiB at
  it, as Linux's are. A request the queue waits 30 s for has its
  transfer stopped, the interface reset, and goes again.
- The boot waits up to 5 s, before Spark starts, for each controller to
  look at its ports and hubs and for each stick found to name its disks,
  so that Spark sees the sticks plugged in at boot, the boot stick among
  them.
- A stick unplugged fails every request in flight and after; its disks
  and partitions leave `/devices` and `/kernel/block`, their letters are
  free again, and its thread ends. A filesystem mounted from it stays
  mounted, and its requests fail.
- `/kernel/block/<disk>/device` has a USB disk's model (its vendor and
  product), its revision as `firmware_rev`, and its device's serial
  number string as `serial`, which udev gives as `ID_SERIAL_SHORT`; a
  disk's serial is now kept up to 64 bytes. Each partition gains
  `parttype`, its type as lsblk and blkid give it (`0xef`, or the GPT
  type's GUID), which Linux has only in udev.
- Spark leaves EFI system partitions alone, as Windows, macOS and
  udisks do: what boots the machine is not one of the user's volumes.
- A device that speaks UAS alone is logged and not used until UAS comes;
  one that speaks both is driven over BOT.

## Consequences

- The boot stick is a disk in every boot test that boots from USB:
  `sda`, or the letter after the SATA disks, with the image's four
  partitions, none of which Spark mounts (its EFI system partition is
  left alone, and the others hold ISO 9660 and HFS+). `storage-usb` runs
  the storage test on four sticks, two at SuperSpeed and two at full
  speed behind a hub, and pulls one out and plugs it in again; `root`
  sees its image's EFI system partition left alone. Real sticks are
  tried on the reference machines.
- A machine booted from a stick that `./ob image`'s image was written to
  now finds Dukovo's root on the stick's fifth partition, as ADR 0137
  foresaw, and keeps its settings and files there from one boot to the
  next, instead of starting afresh in memory. Should an internal disk
  hold a Dukovo root too, the first Spark finds is used, as with two
  internal ones; `parttype` lets it tell the root by its partition type
  when that is needed.
- An EFI system partition is no longer mounted at `/volumes` on any
  disk. The installer and updates, which write it, mount it themselves.
- A stick plugged in after boot appears in `/devices`, but nothing mounts
  it until `volumed` (11b), which mounts by label and ejects.
- One command at a time is as fast as BOT goes; UAS, next in 11b, queues
  commands on USB 3 disks. A stick's thread and the controller's hand
  each stage over, which costs a wakeup or two a command, against a
  stick's milliseconds.
- Linux's table of quirks for named sticks (`unusual_devs.h`) is not
  taken: the tolerances above cover what it most often fixes, and a
  stick that needs more gets its entry when one is met.
