<!--
SPDX-FileCopyrightText: 2026 The Dukovo contributors
SPDX-License-Identifier: Apache-2.0 OR MIT
-->

# 0208. Programs set the fans, LEDs and ARGB header through the platform capability

- Status: Accepted
- Date: 2026-10-31

## Context

Phase 11e, the second half of its fourth part. The kernel drives
Framework's embedded controller (ADR 0207) and reads its fans and
sensors, and `radix-crosec` already sends the commands that change them:
a fan's fixed duty and giving it back, the Framework Desktop's fan
curves, the LEDs, and the ARGB header's colors. The roadmap asks for
fan curves, the power LED and the ARGB header; nothing let a program
ask for them.

- No device file or capability kind fits. The EC is not a disk, a
  network port or a power control; Linux reaches it through
  `/dev/cros_ec`, whose ioctl passes any host command through, and the
  ChromiumOS `ectool` builds its commands in user space.
- Passing any command through would let a program flash the EC's
  firmware, reboot it or change its charge limits. The EC checks little
  of what it is sent: Framework's `EC_CMD_FAN_CONFIGURATION` copies a
  curve as it is, then divides by the difference of its two
  temperatures and indexes its temperature table with the curve's
  sensor. A fixed duty lasts until the machine restarts or resumes; a
  curve, until the EC itself restarts.
- Not every EC has every command. A Chromebook's has fan duties and
  LEDs but neither Framework's curves nor an ARGB header.
  `EC_CMD_GET_CMD_VERSIONS` says whether the EC takes a command, at
  which versions.
- The permission list of the design doc names `admin.hardware`, among
  the capabilities Spark, authd and the services hand to elevated
  processes (Phases 13b and 16). Until then, Spark is what hands out
  capabilities.

## Decision

- **A new capability kind, `platform`** (19), with the read and control
  rights, and its interface `platform` (radix-platform(2)): `describe`
  (the controls the EC has, its fans, sensors and LEDs), `set_fan_duty`
  and `auto_fan`, `fan_curve` and `set_fan_curve`, `led`, `set_led` and
  `auto_led`, and `set_rgb`, nine methods and no more. Asking needs the
  read right, changing anything the control right.
- **The kernel checks each method's arguments** before anything reaches
  the EC: a duty at most 100%; a curve whose temperatures rise, whose
  duties do not fall and stay at most 100%, and whose sensor is one the
  EC has, keeping the duty the EC gives the fan now; a brightness at
  most 255; a fan or LED the EC has; up to 128 ARGB LEDs, copied in from
  the caller a request at a time. A method fails with `not_supported`
  where there is no EC or it lacks the method's commands, which the
  kernel asks it once, at boot.
- **One capability, given to Spark** as its fifth extra capability, as
  the network's was (ADR 0183). A service whose file says
  `admin.hardware = yes` (spark(8)) gets a copy with both rights in a
  slot of its own, `SLOT_PLATFORM` (48), which lasts through `fork` and
  `exec` so that the programs it starts have it too. `consoled` asks for
  it, so the console's shell can run `ectool`.
- **The kernel logs each fan a program takes from the EC, and gives
  back**, once for each change, with the program's name: `` EC: `ectool`
  took fan 0 from the EC, at 50% ``. Its boot line says what the EC
  takes: `..., with 3 fans, 5 temperature sensors and 2 LEDs; it takes
  fan duties, fan curves, LEDs and ARGB colors`.
- **`ectool`** (ectool(8)), a new command in systools, takes the
  ChromiumOS `ectool`'s words and output for what Dukovo has:
  `fanduty`, `autofanctrl`, `pwmgetfanrpm`, `pwmgetnumfans`, `led` and
  `rgbkbd`; and adds `fancurve`, for Framework's curves, whose keys
  (`min_temp`, `max_temp`, `min_duty`, `max_duty`, `sensor` and
  `always_on`) shorten the names of the fields of Framework's command.
  It reads the fans' speeds and the sensors' names from
  `/kernel/class/hwmon`.

## Consequences

- On the Framework Desktop, `ectool fanduty 0 40`, `ectool fancurve 1
  min_temp=45 max_temp=85` or `ectool led power off` work as they do on
  ChromeOS, in the console or a service that asks for the permission.
  Scripts written for ChromeOS's `ectool` keep working for these words.
- Nothing else reaches the EC: no host command passes through, so no
  program can update its firmware, reboot it or change the battery's
  limits through this capability.
- The capability is all or nothing for now: a service with it can stop
  the fans. Phase 13b's authd and Phase 16's permission prompts decide
  who gets `admin.hardware`; the log line keeps a stopped fan from
  being a mystery, and the machine's restart, or `autofanctrl`, gives
  the fans back.
- QEMU has no such EC. The `framework-ec` boot test now runs `ectool` at
  the console, whose shell holds the capability, and must see it say that
  the kernel drives no embedded controller; `posix`'s administration
  test checks its errors and help. The rest is the host tests', against
  the simulated Desktop EC and a simulated Chromebook's, and the
  Framework Desktop's runs.
- Not yet: the EC's other commands (charge limits, keyboard backlight,
  the MEC ECs of Framework's Intel laptops), a settings page for the
  fans and the ARGB header, and finer rights (LEDs without fans).
