<!--
SPDX-FileCopyrightText: 2026 The Dukovo contributors
SPDX-License-Identifier: Apache-2.0 OR MIT
-->

# 0111. Every disk's requests go through one queue, and block devices are read and written raw

- Status: Accepted; an NVMe disk has a lane for each CPU, and failed requests are logged, since [0115](0115-nvme-from-the-specification.md)
- Date: 2026-10-25

## Context

Phase 7 gave the kernel one kind of disk, virtio-blk, and the queue its
requests waited in was part of the virtio driver: request slots, the
I/O classes of ADR 0093, waiting threads woken by the interrupt. Phase
10 adds AHCI and NVMe. Each needs the same queue, and both add what
virtio never needed: a controller that an error stops until it is
restarted, a disk that stops answering, commands that the disk takes
only with nothing else in flight (an ATA flush or TRIM), and writes
that are durable when they finish (forced unit access). Btrfs (10b and
10c) will need those writes for its superblocks, and discards to give
freed space back.

Block devices in `/dev` could only be mounted: reads and writes of them
failed with EINVAL. Disk tools, `dd`, partitioners and mkfs programs
read and write them raw, and ask their size and sector size with
Linux's `ioctl` requests.

## Decision

- `radix_block::BlockDevice` gains `write_through`, a write that is
  durable when it returns, and `discard`, which tells the device that
  sectors hold nothing worth keeping. The defaults write and then flush,
  and fail with `Unsupported`. `Info` says how many sectors one discard
  covers (0 for none) and whether the disk spins. Partitions pass both
  on, moved by their first sector. virtio-blk discards when the device
  offers it (QEMU does).
- One queue, `fs/disk/queue.rs`, serves every disk. A driver is a
  `Controller`: it says whether a request finds room and whether it
  goes out alone, starts requests as numbered slots, hands back finished
  slots, and may be stopped, recovered and reset. Room is taken by
  I/O class and level as ADR 0093 has it. A request that goes out alone
  and is waiting holds back the requests of its rank and below that
  come after it, so the disk empties for it; more urgent ones still
  pass.
- A request the device fails ends in EIO. One it hands back to go again
  (another command's error stopped it, or the link was reset) goes
  again, five times at most, as Linux's SCSI layer retries.
- A controller an error stopped is recovered by a thread that waits on
  it: the thread takes the controller out of the queue's lock, so that
  it can sleep while the controller restarts, and puts it back. The
  interrupt handler leaves the disk alone meanwhile. A request
  unanswered for 30 seconds, as long as Linux gives ATA and SCSI disks,
  has its thread reset the controller the same way. virtio has no reset
  and its requests wait on, with a line in the log every 30 seconds.
- A block device node reads and writes raw, straight to the disk, with
  nothing cached: as FreeBSD's disks do, and as Linux's do when opened
  with `O_DIRECT`. Any offset and length work: a write of part of a
  sector reads the sector first, under the node's lock. Reads stop at
  the end; writes there fail with ENOSPC, as Linux's do. `fsync` empties
  the disk's write cache.
- The whole sectors of a transfer on a sector's boundary, into or from
  memory on a sector's boundary, go straight between the disk and the
  program's anonymous memory, as Linux's `O_DIRECT` moves them: the
  buffer lends its pages, up to 256 parts of pages at a time, and the
  device reads or writes them in place. The rest goes through the
  kernel's pages. A lent page is pinned while the device works, with a
  count of pins in its page record next to its count of sharers: a page
  the program unmaps meanwhile stays until the device is done, and a pin
  is not a sharer, so the program's own write to the page keeps it
  rather than copying it away from the device. A fork waits for the
  lendings under way, since it makes a private page read-only on both
  sides and the next write copies it; with the device still writing the
  original, the parent's copy would miss the rest of the read.
- A raw transfer's requests take as many parts of pages as the device
  does: 126 on a virtio disk, 32 on an AHCI disk, rather than the 16
  that the page cache's requests take. A transfer of more goes as
  several, up to 16 in flight at once, each settled before the call
  returns even after another fails, since the device may still reach
  the memory. virtio-blk puts each request's chain in a table of its
  own (indirect descriptors), so that a request takes one entry of the
  ring rather than one for each buffer, and rings its doorbell once for
  them all; QEMU then merges requests for adjacent sectors into one read
  or write of its file, as it does Linux's. With event indexes the
  device interrupts once for a batch of completions rather than once
  for each, and the driver rings only when the device asks: under a
  hypervisor each interrupt and doorbell is a trip out of the virtual
  machine.
- `ioctl` on a block device takes Linux's requests for its size
  (BLKGETSIZE64, BLKGETSIZE), sector and block sizes (BLKSSZGET,
  BLKPBSZGET, BLKIOMIN, BLKIOOPT, BLKALIGNOFF), read-only and rotational
  flags (BLKROGET, BLKROTATIONAL), BLKDISCARDZEROES, and BLKDISCARD and
  BLKFLSBUF, which need the write right (radix-block(2)).

Four alternatives were set aside:

- Copying every transfer through the kernel's pages, as the first
  version of this decision had it. A mebibyte went as 16 requests of
  64 KiB, one after the other, and a copy: seven times as long as
  Linux's under KVM.
- Pins counted as sharers. The page then looks shared to the program's
  next write fault, which copies it, and the device writes a page the
  program no longer maps.
- A kernel thread per controller for error handling, as Linux's SCSI
  layer has. Every request in flight has a thread waiting for it, so
  one of them is always there to do the work, and no thread sleeps
  through the years in which nothing goes wrong.
- A page cache for block devices, as Linux keeps by default. Raw access
  is for disk tools, which read or write each place once; a cache would
  hold a second copy of what a mounted filesystem's own caches hold,
  and a program that wants one reads files. Measurements may change
  this for small reads of a whole disk (`dd` with small blocks).

## Consequences

- AHCI (ADR 0112) and NVMe (10a.2) are drivers below the same queue,
  with its I/O classes, retries and recovery.
- The `disk-raw` boot test writes and reads raw at odd offsets on a
  virtio and a SATA disk, discards, asks every request, and the host
  checks the bytes; it also reads into and writes from anonymous
  memory, straight and copied, into pages not yet touched, from a page's
  start and from a sector's, and into read-only memory, which fails; `disk-errors` has QEMU fail SATA reads, once and
  for good, and checks that the first is retried and the second ends in
  EIO.
- `ob bench` has rows for raw reads and writes of 4 KiB, 64 KiB and 1
  MiB of a virtio disk, next to Linux's with `O_DIRECT`, and the gate
  holds them to parity like every other row.
- A fork of a process waits for its raw transfers under way, as long as
  a disk takes; Linux instead copies a pinned page for the child at
  once. Programs that fork while another thread reads a disk are rare,
  and the wait is shorter than the copy is complex.
- Memory other than anonymous memory (a frame, a ring, a mapped file's
  pages) is still copied, and so is everything for the RAM disk.
- Raw writes of parts of one sector through two different nodes (a
  disk and its partition) are not kept apart.
