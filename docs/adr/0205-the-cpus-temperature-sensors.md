<!--
SPDX-FileCopyrightText: 2026 The Dukovo contributors
SPDX-License-Identifier: Apache-2.0 OR MIT
-->

# 0205. The CPUs' temperature sensors

- Status: Accepted
- Date: 2026-10-30

## Context

Phase 11e, its second part. The roadmap asks for k10temp and the SMU's
temperature sensors, the Framework Desktop's, and the administration
command set's `sensors` (11e.5) needs something to read. Until now the
kernel knew only the firmware's thermal zones (ADR 0203), which many
desktops lack and which say nothing of the processor's own sensors.

- AMD's Zen processors keep their temperatures in the thermal block of
  their System Management Unit (SMU), on its System Management Network
  (SMN), which the root complex (00:00.0) reaches: the SMN address is
  written to its configuration register 0x60, then the value read at
  0x64. Tctl (SMN 0x59800, bits 31:21 in eighths of a degree, 49 less
  when bit 19 or both of bits 17:16 say so) is the temperature the
  processor's cooling acts on. Most desktop and server models also have
  each core complex die's (Tccd), from an offset that changes with the
  model. AMD's Processor Programming Reference gives the SMN; Linux's
  `k10temp` (GPL-2.0) is the reference for which models have which
  registers, and its implementation notes say the dies' come from
  `zenpower`'s experiments, not from a datasheet.
- Intel's processors have a digital thermal sensor in each core and,
  where CPUID leaf 6 says so, one in the package: IA32_THERM_STATUS and
  IA32_PACKAGE_THERM_STATUS count degrees below TjMax, the temperature
  their thermal control starts at, which MSR_TEMPERATURE_TARGET gives
  (Intel's SDM, volume 3, section 15.8). Each CPU reads its own registers.
  Linux's `coretemp` reads them with `rdmsr_safe`, as some processors and
  hypervisors that report the sensor lack a register, and a read of a
  register a processor lacks raises #GP, which until now always panicked
  the kernel.
- Linux shows both, and the thermal zones, in its hwmon class
  (`/sys/class/hwmon/hwmon<n>`), where lm-sensors' `sensors` and the
  desktops' widgets read them: `k10temp`'s `temp1_input` labelled `Tctl`
  and `Tccd<n>` from `temp3`; `coretemp`'s `Package id <p>` in `temp1`
  and `Core <c>` in `temp<c + 2>`, with `temp<n>_crit` (TjMax),
  `temp<n>_max` and `temp<n>_crit_alarm`; and `acpitz`, a temperature
  for each zone with its critical trip point.

## Decision

- **`radix-sensors`**, a crate with no `unsafe` code whose tests run on
  the host, works out what the sensors say:
  - `k10temp` finds a Zen (families 17h, 19h and 1Ah) by its data
    fabric's function 3 (00:18.3, the device IDs Linux's driver binds to)
    and an AMD host bridge at 00:00.0, reads Tctl and the dies' registers
    over the SMN, and keeps the dies whose valid bit is set and whose
    register does not read all ones, as Linux does, at the offsets and
    counts Linux has for each model. A Tctl below 0 reads as 0, as on
    Linux. Its tests run it against a simulated root complex.
  - `coretemp` reads a CPU's target and status registers through a trait
    the kernel implements, and works out TjMax (100 °C where the
    processor gives none, as Linux takes most), the target below it, each
    reading (none while its valid bit is clear) and its alarm.
- **`cpu::rdmsr_safe`** reads an MSR that may not exist: its `rdmsr` is
  at a known address, and a #GP there resumes at a label that makes the
  read fail, as the user copies' faults resume
  ([ADR 0031](0031-system-call-interface.md)). Only the sensors use it.
  A new self-test, `msr`, and its boot test check that EFER reads and
  that a read of MSR 2FFF_FFFFh, which no processor has, fails and goes
  on. QEMU without KVM reads MSRs it does not know as 0, and even the
  x2APIC's in xAPIC mode, so the test checks the fault with KVM, as CI
  runs it, and says it skipped without.
- **The HAL** gives the kernel AMD's sensors through the chipset, found at
  the first call and read under a spin lock that keeps the SMN's index
  and data registers to one reader, and the running CPU's Intel sample.
- **The kernel's `hwmon`** finds the sensors once the CPUs' speed is set
  and logs which, for example `Sensors: AMD's processor sensors (k10temp)
  read Tctl` or `Sensors: Intel's digital thermal sensors (coretemp) read
  1 package and 4 cores, up to 100 °C`. Each core's sensor is read by the
  first thread of the core, and each package's by the first CPU of the
  package that has one. A read of an Intel sensor has every CPU read its
  registers through its work thread, at most once a second, as Linux's
  readings last a second.
- **`/kernel/class/hwmon`** shows `acpitz`, `k10temp` and a `coretemp`
  for each package, in that order, with Linux's files. A sensor that
  gives no reading fails to read (EIO).

## Consequences

- `sensors`, once ported or written (11e.5), and the desktop's widgets
  find the processors' temperatures where they look on Linux.
- QEMU's CPUs have neither sensor, even under KVM: the host tests cover
  the sensors, the `acpi-thermal` boot test reads its zone through
  hwmon's `acpitz` (its SSDT now holds 45 °C for one more read), and the
  `msr` boot test covers the fault-safe read under KVM. The Dell (coretemp) and the
  Framework Desktop (k10temp, Tctl alone, as its Strix Halo is not among
  the models whose dies' registers Linux knows) check the rest on their
  runs.
- AML may reach the root complex's SMN registers through a PCI_Config
  region, outside the kernel's lock; Linux has the same exposure. No
  firmware the kernel has met does.
- Not yet: Tdie, the die temperature below Tctl's offset on a few first
  Ryzens and Threadrippers, which Linux finds by the model's name; AMD
  processors from before Zen and Hygon's; Intel's processors from
  before Nehalem, whose TjMax Linux guesses from the model; the energy
  and power readings of AMD's RAPL counters; fans and voltages, which
  are the embedded controller's and the Super I/O chip's (11e.4).
