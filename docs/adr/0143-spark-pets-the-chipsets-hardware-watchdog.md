<!--
SPDX-FileCopyrightText: 2026 The Dukovo contributors
SPDX-License-Identifier: Apache-2.0 OR MIT
-->

# 0143. Spark pets the chipset's hardware watchdog, which restarts a machine whose Spark stops responding

- Status: Accepted
- Date: 2026-10-28

## Context

Phase 11d gives servers a hardware watchdog: a timer in the chipset that
restarts the machine unless software pets it in time, so that a machine
that hangs comes back by itself. On Linux, systemd pets it
(`RuntimeWatchdogSec`, often 30 s to a few minutes on servers) or the
watchdog daemon does, through `/dev/watchdog`; the timer catches a hung
kernel as well as a hung init.

The chipsets in reach count down in two stages and restart the machine
when the second runs out:

- QEMU's i6300esb, the watchdog function of Intel's 6300ESB I/O
  Controller Hub (8086:25ab), which QEMU adds with `-device i6300esb`.
  Its datasheet gives its registers: a 20-bit preload value for each
  stage, counted at about 1 kHz, behind an unlock sequence, and a
  WDT_TIMEOUT flag that says it restarted the machine.
- The TCO timer of Intel's ICH9 and ICH10, in their LPC bridge at
  00:1f.0, at its ACPI base + 0x60 (the ICH9 datasheet): 0.6 s ticks,
  10 bits a stage, NO_REBOOT in the root complex's General Control and
  Status register, and SECOND_TO_STS to say it restarted the machine.
  QEMU's q35 machine is an ICH9 and always has one.

The Dell's Kaby Lake PCH moved the TCO timer behind its SMBus controller
and NO_REBOOT behind the sideband interface, and the Framework's AMD FCH
has a watchdog of its own (Phase 11e). Neither can be tested in QEMU.

## Decision

- The kernel finds the watchdog at boot, after the PCI scan, on bus 0
  of PCI segment 0: the i6300esb if there is one, as QEMU has it only
  where asked, else the TCO timer of an ICH9 or ICH10. It stops it,
  reads whether it restarted the machine and clears that, and clears
  NO_REBOOT. One that the firmware locked, or left unable to restart the
  machine, is not used. Each stage is half the timeout.
- The power capability gets three methods: `start_watchdog` (a timeout
  in seconds, clamped to what the watchdog takes; it returns the timeout
  set and whether the watchdog restarted the machine last time),
  `pet_watchdog` and `stop_watchdog`. Powering off or restarting gives a
  running watchdog its longest timeout, up to ten minutes, while the
  filesystems are written back, as systemd's `RebootWatchdogSec`
  defaults to; halting stops it.
- Spark starts it once it supervises, with a timeout of 60 seconds
  (`--watchdog <seconds>` on the kernel command line sets another, 0
  none), and pets it every third of that from its loop. A native boot
  program, which Spark waits for, runs before it starts the watchdog; a
  POSIX one, which Spark watches as it supervises, runs with it.
- kdb pets a running watchdog about once a second while it waits for a
  command, since it stops the CPU Spark may need, and after a panic
  every CPU: whoever is at the debugger is not a hung machine.
- `/kernel/class/watchdog/watchdog0` shows it with Linux's names
  (`identity`, `state`, `timeout`, `min_timeout`, `max_timeout`,
  `bootstatus` with WDIOF_CARDRESET), and watchdog(8) shows that as
  util-linux's wdctl does.

## Consequences

- The boot tests `watchdog` (the i6300esb, under BIOS) and
  `watchdog-tco` (q35's TCO timer, under UEFI) give Spark `--test-hang`,
  which has it stop responding once it has started the watchdog, unless
  the watchdog restarted the machine last time. The watchdog must
  restart the machine, the next boot must say so, and Spark must then
  keep it from firing for twice its timeout.
- A Spark that is slow for a minute, under heavy swapping, restarts the
  machine; `--watchdog` gives a longer timeout.
- The Dell's PCH and the AMD FCH come with the hardware checks: the
  Dell's when its disks are free, the FCH's in Phase 11e.
