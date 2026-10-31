<!--
SPDX-FileCopyrightText: 2026 The Dukovo contributors
SPDX-License-Identifier: Apache-2.0 OR MIT
-->

# 0207. Framework's embedded controller

- Status: Accepted
- Date: 2026-10-31

## Context

Phase 11e, its fourth part. The roadmap asks for the Framework embedded
controller (EC), for fan curves, the power LED and the ARGB header, and
the administration command set's `sensors` (11e.5) wants the fans'
speeds. The kernel knew only the ACPI embedded controller (ADR 0201),
the one the firmware's AML reads its battery and lid from.

- Framework's machines run a fork of the Chromium EC (BSD-3-Clause). The
  Framework Desktop's (its program `dogwood`, on a Nuvoton NPCX over
  eSPI) drives three fans, the power button's LED and a second power LED
  on the mainboard, and the ARGB header through a WS2812 output.
- The host reaches it as Chromebooks reach theirs, at fixed I/O ports:
  a memory map the EC keeps current (temperatures in kelvins less 200
  from offset 0, then from 0x18; four 16-bit fan speeds from 0x10; "EC"
  at 0x20; the host command flags at 0x27), and version 3 of the host
  command protocol, a checksummed request written at 0x800 and started
  by writing 0xda to the command port 0x204, whose status says when the
  EC is done, its result at the data port 0x200 and its response back at
  0x800. The memory map is at 0x900 on Chromebooks (ACPI device
  `GOOG0004`) and at 0xe00 on Framework's (`FRMWC004`), as Linux's
  `cros_ec_lpcs` (GPL-2.0) has it.
- The commands needed: `EC_CMD_HELLO`, `EC_CMD_GET_VERSION`,
  `EC_CMD_TEMP_SENSOR_GET_INFO` (a sensor's name), `EC_CMD_PWM_SET_FAN_DUTY`
  and `EC_CMD_THERMAL_AUTO_FAN_CTRL` (a fan's duty set by the host, then
  given back), `EC_CMD_LED_CONTROL` (an LED's colors and their range, or
  the LED given back), `EC_CMD_RGBKBD_SET_COLOR` (the Desktop's ARGB LEDs)
  and Framework's own `EC_CMD_FAN_CONFIGURATION` (0x3e27), the Desktop's
  fan curve: for each fan, the sensor it follows, a duty that rises from
  a minimum at one temperature to a maximum at another, and whether it
  stops below. `include/ec_commands.h` and the Desktop's program in
  Framework's fork give these.
- Linux shows the EC's fans and sensors in its hwmon class as `cros_ec`
  (`cros_ec_hwmon`): `fan<n>_input` and `fan<n>_fault`, `temp<n>_input`,
  `temp<n>_fault` and `temp<n>_label`, numbered from 1 by their places
  in the memory map.

## Decision

- **`radix-crosec`**, a crate with no `unsafe` code whose tests run on
  the host against a simulated Framework Desktop EC, speaks the protocol:
  it checks the memory map's "EC" and the flag for version 3 before
  sending anything, reads the memory map's temperatures (with the second
  block from version 2 of its thermal data) and fans, and sends the
  commands above, waiting up to a second for each, 10 µs between the
  first hundred looks at the status and 1 ms after, as Linux gives it a
  second. A response's header and checksum are checked.
- **The x86-64 layer** reaches only the EC's ports: reads of 0x200, 0x204,
  the packet (0x800 to 0x8ff) and either memory map, and writes of the
  command port and the packet.
- **The kernel's `crosec`** looks, once uACPI has started, for a present
  `FRMWC004` device, then a `GOOG0004`, and uses the EC behind it if it
  answers `EC_CMD_HELLO`. It logs it, for example `EC: Framework's
  embedded controller (FRMWC004), firmware dogwood-0.0.11 (read-write
  image), with 3 fans and 5 temperature sensors`, or, when the device
  names an EC that is not there, `EC: the firmware names FRMWC004, but
  its memory map does not say EC at port 0xe00; it is not used`. Its
  commands are one at a time, under a sleeping lock.
- **`/kernel/class/hwmon`** gains `cros_ec`, last, with Linux's files for
  each fan and sensor the memory map has; a sensor's label is the name
  the EC gives it.

## Consequences

- `sensors` (11e.5) shows the Framework Desktop's fans and its board's
  sensors as lm-sensors does on Linux.
- The crate already sets fan duties and curves, LEDs and the ARGB
  colors; how programs ask for them, through a capability, comes next
  (11e.4b), with the commands that use them.
- QEMU has no such EC. A new boot test, `framework-ec`, gives QEMU an SSDT
  with a `FRMWC004` device: the kernel must find it and, reading the
  empty bus at 0xe00, leave it alone. The rest is the host tests', and
  the Framework Desktop's runs.
- Not yet: the Microchip MEC ECs of Framework's Intel laptops, whose
  memory map and packet are behind the MEC's EMI ports and whose firmware
  shares them under an AML mutex; the EC's events (host events, MKBP);
  the battery and charge limits Framework's laptops keep in the EC;
  updating the EC's firmware.
