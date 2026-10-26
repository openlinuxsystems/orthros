<!--
SPDX-FileCopyrightText: 2026 The Dukovo contributors
SPDX-License-Identifier: Apache-2.0 OR MIT
-->

# 0115. NVMe disks, written from the specification, with a queue for each CPU

- Status: Accepted
- Date: 2026-10-26

## Context

Both reference machines take NVMe drives: the Framework Desktop has two
M.2 slots, the Dell one. QEMU has an NVMe controller, and Phase 10 puts
the root on a disk. Linux's `nvme` driver is GPL and is not read (the
rule for every GPL source in this project); NVM Express publishes its
specifications for anyone to read.

An NVMe controller takes commands through as many submission queues as
the host makes, each with its own completion queue and interrupt, so
that each CPU can have its own and none waits for another. The queue
every disk's requests go through (ADR 0111) had one lock and one set of
request slots per disk, and every device interrupt went to the boot
CPU, so a second CPU's request waited for the first's lock and its
completion woke it from another CPU. ADR 0093 left more queues for when
measurements called for them.

## Decision

- `radix-nvme` drives NVMe controllers and their namespaces, written
  from the *NVM Express Base Specification 2.0* and the *NVM Command Set
  Specification 1.0*, for controllers of version 1.0 and later. Like
  `radix-ahci` it is a compartment: `forbid(unsafe_code)`, the
  controller reached only through `radix-hw`'s MMIO windows and DMA
  memory, no locks and no sleeping inside. It runs on the host against
  a simulated controller, and Kani proves that reading what a
  controller says of itself and of its namespaces cannot go wrong,
  whatever it says.
- **Setup.** The controller is disabled, given an admin queue, set to
  4 KiB memory pages and the NVM command set, and enabled, each step
  waiting as long as its capabilities say (CAP.TO). The driver asks
  what the controller is (Identify), asks for as many I/O queues as it
  wants (Set Features, Number of Queues), lists the active namespaces
  and asks what each is. A namespace with 512-byte blocks and no
  metadata is a disk; others are logged and left alone. Its physical
  block, the smallest write it takes without reading first, is the size
  it says it is best written in (NPWG), but no more than a write that
  is whole after a power failure (NAWUPF, or the controller's AWUPF),
  as Linux reports it.
- **A queue for each CPU.** The shared queue (ADR 0111) gains *lanes*:
  each lane is a part of the controller with its own lock, its own
  request slots and its own threads waiting. A thread's request goes to
  its CPU's lane. The I/O classes stay the disk's: a request still
  takes room only while no more urgent one waits for it, in any lane.
  An NVMe controller has one lane per CPU, each a submission and
  completion queue pair of 64 entries, at most 16 lanes, and fewer when
  the controller grants fewer queues or MSI-X vectors; its namespaces
  share them, each request naming its namespace. Lane *k*'s completions
  interrupt CPU *k*, through an MSI-X vector aimed at it. AHCI
  ports and virtio disks keep one lane. The cap of 16 keeps the
  kernel's 160 device vectors enough for several controllers; CPUs past
  it share lanes.
- **Data** are described by PRP lists (physical region pages), which
  every controller takes: a request takes up to 128 pages, or fewer
  when the controller says (MDTS), each request slot a list of its own.
  A PRP list needs every buffer but the first to start a page and every
  one but the last to end one; every request the kernel makes does (a
  page cache's pages are whole, and lent memory is contiguous), and the
  driver refuses one that does not. SGLs, which the specification makes
  optional and most drives lack, are not used.
- **Commands.** Reads and writes; writes with Force Unit Access for
  writes through the cache; Flush when the controller has a volatile
  write cache; and Dataset Management with Deallocate for discards when
  it has that, one range of up to 2^32 − 1 blocks a command.
- **Errors.** A command that succeeds is done. One whose status says
  not to retry it (Do Not Retry) fails with EIO: the controller says so
  of what would fail again, such as an invalid field, or a sector it
  cannot read. Any other goes again, five times at most (ADR 0111): an
  abort, a namespace not yet ready, a read the medium got wrong once.
  Linux does the same. ADR 0112 fails an ATA disk's unreadable sector at
  once because ATA has no such bit; NVMe's controller decides.
- **The log.** A request of any disk that fails for good is logged,
  with what the controller said of it (for NVMe, its status), as Linux
  logs I/O errors: at most 10 of a disk's in 5 seconds, Linux's limit
  too, so that a dying disk does not drown the log. This changes ADR
  0111, under which a failed request was not logged.
- **Recovery.** A request unanswered for 30 seconds, Linux's default
  too, has the controller reset: disabled, enabled again, its queues
  made again, and every request that was in flight goes again. A
  controller that says it failed (CSTS.CFS) is reset the same way. One
  that does not come back is dead, and its requests fail.
- **Shutdown.** When the machine is powered off or restarted, after the
  filesystems are written back, every disk's write cache is flushed and
  every NVMe controller is told to shut down (a normal shutdown,
  CC.SHN), waiting as long as it says it takes (RTD3E), at least 5 and
  at most 60 seconds. A drive powered off without one counts an unsafe
  shutdown and may take longer to come up.
- **Names.** Namespaces are `nvme0n1`, `nvme0n2` and on, after the
  controller's number and the namespace's ID, and their partitions
  `nvme0n1p1`, as on Linux.
- Not supported for now, with a line in the log: namespaces with
  4096-byte blocks or metadata, controllers without MSI-X (polled
  instead), the controller memory buffer, and hot plug.

Three alternatives were set aside:

- One queue pair for the whole controller, as AHCI has. Every request then
  waits for the one lock, and its completion comes to the boot CPU,
  which wakes the thread on its own CPU with an interrupt between CPUs.
- A lane for every CPU without a cap. Device vectors are numbered for
  the whole machine rather than for each CPU, and 160 run out on a
  machine with many CPUs and several drives.
- SGLs instead of PRP lists. They describe any buffers, but drives
  that have them also take PRP lists, and many drives do not have
  them.

## Consequences

- The `disk-raw` boot test also writes and reads raw on an NVMe disk,
  `disk-errors` has QEMU fail an NVMe disk's reads and writes, and `storage-nvme`
  runs the storage test on namespaces of two controllers, with as many
  lanes as CPUs.
- `ob bench` has rows for raw reads and writes of an NVMe disk next to
  Linux's with `O_DIRECT`, held to parity like every other row.
- Each lane of a namespace takes 72 KiB of memory (its queues and its
  PRP lists): 1.1 MiB for 16 lanes.
- Every disk is flushed at power off, which takes as long as its cache
  takes to empty; Linux does the same.
- 4096-byte blocks and hot plug can be added inside the crate when
  hardware calls for them.
