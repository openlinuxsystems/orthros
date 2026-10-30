<!--
SPDX-FileCopyrightText: 2026 The Dukovo contributors
SPDX-License-Identifier: Apache-2.0 OR MIT
-->

# 0201. Batteries, AC adapters and lids through uACPI, and the embedded controller

- Status: Accepted
- Date: 2026-10-30

## Context

Phase 11c, its second part. With uACPI running the firmware's AML
(ADR 0200), the kernel can reach what a laptop describes only there: its
battery (`PNP0C0A`), with `_BIX` (or `_BIF` before ACPI 4.0) for what it
is and `_BST` for what it does; its AC adapter (`ACPI0003`, `_PSR`); its
lid (`PNP0C0D`, `_LID`); and a power button that is a device of the
namespace (`PNP0C0C`) rather than the fixed one of ADR 0142. Each
notifies (`Notify`) when it changes.

On a laptop these methods read the embedded controller (`PNP0C09`), a
microcontroller with two I/O ports that AML reaches through
`EmbeddedControl` regions, and which raises a general-purpose event when
it has news, such as the adapter plugged in or the lid closed: the host
then asks it which (the query command) and runs its `_Qxx` method, which
notifies the device. uACPI leaves the controller to the kernel. Without
it, most laptops' batteries cannot be read at all, and on many machines
`_STA` and `_INI` methods read it while the namespace is initialized.

What Linux shows is what the tools read: upower, `acpi`, `battery`
widgets and scripts read `/sys/class/power_supply`, and acpid's scripts
`/proc/acpi/button/lid/*/state`. QEMU emulates none of these devices.

## Decision

- **The embedded controller** is `radix-uacpi`'s own (`ec`), as safe
  Rust over the host's ports and clock: found once the namespace is
  loaded and before it is initialized, so that `_REG`, `_STA` and `_INI`
  can read it. Its `_CRS` names the data port, then the command and
  status port; its `_GPE` the event in `\_GPE`; `_GLK` whether its
  transactions take ACPI's global lock. Transactions are one at a time:
  each step waits for the input buffer to empty or the output buffer to
  fill, polling a microsecond apart for 100 µs and then a millisecond
  apart, up to Linux's 500 ms. An event wakes the events thread, which
  queries the controller until it has nothing more (at most 32 at a
  time) and runs each `_Qxx`. The ECDT, which describes the controller
  to tables loaded before the namespace, is not read; neither are
  controllers in GPE block devices.
- **The crate's namespace API** (`find_devices`, `Node`) finds devices by
  `_HID` or `_CID`, evaluates their objects into integers, packages and
  bytes without allocating, and installs notify handlers, which run in
  the notify thread. `radix_uacpi::power` reads a battery's packages as
  Linux's `battery.c` does: its status, percentage and level, and its
  technology from the type string.
- **The kernel's `acpi::devices`** finds up to four batteries, two
  adapters, two lids and two power buttons once uACPI has started, logs
  what each is and hears their notifies. A battery's description is
  read again after a notify of 0x81, its state when a file is read a
  second after the last read, as Linux's `cache_time`, or after a notify
  of 0x80. No AML runs under a lock. A namespace power button's press is
  a press of the power button, which Spark hears as before; an adapter
  plugged in or out and a lid opened or closed are logged.
- **Linux's files.** `/kernel/class/power_supply/<name>` (`/sys/...`), by
  the device's name in the namespace, as `BAT0` and `ADP1`, with Linux's
  ACPI drivers' files and units: `energy_*` and `power_now` for a battery
  that counts in watts, `charge_*` and `current_now` for one in amperes.
  A value the battery does not know fails to read with EIO, as Linux's
  fail with ENODEV. `/processes/acpi/button/lid/<name>/state`
  (`/proc/...`), Linux's line.
- **Tests.** The crate's host test gives its simulated machine an
  embedded controller at ports 0x62 and 0x66 and a DSDT whose battery,
  adapter and lid read it: the controller is found before `_REG` runs,
  reads and writes go through it, and an event's two queries run their
  `_Qxx` methods, whose notifies reach the handlers. The boot test
  `acpi-power` gives QEMU an SSDT of its own (`tools/xtask/src/acpi/power.asl`,
  compiled by `iasl` and kept beside it) with a battery, an adapter, a
  lid and a power button, and reads their files at the console.

## Consequences

- A laptop's battery, adapter and lid are read, which upower and the
  like expect, once their files are on the reference machines. The
  embedded controller's driver is part of the kernel's trusted
  computing base, as the AML it serves is.
- Nothing acts on them yet: a closed lid or a battery running out does
  not suspend the machine, which needs sleep states other than S5, and
  Spark is told only of power button presses. There are no uevents.
- QEMU cannot raise an embedded controller's event or a notify, so those
  are tested only on the simulated machine; the Dell and the Framework
  give them their first real run.
- Not yet: the ECDT, controllers in GPE block devices, the controller's
  burst mode, battery alarms (`_BTP`), charge limits (`_BTH`), the sleep
  button (`PNP0C0E`), and thermal zones and CPU frequency (ADR 0202).
