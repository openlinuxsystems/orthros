<!--
SPDX-FileCopyrightText: 2026 The Dukovo contributors
SPDX-License-Identifier: Apache-2.0 OR MIT
-->

# 0248. USB-C ports are read through the firmware's UCSI, and nothing on them is set

- Status: Accepted
- Date: 2026-11-01

## Context

On a laptop with USB-C ports (both reference machines have them), the
ports are run by a Platform Policy Manager (PPM): the embedded
controller's firmware, or a USB Power Delivery controller's. It agrees
power with a charger or a phone, picks which end is the host, and
switches alternate modes such as DisplayPort. The operating system
learns what is plugged in, and what power it gives or takes, only by
asking the PPM.

Intel's *USB Type-C Connector System Software Interface* (UCSI,
revisions 1.2 to 3.0) is how it asks, and is published for anyone to
implement. On ACPI machines the PPM's mailbox is the memory a
`USBC000` device's `_CRS` names (`PNP0CA0` as its `_CID`), and the
device's `_DSM` passes the mailbox to the PPM after the operating
system writes a command and from it before it reads the answer. The
firmware notifies the device when a port changes. Power data objects
and request data objects, which say what a charger offers and what was
agreed, are the *USB Power Delivery Specification*'s (revision 3.1).

Linux's `ucsi` and `ucsi_acpi` are GPL and were not used. Linux shows
the ports in `/sys/class/typec`, and the power each takes as a power
supply named `ucsi-source-psy-USBC000:001` and on.

## Decision

- `radix-ucsi`, a compartment with no unsafe code and no dependencies,
  speaks UCSI over a mailbox it is given: it sends one command at a time
  and waits for the CCI to say it is done, acknowledges it, and on an
  error asks GET_ERROR_STATUS why. It reads GET_CAPABILITY,
  GET_CONNECTOR_CAPABILITY, GET_CONNECTOR_STATUS and GET_PDOS, and the
  power data objects and the request data object behind a contract. Its
  tests run it against a simulated PPM.
- The kernel (`acpi/ucsi.rs`) finds the device through uACPI, maps the
  mailbox, resets the PPM, asks to be told of every change it reads
  (connects, roles, power, partners, errors), and logs each port as it
  is: nothing, a charger and the power the machine takes from it, a
  device and the power the machine gives it, or a dock.
  A firmware's notification wakes the `usb-c` thread, which reads the
  ports that changed, logs them and acknowledges the change. Commands
  run AML, so they go one at a time under a lock, only from threads that
  may sleep.
- `/kernel/class/typec/port0` and on give `data_role`, `power_role`,
  `port_type` and `power_operation_mode` as Linux does, and
  `/kernel/class/power_supply/ucsi-source-psy-USBC000:001` and on give
  `type` (*USB*), `online`, `voltage_now`, `current_now` and
  `current_max`, as Linux's do.
- Nothing is set: roles are not swapped (the files cannot be written)
  and alternate modes are left to the PPM, which picks them by itself on
  every machine Phase 11 targets. A machine without the device, or whose
  PPM does not answer its reset, has no ports in `/kernel/class/typec`;
  the log says why.

## Consequences

- The boot test `usb-c` adds a UCSI device to QEMU's namespace with
  `-acpitable` (`tools/xtask/src/acpi/ucsi.asl`): its mailbox is
  `pc-testdev`'s memory and its `_DSM` a simulated PPM in AML, with a
  65 W charger on one port. After the boot reads both ports, a phone is
  plugged into the other, and the test sees the log and the files
  change.
- The log tells a user why a laptop charges slowly (a charger at 5 V, or
  the default 0.5 A), which Linux shows only in sysfs.
- Swapping roles (SET_UOR, SET_PDR), choosing alternate modes, and
  UCSI over I²C (Chromebooks and some phones' PD controllers) are left
  for later; the compartment's mailbox trait is where they would go.
- The firmware's PPM is trusted only as far as its answers' lengths:
  every answer is parsed from its fixed layout, and a malformed one
  fails the command.
