<!--
SPDX-FileCopyrightText: 2026 The Dukovo contributors
SPDX-License-Identifier: Apache-2.0 OR MIT
-->

# 0162. USB Attached SCSI queues commands on USB 3 disks

- Status: Accepted
- Date: 2026-10-29

## Context

ADR 0161 made USB sticks disks over the Bulk-Only Transport (BOT): one
command at a time, each waiting for the last. USB 3 disks, and the
SATA and NVMe bridges in USB enclosures, also speak *USB Attached SCSI*
(UAS, protocol 62h, T10's UAS-2 and USB-IF's UASP), usually in an
alternate setting of the same interface. UAS has four pipes: commands
go out on the *command* pipe as Command IUs, each under a tag, and the
device answers on the *status* pipe with a Sense or Response IU, the
data on the *data-in* and *data-out* pipes between. At SuperSpeed each
command's status and data go on a *stream* of its own, numbered by its
tag, which the host controller keeps a ring for; below SuperSpeed there
are no streams, and the device says which command's data comes next
with a Read Ready or Write Ready IU on the status pipe.

Linux's `uas` driver takes a device's UAS setting when its host
controller has streams (at SuperSpeed) and falls back to BOT where it
has a BOT setting; otherwise the device is not used. It queues as many
commands as the device has streams, less two (32 without streams), and
for each it submits the status, then the data (with streams), then the
command, so that the device finds its buffers waiting. It sends no task
management: a command that does not end in 30 s, or a protocol error,
has the device reset (`usb_reset_device`), and every command in flight
goes again. QEMU's `usb-uas` speaks it at SuperSpeed with streams and at
high speed without, and answers REPORT LUNS for its `scsi-hd`s.

## Decision

- `radix-usb::storage::uas` runs UAS as a protocol engine that knows no
  controller: the kernel hands it each pipe transfer's end and it says
  which transfers to start or stop next (`Action`), and which commands
  ended, with their status and sense, or the residue. It finds the four
  pipes from their Pipe Usage descriptors, builds Command IUs, and
  checks each IU: a Sense IU ends its command, a Response IU other than
  INCORRECT LOGICAL UNIT NUMBER (which fails the command) breaks the
  device, a Read or Write Ready IU starts that command's data, and a
  stray IU is ignored. Its depth is Linux's: the streams less two, or
  32 without. A failed command's data is stopped. Any pipe error, an
  IU that names another command's stream, and a command that times out
  break the device, which is then reset; every command in flight ends
  so, and goes again, as on Linux. It also runs one command at a time
  as a `radix_scsi::Transport`, for the probe. It is tested against a
  simulated device with each of these faults, with and without streams,
  in both orders a device answers in, with a property test that every
  command ends once.
- `radix-xhci` gives an endpoint streams (a Stream Context Array of up
  to 256, each stream its own ring, as Linux's `xhci_alloc_streams`),
  queues a transfer on a stream and rings its doorbell, stops a stream's
  transfer, sets an interface's alternate setting (Configure Endpoint
  dropping the old setting's endpoints and adding the new ones', then
  SET_INTERFACE), and resets a device (Reset Device, then its endpoints
  configured again).
- `radix-scsi` gains REPORT LUNS, and reads the units an IU can name
  from its answer, as Linux's SCSI scan does for a UAS device.
- The kernel takes a device's UAS setting when it is at SuperSpeed on a
  controller with streams whose status and data pipes have them, or
  below SuperSpeed; otherwise its BOT setting, logging why, or, with
  none, logs it and does not use it, as Linux does. The stick's thread
  finds its units with REPORT LUNS (logical unit 0 alone if that fails),
  probes each one command at a time, and then serves the disks' requests
  many at once: each disk has as many requests in flight as the device
  has tags, shared among its disks, and each request's commands go under
  a tag of their own, with `sd`'s handling for USB as over BOT (the last
  eight blocks one a command, SYNCHRONIZE CACHE, a retry after a UNIT
  ATTENTION, a disk started before its requests go again, a wait for one
  becoming ready). Requests over UAS are as large as a transfer on a
  stream holds (`MAX_STREAM_TD_BUFFERS` pages) at any speed.
- The controller's thread starts each pipe transfer the stick's thread
  asks for, in the order asked, which without streams is the order the
  device asked for the data in; with streams it stops a stream by
  stopping the endpoint and ringing the others' doorbells again.
- A request the queue waits 30 s for, or a broken protocol, has the
  device reset as Linux's `usb_reset_device` does: its port reset (a
  root port's, or through its hub), the device addressed and configured
  again, its UAS setting and streams set up again; every command in
  flight goes again. A device that cannot be reset is taken to be gone.
- The log line of a UAS disk ends in ", USB Attached SCSI", and a
  device's disks are named with ` LUN n` after its place when it has
  more than one.

## Consequences

- USB 3 disks and enclosures that speak UAS are used, many of which
  speak nothing else, and those that speak both queue their commands as
  on Linux, which is where their speed is: a SATA or NVMe bridge reads
  ahead and reorders as the disk behind it does.
- `storage-uas` runs the storage test on four disks of three QEMU UAS
  devices, two units of one at SuperSpeed with streams and two at high
  speed without, and pulls the first out. The controller in that test
  has two SuperSpeed root ports, so that QEMU puts the other two at
  high speed. Real UAS disks are tried on the reference machines.
- No task management IUs are sent (ABORT TASK, LOGICAL UNIT RESET): as
  on Linux, a device that stops answering is reset, which costs every
  command in flight a retry. A UNIT ATTENTION that the units changed
  (REPORTED LUNS DATA HAS CHANGED) is retried, not rescanned.
- Linux's list of UAS devices that need quirks or are blocked
  (`unusual_uas.h`) is not taken; a device that needs one gets it when
  met.
