<!--
SPDX-FileCopyrightText: 2026 The Dukovo contributors
SPDX-License-Identifier: Apache-2.0 OR MIT
-->

# 0233. GPIO controllers are driven from their ACPI devices, and run the firmware's GPIO events

- Status: Accepted
- Date: 2026-10-31

## Context

Phase 11e.6, its first part. A laptop's touchpad on I2C (HID over I2C,
11e.6c) tells that it has a report on a GPIO pin, not on an interrupt of
its own: the Dell Inspiron 15 5570's on a pad of the Sunrise Point-LP
PCH's GPIO controller, AMD laptops' on a pin of the FCH's. The firmware
also takes some of its own events on GPIO pins rather than GPEs, as a lid
or an embedded controller on many AMD machines, and lists them in the
controller's `_AEI`, whose methods the operating system runs (ACPI 6.5,
5.6.5). None of it works before the kernel drives the controller.

- The ACPI namespace names the controller (`AMDI0030`, `AMD0030` or
  `AMDI0031` for AMD's, `INT344B` for Sunrise Point-LP's) and its `_CRS`
  its registers and its one interrupt, a global system interrupt that is
  usually level-triggered and active low. A device's `_CRS` names its
  pins as GPIO connections (`GpioInt`, `GpioIo`): the controller's path,
  the pin and how it signals. The kernel read no resource template yet.
- AMD's controller (AMD's Processor Programming References) has a
  register a pin, four bytes apart, with its trigger, its interrupt's
  enable and mask, its level and its pending status, which writing 1
  clears; summary registers say which group of four pins has one pending,
  and a master register's bit ends the interrupt.
- Intel's (the 100 Series PCH datasheet, volume 2) is split in
  communities, each with its own registers: two configuration registers a
  pad, and for each group of 24 pads a status register and an enable
  register. A pad may belong to the Management Engine, may have its events
  go to the firmware's GPEs rather than the driver (HOSTSW_OWN), and may
  be locked.
- Linux's `pinctrl-amd` and `pinctrl-intel` (GPL-2.0) drive both, and its
  `gpiolib-acpi` runs `_AEI`: `_Exx` or `_Lxx` if the controller has one
  for the pin, else `_EVT` with its number.
- The kernel routed no interrupt but ISA's and the SCI through the I/O
  APIC.

## Decision

- **`radix-acpi` reads resource templates** (`resources`): memory and
  I/O ranges (fixed, ranged and address space descriptors), interrupts
  (IRQ and Extended Interrupt) with how they signal, GPIO connections and
  I2C serial bus connections. A descriptor shorter than its kind's fixed
  part ends the walk with an error.
- **uACPI's wrapper** finds a node by a path, absolute or from a node, as
  a resource names its controller, and runs a method with an integer
  argument; a buffer keeps its last byte, a resource template's checksum,
  where a string loses its NUL.
- **The HAL routes a global system interrupt** with the trigger mode and
  polarity a device's resources give (`route_interrupt`), and refuses an
  I/O APIC input that has another already.
- **A new crate, `radix-gpio`**, with no code of Linux's, drives AMD's
  controller and Sunrise Point-LP's: a pin set up to interrupt on a level
  or an edge, masked and unmasked, the pins whose interrupts are pending,
  read and cleared, and a pin's level. A pin the firmware keeps, set up
  for another function, or locked otherwise than asked is refused. Writes
  to an AMD pin's register leave its pending status. Sunrise Point-LP
  cannot trigger on both edges.
- **The kernel finds the controllers** once uACPI runs, maps their
  registers, turns every pin's interrupt off as Linux does, routes each
  controller's interrupt and logs it. Its handler masks a level-triggered
  pin that interrupted, and any pin no driver asked for, and calls the
  pin's handler; the pin's driver unmasks it once it has served its
  device. Drivers ask for the pin a `GpioInt` resource names (`gpio::request`).
- **The firmware's GPIO events** in each controller's `_AEI` run Linux's
  methods in a thread of their own, `gpio-events`; a level-triggered pin
  is unmasked after its method has run. The log lists them.

## Consequences

- Touchpads on I2C (11e.6c) can be told when they have a report, and the
  firmware's events on GPIO pins reach its methods.
- The `gpio` boot test gives QEMU, which has no GPIO controller, its
  pc-testdev, whose 64 KiB of memory at 0xff000000 hold an AMD and an
  Intel controller's registers that an SSDT of the test's names, the
  device's ISA interrupts 5 and 6 theirs. The kernel must set up both and
  ask for the three events their `_AEI` list; `selftest=gpio` then runs
  the SSDT's `FIRE` methods, which set a pin's status and pulse the
  interrupt, and each event must run its method once. The memory is not
  the hardware: the drivers' handling of pending bits, ownership and locks
  is checked against simulated controllers on the host, and the Dell's
  touchpad pin on the hardware run.
- Not yet: other Intel PCHs' layouts (Cannon Lake on, whose groups are not
  24 pads, and the server PCHs), the pins' debounce times and pulls the
  resources give, GPIO pins as outputs for drivers, and a GPIO device in
  `/kernel` for programs.
