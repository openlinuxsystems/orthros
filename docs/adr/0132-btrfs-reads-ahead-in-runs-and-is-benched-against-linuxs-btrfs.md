<!--
SPDX-FileCopyrightText: 2026 The Dukovo contributors
SPDX-License-Identifier: Apache-2.0 OR MIT
-->

# 0132. Btrfs reads ahead in runs, and is benched against Linux's Btrfs

- Status: Accepted
- Date: 2026-10-27

## Context

ADR 0127 reads Btrfs files through the page cache a page at a time: each
missing page is one request to the disk and one search of the checksum
tree, and a compressed extent is read sector by sector, ten requests for
a typical 40 KiB of zstd. Linux reads ahead: a sequential reader's pages
come in runs that the block layer sends as one request, their checksums
are found in one walk of the checksum tree, and a compressed extent is
one request.

`ob bench` had no row for a filesystem: the disk rows read and write
raw devices (ADRs 0111 and 0115). The gate (ADR 0107) needs a row for
each thing Radix does that Linux does, and Btrfs is the first filesystem
Radix writes that Linux has in the same form.

## Decision

- **Runs**: a backing may read several pages at once
  (`Backing::read_run`): given pages from the missing one on, it fills
  as many as one read of its own gives and says how many. By default it
  reads one, so FAT and the other backings are as before.
- **Read-ahead in the page cache**: a read that misses a page reads up
  to its own last page, and more when it starts the file (three times
  what it asks for) or goes on from cached pages (as many as are cached
  right before it), `READ_AHEAD` (32) pages at least, at most `READ_RUN`
  (256, 1 MiB) in all, never past the end of the file nor over a cached
  page. A sequential reader's runs so double up to 1 MiB, as Linux's
  window grows, with nothing kept per file: the pages behind the read
  say how far it has come. A read elsewhere in a file reads only its own
  pages. Short of memory, a run is shorter.
- **Runs of the page cache in one go**: a request of the page cache or
  a filesystem takes up to 256 pages (`radix_block::MAX_FRAMES`), not
  16: up to 16 go as one request with their buffers listed on the stack,
  as before, and more as a raw transfer goes, in requests of as many as
  the device takes, all in flight at once (ADR 0111).
- **Btrfs runs**: the pages of one uncompressed extent from the missing
  one on, as many as the run has, are one request to the disk, from the
  first copy; their checksums are checked in one walk of the checksum
  tree's items. A sector whose checksum fails, or each one when the
  request fails, is read again on its own from each copy in turn, as
  before. A sector no copy gives ends the run there: the pages before it
  are kept, and the read fails only when it is the one the program
  asked for, as Linux's does.
- **CRC-32C by the processor**: Btrfs's default checksum is worked out
  with SSE4.2's `crc32` where the processor has it (CPUID, asked once),
  as Linux's `crc32c-intel` does, eight bytes an instruction in three
  runs at once, the runs' registers joined with tables made at compile
  time; elsewhere with the tables as before. The instruction is
  assembly, which the Btrfs compartment cannot hold, so it is in
  `kernel/lib/hw` (`radix_hw::crc32c`), the one more place that crate
  allows `unsafe` (docs/coding-standard.md). It uses general registers
  only, so the kernel's vector registers stay unused.
- **A compressed extent in one request**: its sectors are read into the
  workspace in one request and checked in one walk, then uncompressed;
  the run takes the extent's pages from the missing one on, and the page
  cache the extent's others, as before.
- **zstd at speed**: the decoder (ADR 0128) decodes a block's sequences
  128 at a time into lengths and offsets, then makes their copies, so
  that each loop keeps its values in registers; the three codes' states
  are one table, their indexes masked to its size rather than checked;
  most sequences' literals and match are each one copy of 16 bytes,
  which may run past them into room the buffers keep for it (raw
  literals are copied into the literals buffer for that); Huffman
  literals come from four streams at once. Everything stays safe Rust:
  the Btrfs compartment has no `unsafe`.
- **Benchmarks** (docs/design/ipc.md): a third disk, a virtio disk of 192
  MiB that `ob bench` makes afresh with `mkfs.btrfs --compress zstd`,
  holds 32 files of 1 MiB of noise, which `mkfs.btrfs` stores plain, and
  32 of text, which it stores as zstd extents. `posix/bench --btrfs`
  reads each file whole in 128 KiB reads (`posix_btrfs_read_mib`,
  `posix_btrfs_read_zstd_mib`, per file), then makes a file, writes 4
  KiB, closes and removes it, unsynced (`posix_btrfs_create`). Radix
  mounts the disk as Spark does (`/volumes/vdb`); Linux's VM mounts it
  at `/mnt/vdb`. The runner's kernel has Btrfs as a module, so
  `ob bench` puts the modules it needs (from the runner's
  `/lib/modules`, in the order `modules.dep` gives) in the Linux VM's
  initramfs, and its init loads them before it mounts. Both systems read
  the disk cold: each boot mounts it afresh.

Rejected:

- **Linux's read-ahead state**, a window kept per open file that grows
  and shrinks, read in the background as the reader nears its end: the
  pages already cached behind a read size its run as well for a reader
  going through a file, with no state per file.
- **Several extents per run**: a run ends at its extent's end, so a file
  of 128 KiB compressed extents takes a request each. Requests for
  neighbouring extents could be merged later; the gate decides.
- **A built Btrfs in the Linux VM's kernel**: the runner's kernel is the
  one every other row boots, and building another would compare against
  a kernel no one runs.

## Consequences

- A sequential read of a plain Btrfs file is one request to the disk
  per 64 pages, and of a compressed one, one per extent; host tests
  count the requests, check runs that stop at a cached page or at the
  end of the file, and read a file around a sector no copy gives.
- FAT still reads a page at a time; it gains runs when it implements
  `read_run`.
- CRC-32C of a MiB in 4 KiB sectors took 51 µs with the instruction and
  873 µs with the tables on a test host; a host test checks the
  instruction against the bit-at-a-time definition at every length
  around a pass's three runs, and Btrfs's tests check the tables against
  it.
- The zstd decoder went from 226 to 666 MB/s on a test host's text of
  the benchmark's kind, where the reference `zstd` tool (C, with its
  assembly and BMI2) decodes 814 MB/s.
- The three Btrfs rows are gated at 1.00, as every row is.
