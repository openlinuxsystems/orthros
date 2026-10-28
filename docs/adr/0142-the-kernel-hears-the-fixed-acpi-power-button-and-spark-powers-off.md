<!--
SPDX-FileCopyrightText: 2026 The Dukovo contributors
SPDX-License-Identifier: Apache-2.0 OR MIT
-->

# 0142. The kernel hears ACPI's fixed power button, and pressing it asks Spark to power off

- Status: Accepted
- Date: 2026-10-28

## Context

Phase 11c has the power button shut the machine down cleanly. Until
now, pressing it did whatever the firmware does when no operating
system handles it: on QEMU, nothing; on a PC, what its firmware
chooses, at worst cutting the power with the disks' last writes lost.

ACPI signals the button in one of two ways. As a fixed feature,
pressing it sets PWRBTN_STS in a PM1 status register,
which raises the System Control Interrupt while PWRBTN_EN is set: no
AML is involved. As a device of the ACPI namespace (`PNP0C0C`, the
FADT's PWR_BUTTON flag set), its presses arrive as a general-purpose
event whose AML method sends a `Notify`, which needs an AML
interpreter. Radix has none yet; uACPI comes with the rest of 11c.
QEMU's PC machines, `pc` and `q35`, make the button a fixed feature,
and QMP's `system_powerdown` presses it.

The kernel enters ACPI mode only to power off (ADR 0099). Hearing the
SCI takes the machine into ACPI mode for good, after which the firmware
no longer handles the button, or any other event, itself.

## Decision

- At boot, when the FADT gives a fixed power button and PM1 event
  registers, on a machine that is not hardware-reduced, the kernel
  takes the machine into ACPI mode, clears every fixed event and turns
  off all but the button's, clears and turns off every general-purpose
  event, as nothing can run their AML methods yet, and routes the SCI
  through the I/O APIC: level-triggered and active low, as ACPI defines
  it, unless the MADT overrides it. The interrupt handler clears every
  status bit it finds set, turns off any general-purpose event that is
  set, and counts a press.
- A machine whose button is a namespace device, or that is
  hardware-reduced, is left as it was, in the firmware's mode, so the
  firmware keeps its button.
- The power capability gets a fourth method, `notify_button`: Spark
  gives it a notification, which the kernel signals at each press from
  a thread of its own (a notification cannot be signalled with
  interrupts masked). It fails with `not_supported` where the kernel
  does not hear the button.
- Spark waits for that notification in `poll` with its others. A press
  starts a shutdown to power off at once, as poweroff(8) does and as
  systemd-logind's default `HandlePowerKey=poweroff` does; a shutdown
  set for later is moved to now, and a press during a shutdown changes
  nothing.

## Consequences

- The boot test `power-button` boots normally under BIOS, presses the
  button through QMP and checks that Spark stops the services and QEMU
  ends in S5. `consoles` still powers off through poweroff(8) under
  UEFI.
- No AML runs, so the battery, the lid, thermal zones and namespace
  buttons wait for uACPI. Once it runs, general-purpose events are
  turned on for the methods it has, and namespace buttons reach the
  same `notify_button`.
- With the machine in ACPI mode from boot, an event the firmware would
  have handled in System Management Mode goes unhandled until uACPI.
  The firmware of both reference machines is to be checked on them
  when they are free; the sleep button and wake events are left for
  later.
