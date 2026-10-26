<!--
SPDX-FileCopyrightText: 2026 The Dukovo contributors
SPDX-License-Identifier: Apache-2.0 OR MIT
-->

# 0119. NVMe data in long runs of memory go as scatter gather lists

- Status: Accepted
- Date: 2026-10-26

## Context

ADR 0115 described every NVMe command's data with PRPs, one entry for
each page, and left scatter gather lists (SGLs) out: they are optional,
and many drives lack them. After ADR 0118 a buffer written in order lies
in runs of 128 KiB, and on a second CI runner type (Intel Xeon Platinum
8573C) that brought the virtio 1 MiB rows of `ob bench` under Linux, but
not the NVMe 1 MiB write, at 1.09 to 1.13 times Linux's. QEMU's worker
thread took 108.8 µs to write a 512 KiB command's data from Radix's
buffer and 85.0 µs from Linux's.

QEMU's traces of Linux, under emulation, showed the difference in the
commands rather than the memory: from a buffer in contiguous memory
Linux's 512 KiB write was one SGL data block, which QEMU maps in one
piece, and from a buffer in scattered pages it was 128 PRPs, which QEMU
maps a page at a time, as it maps Radix's. Linux's driver uses an SGL
on an I/O queue of a controller that has them when the request's
segments average at least its `sgl_threshold` module parameter, 32 KiB
by default, and PRPs otherwise. QEMU's controller has SGLs.

## Decision

- A controller that reports SGLs for NVM commands (Identify Controller
  SGLS, bits 1:0 01b or 10b) gets a read or write whose buffers make
  runs of memory averaging at least 32 KiB as an SGL (NVM Express Base
  Specification 2.0, 4.1.2): one data block descriptor in the command
  for a single run, or a last segment descriptor in it for a list of up
  to 64 data blocks, in the kibibyte each request slot has for its PRP
  list. Any other request goes as PRPs, as before.
- Either way the driver takes the same buffers, those a PRP list can
  describe, and refuses any other: a request that works on one
  controller works on every one, and stays within the controller's
  transfer limit (MDTS), which counts pages.

## Alternatives

- **SGLs for every request on a controller that has them.** Runs of a
  page take a 16-byte descriptor each against a PRP entry's 8, and give
  the controller no fewer pieces; Linux's threshold avoids them, and so
  does this.
- **Any buffers an SGL describes, on a controller that has them.** CI's
  QEMU has SGLs and many drives do not: a request only SGLs can carry
  would pass every test and fail on such a drive.
- **Longer runs of frames instead.** QEMU maps a PRP list a page at a
  time however the pages lie, so no placement of frames helps it.

## Consequences

- On QEMU a 512 KiB transfer from a buffer written in order is a few
  data blocks rather than 128 pages, as Linux's is.
- Drives without SGLs, as many client drives are, see no change.
- SGL descriptors have their own encoding and a list format of their
  own, and the simulated controller in the driver's tests decodes both;
  a proof checks that an SGL covers a request's buffers in order.
