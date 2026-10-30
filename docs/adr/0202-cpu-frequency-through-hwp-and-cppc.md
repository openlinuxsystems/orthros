<!--
SPDX-FileCopyrightText: 2026 The Dukovo contributors
SPDX-License-Identifier: Apache-2.0 OR MIT
-->

# 0202. CPU frequency through Intel's HWP and AMD's CPPC

- Status: Accepted
- Date: 2026-10-30

## Context

Phase 11c, its third part. Until now the kernel has left each CPU's
speed to the firmware, which on most machines leaves it at the base
frequency or lets it run as the firmware's own setup says, without
saving power when idle or boosting when busy as the operating system
would ask.

Both reference machines' CPUs pick their own speed, given a range and a
preference. Intel's hardware P-states (HWP, from Skylake; the Dell's
Kaby Lake R has it) and AMD's Collaborative Processor Performance
Control through its registers (CPPC, on most of its CPUs from Zen 3;
the Framework's Strix Halo has it) give each CPU levels from 0 to 255: its
lowest, most efficient, nominal (Intel's guaranteed) and highest. The
operating system asks for a lowest and a highest level, a desired one
or the CPU's own choice, and an energy preference from 0 (performance)
to 255 (power). The CPU then changes its speed within them itself, as
its load changes, faster than a governor in the kernel could.
Linux's `intel_pstate` and `amd-pstate-epp` drivers do this, in their
"active" mode, by default.

Older CPUs (Intel's SpeedStep through IA32_PERF_CTL and ACPI's `_PSS`,
AMD's P-states, and CPPC through ACPI's shared memory, `_CPC`) need a
governor in the kernel, and are not on the reference machines.

What Linux shows is what the tools read: cpupower, powertop and
power-profiles-daemon read `/sys/devices/system/cpu/cpufreq`.

## Decision

- **A new crate, `radix-pstate`**, works out the registers of either
  control: what CPUID says the CPU has, its levels from
  IA32_HWP_CAPABILITIES or MSRC001_02B0, its request in
  IA32_HWP_REQUEST or MSRC001_02B3, Linux's names and values for the
  energy preferences, and frequencies. The architecture reads and writes
  the registers, through new methods of the HAL's `Cpu` trait, so the
  crate's tests run on the host.
- **At the end of boot**, each CPU's work thread turns its control on
  (IA32_PM_ENABLE or MSRC001_02B1, which stays on until the CPU is
  reset) and asks for every level from the lowest to the highest, at
  the CPU's choice, with the firmware's energy preference, as Linux
  does. Levels that make no sense (a lowest above the nominal, or a
  nominal above the highest) leave that machine's CPUs to the firmware.
- **Two boot flags**: `pstate=off` leaves the CPUs' speed to the
  firmware, as Linux's `intel_pstate=disable` and `amd_pstate=disable`
  do; `epp=` sets the energy preference by Linux's names, `performance`,
  `balance_performance`, `balance_power` or `power`. Linux sets it in
  its files; Radix's are read-only (ADR 0201), so the flag is how the
  user chooses until the settings can.
- **Frequencies** are for people, worked out as Linux does. Intel's
  levels are 100 MHz apart, but on a hybrid CPU, whose base frequency
  (CPUID leaf 16h) is at its nominal level. AMD's are fractions of the
  nominal frequency, which is the TSC's on every CPU with CPPC; a CPU
  whose cores' highest levels differ ranks them, and its highest
  frequency is that of level 166 (196 on family 19h's models 70h to
  7Fh), Linux's rule. The frequency a CPU ran at is the TSC's times its
  APERF counter's count over MPERF's, over 10 ms, measured when a file
  asks and kept for 100 ms.
- **Linux's files**, in `/kernel/devices/system/cpu` (`/sys/...`):
  `possible`, `present` and `online`, as Linux's lists (`0-7`), for
  every machine; and where a control is on, `cpufreq/policy<n>` for each
  CPU, as Linux's drivers have them, with `cpu<n>/cpufreq` a link to it.
  Each holds `affected_cpus` and `related_cpus`, `cpuinfo_min_freq` and
  `cpuinfo_max_freq`, `scaling_min_freq`, `scaling_max_freq` and
  `scaling_cur_freq` (kHz), `scaling_driver`, `scaling_governor`
  (`powersave`, the only one) and `scaling_available_governors`, and,
  with an energy preference, `energy_performance_preference` and
  `energy_performance_available_preferences`. Intel adds
  `base_frequency`; AMD `amd_pstate_highest_perf`,
  `amd_pstate_max_freq` and `amd_pstate_lowest_nonlinear_freq`. The
  synthetic filesystem gains symbolic links for this.
- **The log** says which control each machine has: `CPU frequency:
  Intel's HWP on 8 CPUs, 400 to 3400 MHz, base 1800 MHz, energy
  preference balance_performance`, or that the CPUs keep the firmware's
  setting, and why.

## Consequences

- The reference machines save power when idle and boost when busy, as
  under Linux, once they boot Dukovo. The benchmarks run in QEMU, which
  has neither control, so they are unchanged.
- QEMU emulates neither, even under KVM, so the boot tests see
  only the firmware's path and the `cpu` directory's lists; the
  registers are tested on the host, and on the Dell and the Framework
  first.
- Not yet: changing the preference or the range while running (writable
  files), Intel's HWP interrupts and its energy-performance bias on CPUs
  without a preference, the preferred cores' order in the scheduler,
  CPPC through ACPI's shared memory, and CPUs without either control.
