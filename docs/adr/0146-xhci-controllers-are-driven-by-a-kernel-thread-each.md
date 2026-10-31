<!--
SPDX-FileCopyrightText: 2026 The Dukovo contributors
SPDX-License-Identifier: Apache-2.0 OR MIT
-->

# 0146. xHCI controllers are driven by a kernel thread each, from Intel's specification

- Status: Accepted
- Date: 2026-10-28

## Context

Phase 11a brings USB: the host controller first, then hubs, keyboards and
mice, and a console over the controller's debug port. Every PC sold since
about 2012 has an xHCI controller (Intel's eXtensible Host Controller
Interface, PCI class 0c/03/30), which drives USB 3 and USB 2 devices on
the same ports. (Corrected by ADR 0243: this said that the older EHCI,
OHCI and UHCI controllers survive only on machines too old to boot
Dukovo. They do not: Intel's 6 and 7 Series chipsets and AMD's of the
same years carry their USB 2.0 ports on EHCI controllers, with OHCI or
UHCI companions, beside or without an xHCI, on machines that boot
Dukovo; Phase 11g drives them.) QEMU's `qemu-xhci` is the controller the boot tests
already boot from, through the firmware's own driver.

An xHCI controller is driven through rings in memory: the driver puts
commands (Enable Slot, Address Device, Configure Endpoint and so on) on a
command ring and transfers on a ring per endpoint, rings a doorbell, and
the controller answers on an event ring, with an interrupt. A device is
given a slot and a context in memory the controller keeps up to date. The
firmware may own the controller through its own driver and SMIs, and
hands it over through the legacy support capability when asked.

Linux's `xhci-hcd` is GPL and spread over the USB core's generic layers;
FreeBSD's `xhci.c` (BSD) sits under its own `usb` stack. Both are written
for a stack where any thread may submit a transfer at any time, with a
lock around the rings and completions run from the interrupt.

## Decision

- The driver is our own, written from Intel's xHCI specification (rev
  1.2) and the USB 2.0 and 3.2 specifications, as two compartments with
  no unsafe code: `radix-usb` holds what every USB driver shares (speeds,
  requests, descriptors and their parsing, class names), and `radix-xhci`
  the controller (its registers, rings, commands, slots and control
  transfers on endpoint 0). Their tests run on the host, against a
  simulated controller that follows the specification.
- Each controller is run by one kernel thread, `usb1`, `usb2` and on, and
  by nothing else: the thread takes the controller from the firmware (the
  legacy handoff, with its SMIs turned off), resets and starts it, and
  then waits for events. Its interrupt (one MSI-X entry, else MSI, else a
  10 ms poll) only wakes the thread. Commands and control transfers are
  synchronous: the thread puts one on its ring and reads the event ring
  until its answer comes, keeping the events meant for others to handle
  after. With one owner the rings need no lock, and nothing is logged with
  a lock held.
- A device found on a root port is reset (USB 2 ports; USB 3 ones reset
  themselves), given a slot and addressed, its packet size is read and
  set, and its descriptors and strings are read and logged with its bus
  and port, named as Linux names them (`1-3`): product, IDs, maker, class
  and speed. A device that cannot be set up twice is left alone until it
  is unplugged. Unplugging one disables its slot.
- A command or transfer that does not finish within its time is aborted
  (the command ring) or stopped and skipped (the endpoint); a stalled
  endpoint is reset. A controller that halts with an error is logged and
  left.
- Before the machine is powered off or restarted, every controller is
  halted, so none goes on writing to memory. `usb=off` leaves the
  controllers as the firmware left them.

## Consequences

- The boot test `usb-devices` gives QEMU's controller a keyboard and a
  mouse beside the boot stick, plugs a tablet in and pulls it out: each
  must be named on its port. Every other case that boots from USB runs
  the driver too.
- Hubs, and so devices behind them, come in the next part of 11a, with
  interrupt and bulk endpoints; then HID keyboards and mice with console
  keymaps, and a console over the xHCI debug capability. USB storage
  (the boot stick, read by the firmware today) comes after.
- A thread per controller costs one context switch per event. USB's
  slowest-moving devices (keyboards, mice) make few; storage, which makes
  many, may want its completions handled in the interrupt later, and the
  driver's single owner can then become a lock.
- Isochronous transfers, streams, USB 3 link power management and
  suspend are not supported yet.
