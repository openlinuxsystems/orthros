sensors(1) "Dukovo" "Dukovo Manual"

; SPDX-FileCopyrightText: 2026 The Dukovo contributors
; SPDX-License-Identifier: Apache-2.0 OR MIT

# NAME

sensors - the machine's temperatures and fan speeds

# SYNOPSIS

*sensors* [*-A*] [*-f*] [*-u* | *-j*] [_chip_ ...]

# DESCRIPTION

sensors prints what the kernel's hardware monitors read, from
_/kernel/class/hwmon_ (*radix-sysfs*(5)), as lm-sensors' sensors prints
it on Linux: each chip by lm-sensors' name for it, its adapter, then its
fans and its temperatures, each with its limits and alarms.

```
$ sensors
k10temp-pci-00c3
Adapter: PCI adapter
Tctl:         +45.9°C

cros_ec-isa-0000
Adapter: ISA adapter
fan1:         1190 RPM
temp1:        +36.9°C
```

The chips are those the kernel reads (ADRs 0203, 0205 and 0207):

[[ *Chip*
:- *What it reads*
|  *acpitz-acpi-0*
:  the firmware's thermal zones, with their critical trip points
|  *k10temp-pci-00c3*
:  AMD's processor sensors: *Tctl*, and each core complex die's
|  *coretemp-isa-0000*
:  each of Intel's packages, one chip each: the package's sensor and
   each core's, with their limits
|  *cros\_ec-isa-0000*
:  the fans and sensors of Framework's embedded controller (*ectool*(8))

A _chip_ names the chips to print, as lm-sensors names them; any of its
three parts may be *\**: *coretemp-\**, *\*-isa-\**.

A value the sensor gives none of says N/A; a sensor or fan that says it
failed says FAULT.

# OPTIONS

*-A*, *--no-adapter*
	Leave out each chip's adapter line.

*-f*, *--fahrenheit*
	Temperatures in degrees Fahrenheit.

*-u*
	Each file's value, raw: _temp1\_input: 45.875_.

*-j*
	The values, raw, in JSON.

# EXIT STATUS

0 when a chip was printed; 1 when the kernel reads no sensor (*No
sensors found!*), when no chip has a name given, or for a mistake in the
command line.

# SEE ALSO

*radix-sysfs*(5), *ectool*(8), *watchdog*(8), *systools*(1)
