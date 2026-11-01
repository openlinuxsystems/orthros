<!--
SPDX-FileCopyrightText: 2026 The Dukovo contributors
SPDX-License-Identifier: Apache-2.0 OR MIT
-->

# 0199. USB Ethernet adapters share the driver program's link

- Status: Accepted
- Date: 2026-11-01

## Context

Laptops without an Ethernet port, the Framework among them, and most
USB-C docks reach a wired network through a USB adapter, and the
roadmap's 12a asks for CDC-ECM, CDC-NCM and Realtek's RTL8153. These
adapters are not PCI cards: their frames come and go as USB transfers,
which only the thread of their xHCI controller can start (ADRs 0146
and 0147),
and they come and go as they are plugged in, after the PCI scan has
given every card its interface and its stack (ADR 0197). A network
card's interface has a `Nic` under its stack, whose calls the stack
makes under its lock, from its own thread; a USB adapter cannot answer
those calls itself, because every transfer must go through the
controller's thread.

Two things in the tree already fit. The link of ADR 0198 is how a card
driven by a program hands frames to its interface's stack: shared
slots, two rings and a link word, with the stack's side in the kernel
and the driver's side wherever the driver runs, and nothing on either
side that waits for the other. And the USB thread already carries a
device class through rings of its own, for serial ports (ADR 0246).

The RTL8153's first configuration is Realtek's own protocol, and its
second CDC-ECM; QEMU's `usb-net` has RNDIS first and CDC-ECM second.
The kernel sets up a device's first configuration, so either would get
no driver, and QEMU's RNDIS interface, of the ACM subclass, would be
taken for a serial port.

## Decision

- **An adapter's interface has the same `Program` card as a driver
  program's, with the controller's thread on the driver's side of the
  link.** The thread puts what the bulk IN endpoint brings on the link
  (a frame for ECM, the frames of a block for NCM) and wakes the stack;
  the stack's flush wakes the thread, which takes each frame to send and
  sends it on the bulk OUT endpoint, one transfer at a time each way, a
  frame or a block of its own each. No new kind of card, no lock between
  the two threads, and the same bounds checks on both sides.
- **While the link is full the bulk IN endpoint is not read**, and the
  adapter holds or drops what comes, as a card does. The thread marks
  the link's pages as wanting room before it looks again, and the stack
  wakes it when it takes a frame.
- **A transfer that would end on a packet's boundary gets one zero byte
  more**, as Linux's usbnet pads it, rather than a zero-length packet:
  past an ECM frame's end, which the IP header's length ignores, and past
  an NCM block's own length.
- **An adapter gets its interface when it is set up, after the PCI
  cards'**; the interface's stack is built on the controller's thread,
  whose stack is 48 KiB rather than 16, since building it takes a 20 KiB
  frame (ADR 0058). Unplugged, the adapter takes its link away: the
  interface goes down, keeps its addresses, and is given back to an
  adapter with the same MAC address, in any port, so netd finds the same
  interface and asks for its address again.
- **A device whose first configuration is its maker's own, RNDIS or
  ActiveSync is set up in another configuration that has an ECM or NCM
  adapter, if it has one**, as Linux's chooses without the maker's
  driver. Every other device keeps its first configuration.
- **NCM adapters are asked for blocks of at most a page**, the most one
  transfer carries (SET_NTB_INPUT_SIZE), and each block sent carries one
  frame. More frames a block would save transfers; one is what every
  adapter takes.
- **An adapter's link is taken to be up, at 100 Mb/s, until its
  interrupt endpoint says otherwise.** QEMU's says it is connected on
  every poll, and some adapters say nothing until it changes.
- The packet filter is set to the adapter's own frames, broadcasts and
  every multicast frame; an adapter that refuses the request, as QEMU's
  does, is used as it is.

## Consequences

- QEMU's `usb-net` is driven in its CDC-ECM configuration, which the
  usb-ethernet boot test covers, unplug and plug-in again included; the
  NCM blocks and the configuration choice have host tests. No RTL8153 or
  NCM adapter has been tried on hardware yet.
- One transfer each way at a time keeps the thread simple and is slower
  than a queue of them: an adapter at USB 3's speeds will not reach a
  gigabit. Queuing several transfers per endpoint, and several frames per
  NCM block, are the next steps when one is measured.
- Realtek's own protocol (Linux's r8152), RNDIS and the ASIX chips' are
  not driven; their adapters need drivers of their own.
- An adapter holds a slot among the eight cards' interfaces from when it
  is first plugged in, unplugged or not, until the machine restarts.
