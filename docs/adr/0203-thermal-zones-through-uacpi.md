<!--
SPDX-FileCopyrightText: 2026 The Dukovo contributors
SPDX-License-Identifier: Apache-2.0 OR MIT
-->

# 0203. Thermal zones through uACPI

- Status: Accepted
- Date: 2026-10-30

## Context

Phase 11c, its last part. The roadmap asks that the CPU slow down at the
ACPI passive trip point and the system shut down cleanly at the critical
one. Firmware describes what it measures as thermal zones (`ThermalZone`
objects in the namespace): `_TMP` gives a zone's temperature in tenths of
a kelvin, and its trip points are `_CRT` (power off), `_HOT` (hibernate)
and `_PSV` (slow the processors `_PSL` lists), with `_TC1`, `_TC2` and
`_TSP` for the passive cooling's pace. `_TZP` says how often to look,
and the firmware notifies 0x80 when the temperature changed and 0x81
when the trip points did, often from an embedded controller's event
(ADR 0201). The CPUs' fans are mostly the embedded controller's own on
laptops; ACPI's active trip points (`_ACx`) are for the rest.

Linux's ACPI thermal driver registers each zone with its thermal core,
whose step-wise governor decides the passive cooling over ACPI's trend,
`_TC1 × (Tn − Tn−1) + _TC2 × (Tn − Tt)`, and whose processor cooling
lowers the CPUs' highest frequency by a fifth a step, up to three steps.
What it shows is what the tools read: `/sys/class/thermal`, which
sensors, upower and the desktops' widgets read.

## Decision

- **`radix_uacpi::thermal`** works out what a zone's objects say, so its
  tests run on the host: temperatures in Linux's thousandths of a
  degree Celsius (from 2732 tenths of a kelvin, Linux's 0 °C), trip
  points without the nonsense some firmware gives (at or below 0 °C, or
  above 200 °C), a passive trip point only with its constants, as in
  Linux, and the passive cooling's steps as the step-wise governor takes
  them: one more while the zone is at or past the trip point and the
  trend rises, one fewer once it is below it and the trend falls.
- **The kernel's `acpi::thermal`** finds up to eight zones once uACPI has
  started, logs what each says, and starts the `thermal` thread, which
  looks at each zone every `_TZP`, every `_TSP` while it cools, and at
  once when the firmware notifies 0x80; a notify of 0x81 has its trip
  points read again. No AML runs under a lock.
- **Passive cooling** caps every CPU's highest level, a fifth of it off
  a step (`radix_pstate::cooled`), where the CPUs pick their own speed
  (ADR 0202); the most any zone asks for wins. `_PSL` is not read: it
  lists every processor on the firmware we know, and naming ACPI's
  processors as CPUs needs the MADT's processor UIDs.
- **At the hot trip point** the kernel warns: it cannot hibernate.
- **At the critical trip point** the machine powers off cleanly: Spark
  is asked to, as for a press of the power button, and if the machine is
  still on 30 seconds later the kernel powers it off itself, as Linux's
  `hw_protection_shutdown` does.
- **Linux's files**, in `/kernel/class/thermal` (`/sys/...`):
  `thermal_zone<n>` for each zone, with `type` (`acpitz`), `temp`,
  `mode`, `policy` and `available_policies` (`step_wise`), and
  `trip_point_<k>_type`, `_temp` and `_hyst` in Linux's order (critical,
  hot, passive); and where the CPUs can be slowed, `cooling_device<n>`
  for each, with `type` (`Processor`), `max_state` (3) and `cur_state`.
- **Tests.** Host tests of the crate's module. The boot test
  `acpi-thermal` gives QEMU an SSDT of its own
  (`tools/xtask/src/acpi/thermal.asl`) with a zone, looked at when it
  notifies a change, that stays at 45 °C while the console reads its
  files, then warms 5 °C a read past its passive, hot and critical trip
  points: the test checks each line and the clean power-off.

## Consequences

- A machine that overheats slows down, then powers off cleanly, as under
  Linux; the CPUs' and the embedded controller's own protections stay
  beneath it.
- Spark hears the critical trip point as a press of the power button,
  and logs it so.
- QEMU's CPUs pick no speed of their own, so the boot test sees passive
  cooling that cannot slow them; the cap is tested on the host, and on
  the reference machines first.
- Not yet: active trip points and fans (`_ACx`, `_ALx`), the processors
  `_PSL` names, the throttling states (`_PTC`, T-states) Linux uses past
  the frequency steps, a hibernation at the hot trip point, thermal
  zones' netlink events, and writable files (`mode`, `policy`).
