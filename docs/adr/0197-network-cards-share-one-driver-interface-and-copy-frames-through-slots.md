<!--
SPDX-FileCopyrightText: 2026 The Dukovo contributors
SPDX-License-Identifier: Apache-2.0 OR MIT
-->

# 0197. Network cards share one driver interface and copy frames through slots

- Status: Accepted
- Date: 2026-11-01

## Context

The first card the kernel drove was virtio's (ADR 0181), with its own
type in the network stack. Phase 12 adds the cards servers and virtual
machines have: Intel's e1000 and e1000e (QEMU's, VirtualBox's and
VMware's default), igb, igc and ixgbe; VMware's VMXNET3; Amazon's ENA,
on every current EC2 instance; and Google's gVNIC, on Compute Engine.
Realtek, USB Ethernet, Atheros and Broadcom follow in the next pull
request. Each card has its own registers and rings, but the stack wants
the same few things of all of them: take a received frame, hand a frame
to send, ring the doorbells, say whether the link is up and whether the
card failed.

QEMU emulates the e1000, e1000e, igb and VMXNET3, but none of the
others. Their drivers can only be checked against a model of the card.

## Decision

- **One trait, `radix_nic::Nic`, for every card but virtio's**, in a
  crate the drivers share (`kernel/drivers/nic`): `info`, `link`,
  `failed`, `receive`, `can_send`, `send`, `flush`, `dropped` and
  `stop`. The stack's thread calls them one at a time under its lock,
  and none of them sleeps: `start`, which runs once at boot, is given
  how to wait and how to allocate DMA memory. Each driver is a crate of
  its own (`radix-intel-net`, `radix-vmxnet3`, `radix-ena`, `radix-gve`)
  that forbids unsafe code, as the block drivers do.
- **Frames are copied through slots, as virtio's are** (ADR 0181):
  `radix_nic::Slots` gives each ring 2048 bytes for each descriptor, in
  DMA memory allocated once and mapped behind the IOMMU for that card
  alone. Where a card reads and writes frames only in pages it was
  given (gVNIC's queue page lists), those pages are its slots. No
  offloads, one queue pair, one interrupt.
- **Interrupts**: a card that has MSI and should use it gets one MSI
  for every cause; ENA and gVNIC, which have only MSI-X, get every entry
  of their table pointed at the same message, so whichever vector a
  queue names wakes the card's thread. Each driver acknowledges or
  unmasks its card once nothing is left to take, then looks once more.
  A card the kernel cannot give an interrupt is polled, every
  millisecond while frames move and every 10 ms when quiet. The 8254x
  on PCI are always polled, as Linux's e1000 leaves their MSI unused.
- **`failed` is asked only when a poll moved nothing**, since it may
  read a register, which costs a trip to the hypervisor in a virtual
  machine. A card that failed is left stopped.
- **Stopping comes before memory goes.** `stop` turns the card's
  interrupts off and resets it or takes its queues back, and every
  driver stops its card when dropped, also when `start` fails halfway:
  ENA's registers and gVNIC's device own the memory they gave the card,
  so it is reset before any of it is freed.
- **Each family follows FreeBSD's driver for the sequences and errata
  its datasheet leaves out**: em, igb, igc and ix (BSD-2-Clause, with
  Intel's shared code under BSD-3-Clause), if_vmx (ISC), ena with Amazon's ena-com (BSD-3-Clause)
  and gve (BSD-3-Clause, Google). No code is copied; the credits are in
  `credits/freebsd-network-drivers.md`. Linux's drivers (GPL-2.0) were
  read only for facts: gVNIC's admin queue by address on revision 1.
- **Cards QEMU lacks are tested against a simulated card** in the
  crate's host tests: igc, ixgbe, ENA and gVNIC, each model written from
  what the reference driver expects of the hardware (registers,
  doorbells, completions with their phase or sequence bits, interrupt
  masking). The cards QEMU has also run in the `network-cards` boot
  test, behind QEMU's IOMMU, each on a network of its own, where netd
  sets each up by DHCP and 2 MiB goes through each and back.

## Consequences

- A new card is a crate that implements `Nic`, a line in
  `radix::net::identify` and a start call; the stack, `ip`, netd and
  the bench rows need nothing more.
- Each frame is copied once each way, as with virtio. Offloads,
  multiple queues and zero-copy are later work, each measured against
  Linux first.
- Real machines are covered only as far as their cards follow the
  datasheets and the models: the first boot on each kind of instance or
  board is the real test. What is left for later:
  - ENA's low-latency queues (descriptors in the card's memory), which
    some instance types prefer or require; the driver puts them in host
    memory now.
  - gVNIC's DQO queue format, which Google's third-generation machine
    types use instead of GQI-QPL; the driver refuses a card without
    GQI-QPL.
  - An interface MTU below 1500 (Compute Engine's networks default to
    1460), which needs the MTU from DHCP's option 26.
  - SFP+ modules on the 82599, whose links are taken as the NVM set
    them up; fiber and SerDes gigabit cards are not taken.
  - Legacy INTx interrupts, through ACPI's `_PRT`; a card without MSI is
    polled.
- The e1000 driver also runs as a userspace driver process, over the
  same crate (the roadmap's 12a.4); that leads the next pull request,
  with the capabilities it needs for registers, DMA and interrupts.
