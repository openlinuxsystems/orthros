<!--
SPDX-FileCopyrightText: 2026 The Dukovo contributors
SPDX-License-Identifier: Apache-2.0 OR MIT
-->

# 0234. DesignWare I2C controllers are polled, at the speed the devices the ACPI namespace names on them allow

- Status: Accepted
- Date: 2026-10-31

## Context

Phase 11e.6, its second part. A laptop's touchpad on I2C (HID over I2C,
11e.6c) sits on one of the chipset's I2C buses: the Dell Inspiron 15
5570's on Intel Sunrise Point-LP's LPSS, AMD laptops' on the FCH's. Both
are Synopsys's DesignWare I2C controller (DW_apb_i2c). The kernel drove
only the SMBus host controller (ADR 0206).

- Intel's controllers are LPSS functions (the PCH's low power
  subsystem): PCI functions 8086:9d60 to 9d65 on Sunrise Point-LP and
  a160 to a163 on -H, or, when the firmware hides them in ACPI mode, ACPI
  devices `INT3442` to `INT3447`. The controller's registers come first in
  the function's window and LPSS's own after them, at 200h: the
  function's and its DMA engine's resets, the address the DMA engine is
  remapped from, and what the function is (I2C, UART or SPI). Their clock
  is 120 MHz. Their interrupt is a legacy PCI interrupt.
- AMD's are ACPI devices, `AMDI0010` with a clock of 150 MHz or `AMD0010`
  with 133 MHz, whose `_CRS` gives their registers.
- The controller (Synopsys's databook, Intel's PCH datasheets) takes
  commands in a transmit FIFO, a byte to write or a read, each with a
  stop after it or a repeated start before it, and puts what it reads in
  a receive FIFO. Built without IC_EMPTYFIFO_HOLD_MASTER_EN, it sends a
  stop as soon as its transmit FIFO runs dry. SCL's high and low times
  are counts of its clock, written for the speed, and SDA's hold time a
  count too (from version 1.10a).
- The ACPI namespace names the devices on a bus: a device's `_CRS` has an
  `I2cSerialBus` resource with its address, the fastest speed it allows
  and the controller's path. A controller's `SSCN` and `FMCN` methods may
  give the firmware's counts for standard and fast mode, and `_PS0`
  powers it on.
- Linux's `i2c-designware`, `intel-lpss` and `i2c-core-acpi` (GPL-2.0)
  drive them so: the bus's speed is the slowest any device allows, the
  firmware's counts are used if it has them, else counts from the I2C
  specification's times (NXP's UM10204), with SDA held 230 ns on Sunrise
  Point's.

## Decision

- **A new crate, `radix-i2c`**, with no code of Linux's, drives the
  controller as a bus master (`designware`): it reads the controller's
  type, version and FIFO depths; sets it up for standard or fast mode
  with counts and an SDA hold; and runs a transfer of messages, writes
  and reads, to one device, joined by repeated starts and ended by a stop.
  An address not acknowledged, a byte not acknowledged and arbitration
  lost are told apart, and a transfer that does not end in time is
  aborted.
- **Polled.** The kernel routes no legacy PCI interrupt, and a transfer
  to a touchpad is a few dozen bytes. While commands are left to queue
  the driver spins in steps of 10 µs, so that the FIFO never runs dry; once
  all are queued it sleeps for the bytes left on the bus. It never has
  more reads queued than the receive FIFO holds.
- **LPSS** (`lpss`): a function that says it is an I2C controller is put
  in reset and out of it, its DMA engine told the window's address, as
  Linux's `intel_lpss_init_dev` does.
- **The kernel finds the controllers** once uACPI runs: LPSS's PCI
  functions, which the PCI scan keeps, with their ACPI nodes by `_ADR`
  under a PCI root bridge of their bus, then the ACPI devices. The
  devices on each bus are those present whose `_CRS` names it; the bus
  runs at fast mode if all allow 400 kHz or more, else standard mode, and
  fast mode if none is named. Each controller is powered on (`_PS0`, and
  D0 for a PCI function) and set up with `SSCN` or `FMCN`'s counts if the
  firmware has them, else counts for its clock, rounded up, with Sunrise
  Point's 230 ns hold. The log names each bus and the devices on it.
- **Buses and devices** are `i2c-1` onwards in `/kernel/bus/i2c/devices`,
  after the SMBus's `i2c-0`, with Linux's names: the adapter's `name` is
  `Synopsys DesignWare I2C adapter`, and a device is `i2c-<_HID>:<nn>`,
  its ACPI name. Drivers find the devices (`i2c::designware::devices`)
  and run transfers (`transfer`), which hold the bus's mutex.
- **uACPI's wrapper** walks every device of the namespace and gives a
  device's parent, whether `_STA` says it is present, its `_ADR` and its
  `_HID`.

## Consequences

- The touchpad's HID over I2C driver (11e.6c) can read and write its
  device.
- The `i2c` boot test gives QEMU, which has no DesignWare controller, its
  pc-testdev, whose memory holds an Intel controller in ACPI mode, with
  LPSS's registers, and an AMD one, set by an SSDT of the test's names
  that also names a device on each bus and one that is not there. The
  kernel must find both and their devices, set them up for fast and
  standard mode with the firmware's counts and the clock's, and take LPSS
  out of reset; `selftest=i2c` reads the registers back and runs a
  transfer on each, which the memory ends at once or refuses; the
  console reads the buses' and devices' names. The memory is not the
  hardware: transfers against a controller that takes its FIFO a byte at
  a time, with devices that answer, refuse or hold the bus, are checked
  on the host, and the Dell's touchpad on the hardware run.
- Not yet: other Intel PCHs (Cannon Lake on), fast mode plus and high
  speed, the controller's interrupt, the AMD PSP's bus semaphore
  (`AMDI0019`), I2C as an SMBus for other drivers, and `/dev/i2c-N` for
  programs.
