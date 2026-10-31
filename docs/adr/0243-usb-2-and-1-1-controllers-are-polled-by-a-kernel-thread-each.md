<!--
SPDX-FileCopyrightText: 2026 The Dukovo contributors
SPDX-License-Identifier: Apache-2.0 OR MIT
-->

# 0243. USB 2.0 and 1.1 controllers (EHCI, OHCI, UHCI) are polled by a kernel thread each, and Intel's ports go to xHCI

- Status: Accepted
- Date: 2026-10-31

## Context

ADR 0146 drove xHCI, and said the older controllers survive only on
machines too old to boot Dukovo. That was wrong. Intel's 6 Series
chipsets (2011) have no xHCI at all: their USB 2.0 ports sit on two EHCI
controllers, each behind a rate-matching hub. The 7, 8 and 9 Series and
Bay Trail have both: their USB 2.0 ports start on the EHCI controllers
and move to the xHCI one only when the operating system flips a switch
in the xHCI's configuration space (XUSB2PR and USB3_PSSEN, with masks the
firmware sets in XUSB2PRM and USB3PRM). AMD's chipsets of the same years
pair EHCI with OHCI companions; Intel's older ones and VIA's pair it with
UHCI. Every one of these machines boots Dukovo, and QEMU has all three
(`usb-ehci`, `ich9-usb-ehci1` and its `ich9-usb-uhci1` to `3`,
`pci-ohci`, `piix3-usb-uhci`).

The three are older designs than xHCI, and simpler: the driver puts
queue heads (EHCI, UHCI) or endpoint descriptors (OHCI) and transfer
descriptors in memory, in lists the controller walks every frame, and
reads back what each did. The driver addresses each device itself
(SET_ADDRESS), keeps track of hubs' transaction translators on EHCI, and
on UHCI keeps each pipe's data toggle. The controller reads its work
through 32-bit addresses. EHCI hands a low- or full-speed device to a
companion OHCI or UHCI controller by giving up the port.

Linux's `ehci-hcd`, `ohci-hcd` and `uhci-hcd` are GPL. FreeBSD's
`ehci.c`, `ohci.c` and `uhci.c` (BSD) were read as a reference for the
layout of the periodic schedule; nothing is copied.

## Decision

- The drivers are our own, written from Intel's *Enhanced Host
  Controller Interface Specification* (1.0), the *OpenHCI* specification
  (1.0a) and Intel's *UHCI Design Guide* (1.1), in one compartment with
  no unsafe code, `radix-usbhc`. Each implements `radix_usb::host::Hcd`:
  ports (status, reset, disable), synchronous control transfers to a
  device's default pipe, and interrupt pipes that are started and
  polled. Their tests run on the host, against a simulated controller of
  each kind that walks the structures in memory as its specification
  says.
- Each controller is run by one kernel thread, as an xHCI one is (ADR
  0146), which takes it from the firmware (EHCI's legacy support
  capability, OHCI's ownership change request, UHCI's legacy support
  register), resets and starts it. The thread sets each device up
  (address, descriptors, configuration) with the code the xHCI thread
  uses, and runs hubs and HID keyboards and mice as the xHCI thread does.
- The controllers are polled every 10 ms, with their interrupts off: the
  kernel routes no INTx interrupts yet, and keyboards and mice ask to be
  polled every 8 to 10 ms anyway. A controller that stops with an error
  is logged and left; at power-off each is halted, and its thread reads
  no more.
- The periodic schedule of each is a tree of empty heads for periods of
  1 to 32 ms, which the frame list (or OHCI's 32 interrupt entries) leads
  into; an interrupt pipe hangs after the least-loaded head of its
  period. Low- and full-speed devices behind an EHCI's high-speed hub are
  reached through its transaction translator, as split transactions.
- EHCI binds before its companions, so that it routes every port to
  itself first and then gives up those with slower devices. Without a
  companion, a slower device on an EHCI port is logged and left.
- On an Intel xHCI beside an Intel EHCI controller, the kernel moves
  every port the firmware's masks allow to the xHCI before any controller
  starts, logs it, and puts the firmware's routing back at power-off, as
  other systems do: xHCI drives USB 3 and is faster. `usb=off` leaves
  them all alone.
- UHCI's registers are in I/O port space: the hardware layer gains
  `Devices::device_port_read` and `device_port_write`, which the kernel
  calls only within the controller's I/O BAR.

## Consequences

- The boot tests `usb-ehci`, `usb-ohci` and `usb-companions` give QEMU's
  EHCI, OHCI, and ICH9 EHCI with UHCI companions a keyboard that types
  into the shell, a tablet or a mouse behind a hub, and read the buses
  with `lsusb -t`.
- USB sticks on these controllers are not read yet: bulk transfers come
  with a later part. A machine whose USB 2.0 ports are only on EHCI boots
  from them through the firmware, which reads the stick, but the system
  then does not see it; this is logged.
- Isochronous transfers (webcams, audio) and suspend are not supported.
- Polling costs a wakeup every 10 ms per controller while it runs; MSI
  is not offered by these controllers, so interrupts wait for INTx
  routing.
