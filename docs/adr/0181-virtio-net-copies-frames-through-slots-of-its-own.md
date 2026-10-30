<!--
SPDX-FileCopyrightText: 2026 The Dukovo contributors
SPDX-License-Identifier: Apache-2.0 OR MIT
-->

# 0181. The virtio network card copies frames through slots of its own

- Status: Accepted
- Date: 2026-10-30

## Context

The first network card the kernel drives is virtio's (virtio 1.2,
section 5.1), which QEMU and every cloud that runs on KVM give a
machine. The disks' virtio driver lends the device the pages of each
request (ADR 0156); a network card is different in three ways.

- The device fills receive buffers on its own, whenever a frame
  arrives, so it must always hold some. A frame's buffer cannot be the
  memory of whoever reads it, as no one has asked yet.
- Frames are small (at most 1514 bytes, or 1518 with a VLAN tag) and
  many. Mapping and unmapping each one in the IOMMU (ADR 0164) costs
  more than copying it.
- smoltcp (ADR 0180) hands a frame to send as a borrowed slice for the
  length of one call, and reads a received one the same way. It never
  owns DMA memory.

The card offers more: mergeable receive buffers, checksum and
segmentation offloads, more queue pairs and a control queue. Each one
is code that reads what the device wrote.

## Decision

- **Each queue has a slot of 2048 bytes for each descriptor**, in DMA
  memory allocated once, when the card is set up, and mapped for it
  alone behind the IOMMU. A slot holds the 12-byte `virtio_net_hdr` and
  the largest frame with room to spare. A descriptor's slot is fixed:
  the one at its index.
- **Every receive slot is the device's from the start.** A frame taken
  out is copied into the stack's buffer and its slot handed straight
  back, so the device never runs short while the stack is slow. The
  length the device reports is checked against the slot before a byte
  is read; a frame shorter than an Ethernet header, or longer than its
  slot, is dropped and counted.
- **A frame to send is copied into the slot of the descriptor that
  carries it.** Slots of frames the device has sent are taken back
  before more are sent. When none is free, the stack holds the frame.
- **The driver asks for the MAC address and the link status, and
  nothing else**: no offloads, no mergeable buffers, no control queue
  and one queue pair, whose two queues raise one MSI-X entry. It
  requires virtio 1.0 and takes `VIRTIO_F_ACCESS_PLATFORM` when the
  device offers it, so a card behind the IOMMU works.
- A card with no MAC address of its own gets a random, locally
  administered one.

## Consequences

- Each frame is copied once on its way in and once on its way out. At
  the rates of 12a.1 (the self-test's pings, and later QEMU's user
  network) the copies cost less than the IOMMU's maps would.
- The driver keeps 1 MiB of slots per card for its two queues of 256
  descriptors, allocated once.
- Throughput comes with 12a.3, which measures against Linux and adds
  what the benchmark rows need: checksum and segmentation offloads,
  mergeable buffers and more queue pairs, each with its own checks.
- A device that reports a length past its slot, or names a descriptor
  it was never given (counted as bad entries by the queue), cannot make
  the driver read outside the slots.
