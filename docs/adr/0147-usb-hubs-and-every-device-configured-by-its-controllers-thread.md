<!--
SPDX-FileCopyrightText: 2026 The Dukovo contributors
SPDX-License-Identifier: Apache-2.0 OR MIT
-->

# 0147. USB hubs are driven by their controller's thread, which configures every device

- Status: Accepted
- Date: 2026-10-28

## Context

ADR 0146 brought the xHCI driver: a kernel thread per controller sets up
the devices on its root ports, gives each a slot and an address and reads
what it is, but goes no further: it gives the controller none of their
endpoints but endpoint 0, and puts none of them in a configuration, so no
class driver can use them. Nor can it reach a device behind a hub, which
is where most USB devices are: a laptop's internal hub carries its camera,
fingerprint reader and Bluetooth; a desktop's front ports and every
monitor or dock with USB ports are hubs; the Framework Desktop's
controllers have a hub on several ports.

A hub is a USB device of class 9 that reports on an interrupt endpoint
which of its ports changed, and is driven with class requests to each
port: read its status, power it, reset it, clear what changed (USB 2.0
chapter 11; USB 3.2 chapter 10 for USB 3 hubs, which differ in their
descriptor, their port status and a request saying how deep they are).
An xHCI controller reaches a device behind hubs by the root port and a
*route string* of hub ports, four bits each, five hubs deep at most. It
must be told which slots are hubs and, for a low- or full-speed device
behind a high-speed hub, which hub's Transaction Translator (and which
of its ports) converts its traffic.

Linux's hub driver (`drivers/usb/core/hub.c`) runs from a workqueue,
with a thread per hub event; FreeBSD's `uhub` from its USB explore
thread, one per bus. Both configure every device after enumerating it,
then match class drivers to its interfaces.

## Decision

- Every device is configured once it is identified, as Linux does: the
  controller is given the endpoints of the first setting of every
  interface of its first configuration (at most eight; the log says when
  some are left out), each with its own transfer ring and a page for its
  data (Configure Endpoint, the endpoint's type, packet size, burst and
  service interval from its descriptors), then the device is sent
  SET_CONFIGURATION. Class drivers (HID next) then start transfers on the
  endpoints they want; a transfer on an endpoint other than 0 is one TRB
  whose completion comes with the controller's other events, not waited
  for.
- Hubs are driven by their controller's thread, with nothing of their
  own. Once configured, a hub's descriptor is read, its slot marked as a
  hub's (its ports, a high-speed hub's think time), a USB 3 hub told its
  depth, its ports powered, and, after their power is good (at least
  100 ms, as Linux waits), each port queued to be looked at; then its
  status endpoint is listened to, and the ports each report names are
  queued.
- The thread looks at queued ports in turn, between its events, never
  from inside another device's setup, so a hub behind a hub costs no
  deeper stack. A port whose device left lets go of it and of everything
  behind it, hub first; a port with a device come leaves it 100 ms to
  settle, resets it (a warm reset for a USB 3 port whose link is down),
  and sets up the device as on a root port, with its route string, its
  speed from the port's status, and its Transaction Translator.
- Devices are named as Linux names them, by bus, root port and hub
  ports: `1-7.8.4`. Hub ports past 15, which a route string cannot name,
  and hubs more than five deep are logged and left alone.
- `radix-usb` gains the hub's descriptors, port status and requests, and
  `radix-xhci` endpoint configuration, transfers on any endpoint with
  recovery from stalls and errors, hub slots and Transaction Translators;
  both are tested on the host, the controller against its simulator.

## Consequences

- The boot test `usb-devices` puts a keyboard behind two of QEMU's hubs
  and a mouse behind one, plugs a tablet into a hub and pulls it out, and
  pulls out the second hub, which must take its keyboard along. QEMU's
  hub is full speed, so high-speed hubs' Transaction Translators and USB
  3 hubs are checked by the host tests until the Dell and the Framework
  Desktop are.
- Configuring every device with every endpoint costs controller memory
  (two pages an endpoint) and, on real controllers, periodic bandwidth
  for interfaces no driver uses; Linux does the same. A device whose
  configuration the controller refuses, for want of bandwidth or
  otherwise, is logged as one that could not be set up, and left alone.
- Multi-TT hubs run with one Transaction Translator, in their first
  interface setting; selecting the per-port setting can come when a
  machine needs it.
