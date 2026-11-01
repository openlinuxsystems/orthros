<!--
SPDX-FileCopyrightText: 2026 The Dukovo contributors
SPDX-License-Identifier: Apache-2.0 OR MIT
-->

# 0198. A card's driver can run as a program, on a device capability

- Status: Accepted
- Date: 2026-11-01

## Context

Drivers run in the kernel (ADRs 0052 and 0180), where a bug in one is a
bug in the kernel. The specification (section 15) asks that a
non-trivial driver run isolated, as a program, and the roadmap's 12a
names the e1000's: the network card QEMU, VirtualBox and VMware give a
machine by default, whose driver (ADR 0197) the kernel already has. The
model is the one the driver host of 1.6.0 builds on, so it has to be
general enough for any PCI card, not only the e1000.

A driver program needs four things the kernel holds: the card's
registers, memory the card can reach and nothing else, the card's
interrupts, and a way to hand frames to the network stack, which stays
in the kernel (ADR 0180). The capability model has kinds for raw
interrupt lines, register pages and DMA memory (7, 9 and 10), after
seL4's, but none of them knows which card it belongs to: an IOMMU
confines a card's DMA to its own domain, an MSI is programmed in the
card's own configuration space and remapped for it alone (ADRs 0164 and
0166), and a register page may be shared with another card's
registers. Handing out those ranges would let a driver reach what is
not its card's.

## Decision

- **A new kind, `device` (22): one PCI function the kernel left for a
  program to drive**, with the read right (what it is) and control (to
  drive it). The methods are in radix-device(2):
  - `map_registers` maps one of its memory BARs, whole, uncached and
    never executable, into the caller's address space. A BAR smaller
    than a page is refused, since its page may hold another card's
    registers. The configuration space stays the kernel's.
  - `map_dma` gives it zeroed memory, mapped both into the caller's
    space and into the card's own IOMMU domain, and returns the address
    the card reaches it at; at most 4 MiB a card, charged to the
    caller's resource domain.
  - `interrupt` gives the card an MSI, whose arrival `wait` reports. A
    card without one is polled. The card's configuration space, where
    its MSI is set, stays the kernel's.
  - `attach_network` makes it a network interface (below), and `wait`
    sleeps until the card interrupts, the stack has frames to send, or a
    timeout.
  - Each mapping goes where the caller says, or, given address 0, where
    the kernel places anonymous memory, and the method returns where: a
    driver on the C library need not guess at addresses its `mmap` may
    take.
- **The kernel leaves a card to a program only when asked and only
  behind an IOMMU.** The boot flag `e1000=program` leaves the e1000 and
  e1000e (radix-intel-net's `em` family) to a program (`e1000=kernel`,
  the default, does not). The IOMMU must put the card in a domain of its
  own, with interrupt remapping on and no other function sending with
  the same requester ID; from then on it refuses a function that would
  share that domain. With `iommu=off`, no IOMMU or a shared ID, the
  kernel drives the card itself and says why, since the card could then
  reach memory, or raise interrupts, that are not its own. Each card left gets its interface at boot,
  `netN` as any card's, down until a driver attaches.
- **Spark holds the root device capability**, which lists the cards left
  and opens a capability to one, one holder at a time. A service file's
  `devices.pci` names the cards a service drives by `vendor:device`, and
  Spark gives it each card found in `SLOT_DEVICE` and the slots after it
  (at most four), keeping no copy, so that the card stops when the
  service ends. A service that names cards of which the kernel left none
  is not started. The e1000's driver is `e1000d`, a Spark service whose
  file asks for nothing else: no network, no files beyond its own.
- **Frames go through a link, memory the kernel and the driver share**:
  a header page and 256 slots of 2048 bytes each way, one ring of
  received frames that the driver fills and the kernel empties, one of
  frames to send that the kernel fills and the driver empties, each side
  moving only its own index. The kernel reads the driver's index once,
  refuses one that runs ahead of its own by more than the ring, takes a
  frame's length only up to a slot and copies the frame out before the
  stack looks at it, so a driver can confuse only its own interface. The
  layout and both sides' rules are `radix_nic::link`, which the kernel
  and the driver both build, with host tests.
- **The driver program builds the kernel's driver crate unchanged**:
  `radix-intel-net` runs in `e1000d` over the same `radix_hw::Mmio` and
  `Dma` types, made from the mapped registers and DMA memory, the one
  place `e1000d` has unsafe code. Each frame is copied once more than in
  the kernel, from the card's slot to the link's.
- **When the device capability goes, the card stops before anything
  else goes**: the kernel turns off its bus mastering and its MSI, frees
  the interrupt, takes the registers, DMA memory and link out of the
  driver's space, and only then frees the memory, so a driver that
  crashes leaves nothing the card can still reach. The interface stays,
  down, with its addresses, and takes the next driver that attaches:
  Spark starts `e1000d` again, and the network comes back.

## Consequences

- A driver bug now ends a program, which Spark restarts, not the kernel:
  the `userspace-e1000` boot test kills `e1000d` while frames move and
  checks that the card stops, the driver comes back and DHCP and the
  traffic go on.
- The e1000 in a program costs a copy per frame and a system call per
  batch more than in the kernel; it is off the `ob bench` rows, and only
  it runs as a program until the 1.6.0 driver host, as the roadmap's risk
  table says.
- dukovo.toml's `features.userspace-e1000`, on by default as the roadmap
  has it, puts `e1000=program` on the command line of the images
  `ob image` and `ob run` build, unless it names `e1000=` itself; the
  boot tests set their own, and the image's other cards are unchanged.
  `e1000d` is always in the image, and is not started without a card.
- Kinds 7, 9 and 10 keep no methods; a device's resources come only
  through its device capability.
- Left for later: INTx interrupts for cards without MSI (they are
  polled, as in the kernel), MSI-X, whose table is in a BAR the driver
  maps, with more than one queue, and leaving other families to
  programs, which needs only a line in the kernel's list and a driver
  program.
