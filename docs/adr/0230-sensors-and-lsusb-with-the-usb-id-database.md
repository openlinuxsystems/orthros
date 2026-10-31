<!--
SPDX-FileCopyrightText: 2026 The Dukovo contributors
SPDX-License-Identifier: Apache-2.0 OR MIT
-->

# 0230. sensors and lsusb are systools, with the USB ID database as a port

- Status: Accepted
- Date: 2026-10-31

## Context

Of Phase 11's hardware commands, lspci, lscpu and lsmem came first (ADR
0144); sensors and lsusb waited for what they read. The kernel now shows
the processors' and the firmware's temperatures and those of
Framework's embedded controller in `/kernel/class/hwmon` (ADRs 0203,
0205 and 0207), as Linux's hwmon class. It sets up the USB devices on
its xHCI controllers (ADRs 0146 to 0148, 0161 and 0162), but shows none
of them to programs.

On Linux, sensors comes from lm-sensors (GPL-2.0-or-later, its library
LGPL-2.1-or-later), which reads `/sys/class/hwmon` through libsensors
and its configuration of each chip; lsusb from usbutils
(GPL-2.0-or-later), which reads `/sys/bus/usb/devices` through libusb
and names devices from the Linux USB ID Repository's database,
`usb.ids`. The database is data, not code. The repository's page says it
may be distributed under the GPL, version 2 or later, or the 3-clause
BSD license; the copy on GitHub that the maintainer keeps holds the
GPL's version 3 as its LICENSE.

## Decision

- sensors and lsusb are tools of systools (ADR 0096), written for Radix
  in Rust, with the Linux tools' output and common options. No code of
  lm-sensors or usbutils is taken.
- sensors names each chip as libsensors does (`k10temp-pci-00c3`,
  `coretemp-isa-0000`, `acpitz-acpi-0`, `cros_ec-isa-0000`) and prints
  its fans and temperatures, their limits and alarms, in lm-sensors'
  format, with `-u` and `-j` for its raw and JSON output. It needs no
  configuration: the kernel already names each sensor.
- `/kernel/bus/usb/devices` shows each controller's root hub (`usb1`),
  each device set up (`1-3.2`, its bus and ports) and each interface of
  its configuration (`1-3.2:1.0`), with Linux's files: the descriptors'
  fields, the device's strings, its bus, number, path and speed, and
  each interface's driver as a link into `/kernel/bus/usb/drivers`.
  Devices are numbered as Linux numbers them, from 2 on each bus. What
  is shown is copied from the controller's thread when it sets a device
  up, under a lock of its own, at most 64 devices.
- A root hub has no vendor or product ID: Linux gives its root hubs the
  Linux Foundation's (`1d6b`), which Radix does not borrow. lsusb's list
  leaves the root hubs out, and its tree (`-t`) shows them.
- The port `usbids` installs `usb.ids` in
  `/system/resources/hwdata/usb.ids`, where hwdata puts it on Linux,
  unchanged, pinned to a commit of the GitHub copy, under its terms as
  the repository's page gives them, GPL-2.0-or-later OR BSD-3-Clause;
  the license manifest holds the GPL text the copy carries as its
  notice. lsusb reads only the vendors and products it shows, and the
  classes.

## Consequences

- The `usb-devices` boot test runs lsusb and checks its list and tree
  against QEMU's devices; `acpi-thermal` runs sensors against its
  thermal zone.
- lsusb's `-v`, the full descriptors, waits for the kernel to show
  them, as Linux's `descriptors` file does. hwinfo, the last of the
  hardware commands, is Phase 11e.5b's.
- Updating the database is a change of the port's commit, as for any
  port.
