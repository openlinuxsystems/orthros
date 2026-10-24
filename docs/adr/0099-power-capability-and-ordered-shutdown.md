<!--
SPDX-FileCopyrightText: 2026 The Dukovo contributors
SPDX-License-Identifier: Apache-2.0 OR MIT
-->

# 0099. Turning the machine off is a capability only Spark holds, and Spark stops the services first

- Status: Accepted
- Date: 2026-10-23

## Context

Phase 9 brings shutdown, reboot, poweroff and halt. On Linux any root
process may call reboot(2), and init systems stop the services first
only when asked through their own interfaces. Radix has no ambient
authority: a right to turn the machine off must be a capability. And
the disks must be written back before the power goes, whoever asks.

Powering off needs ACPI's S5 state: its sleep type values are in the
DSDT's `\_S5` package, which is AML, and Radix does not interpret AML
yet (ADR 0013).

## Decision

- A new kernel object kind, Power (the 18th), has an interface,
  radix-power(2), with three methods: `power_off`, `restart` and
  `halt`. The kernel makes one capability to it and gives it to Spark
  in startup slot 10; nothing else holds it unless Spark shares it.
- Each method first writes back every filesystem (`sync_all`) and gives
  the screen back to the kernel log, so the last messages show. Then
  `power_off` enters S5 through the FADT's PM1 control registers, after
  switching the machine into ACPI mode if the firmware left it out;
  `restart` records a clean restart for the crash log and resets the
  machine as before; `halt` stops every processor. A machine without a
  way to power off answers `not_supported`, and Spark halts it instead.
- The S5 sleep types are found by matching the package's encoding in
  the DSDT or an SSDT (`NameOp '_S5_' PackageOp ...`), as firmware
  writes it, without interpreting AML: the same bytes every ACPI
  implementation emits.
- shutdown(8), reboot(8), poweroff(8) and halt(8) ask Spark over its
  control socket (ADR 0101). Spark stops the services in order and then
  calls the capability. `--force` skips the services; the kernel still
  writes the disks back.

## Consequences

- Only Spark, and whoever it shares the capability with, can turn the
  machine off; a compromised service cannot. Until users come (Phase
  13), anyone can ask Spark to.
- The disks are written back on every path, forced or not.
- The S5 pattern match would miss an `\_S5` package built at run time by
  AML methods; such a machine is halted until the AML interpreter comes,
  at the latest with Phase 11's ACPI power button.
- Sleep states other than S5 wait for later power management.
