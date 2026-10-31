<!--
SPDX-FileCopyrightText: 2026 The Dukovo contributors
SPDX-License-Identifier: Apache-2.0 OR MIT
-->

# 0235. HID over I2C touchpads are read as the mice they are until told otherwise

- Status: Accepted
- Date: 2026-10-31

## Context

Phase 11e.6, its third part. A laptop's touchpad, and on some its
keyboard or touchscreen, is a HID device on one of the chipset's I2C
buses, which the kernel now drives (ADR 0234): the Dell Inspiron 15
5570's, on Intel Sunrise Point-LP's LPSS, and AMD laptops'. Its reports
are HID's, as a USB mouse's are (ADR 0148), but the transport is
Microsoft's *HID over I2C Protocol Specification* (1.00).

- The ACPI namespace names the device with `PNP0C50`, as its `_HID` or
  more often its `_CID`, its own `_HID` being the vendor's. Its `_CRS`
  gives its address on the bus and its interrupt, a GPIO pin's on most
  machines (`GpioInt`), which the GPIO controller's driver (ADR 0233)
  routes. Its `_DSM`, function 1 of UUID
  3CDFF6F7-4267-4555-AD05-B30A3D8938DE, gives the register of its HID
  descriptor.
- The HID descriptor, 30 bytes, gives the other registers: the report
  descriptor's and its length, the input register and the longest input
  report, the output register and the command register, which takes the
  requests RESET and SET_POWER. A register is two bytes, low first,
  written before what is read from it.
- The device holds its interrupt until its report is read: the host
  reads the input register, with no register written first, and the
  report's length is its first two bytes. After a reset it interrupts
  once, with a length of 0.
- A touchpad's report descriptor has a mouse's collection and a
  Precision Touchpad's. It reports as the mouse, relative motion and
  buttons, until the host sets its input mode feature to report fingers.
- Linux's `i2c-hid-acpi` and `i2c-hid-core` (GPL-2.0) set it up so:
  `_PS0`, the `_DSM`, the descriptor, the interrupt, SET_POWER on (sent
  twice when a device that sleeps deeply does not acknowledge the
  first), RESET and a wait for its interrupt, then the report
  descriptor. They read the input register once each interrupt.

## Decision

- **`radix-i2c` gains `hid`**, with no code of Linux's: the descriptor,
  read and checked (version 1.00, a report descriptor of at most 4 KiB,
  input reports of at most 512 bytes); SET_POWER and RESET; the report
  descriptor; and an input read, which tells a report from a reset and
  from a length the descriptor does not allow. It speaks to the device
  through a trait, which the kernel implements with a transfer on the
  device's DesignWare bus and the host tests with a simulated device.
- **uACPI's wrapper** evaluates a `_DSM` (`Node::dsm`): its UUID as a
  buffer, its revision and function, an empty package.
- **The kernel** finds, once the buses are found, the devices on them
  whose `_HID` or `_CID` is `PNP0C50` or `ACPI0C50`, four at most, and
  starts the thread `i2c-hid`. It sets each up as Linux does, waiting a
  second at most for the reset's interrupt and going on without it with
  a warning, then lays the report descriptor out with `radix-hid`. Each
  device's pin interrupt sets its bit and wakes the thread, which reads
  the input register once, decodes the report and tells what changed
  where USB's reports go: keys to `/devices/kbd`, the pointer to
  `/devices/mouse`. A level-triggered pin stays masked from its
  interrupt until the report is read. A device whose reads fail more
  than three times in a row is given up, every key and button it held
  let go of, and its pin left masked.
- **A touchpad is its mouse.** The kernel does not set its input mode, so
  it moves the pointer and clicks with its buttons; a tap is a click if
  the touchpad's firmware makes it one. The log says what each device
  is, the touchpad by its collection, with the HID descriptor's vendor
  and product IDs.
- **USB's and I2C's HID devices share** how a report is told and how the
  log says what a device is (the kernel's `hid` module); a USB touchpad
  is now logged as one.

## Consequences

- The Dell's touchpad moves the pointer of the desktop and the console's
  mouse, as a USB mouse does.
- The `i2c` boot test's touchpad gains a `_DSM`: the kernel must find it
  by its `_CID`, read the register from it and try to read its HID
  descriptor, which the test device's memory never answers, and say so;
  `selftest=i2c` waits for that before it reads the registers back. The
  protocol itself is checked on the host, against a simulated device
  that answers through the DesignWare simulation and interrupts after a
  reset, and `radix-hid`'s tests lay out a touchpad's report
  descriptor, mouse and Precision Touchpad collections, and decode its
  mouse's reports. The Dell's touchpad is checked on the hardware run.
- Not yet: the Precision Touchpad's own reports (fingers, two-finger
  scrolling, gestures), a device whose interrupt is not a GPIO pin's
  (`Interrupt` resources), output reports (a keyboard's lights),
  SET_POWER sleep when the system suspends, and wake from the touchpad.
