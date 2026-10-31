<!--
SPDX-FileCopyrightText: 2026 The Dukovo contributors
SPDX-License-Identifier: Apache-2.0 OR MIT
-->

# 0206. The chipset's SMBus controller

- Status: Accepted
- Date: 2026-10-31

## Context

Phase 11e, its third part. The roadmap asks for the FCH's SMBus, the
Framework Desktop's, and the administration command set's `hwinfo`
(11e.5) wants what the memory modules' SPD EEPROMs say of them, which
only the SMBus reaches. The kernel had no SMBus driver.

- The SMBus is the chipset's slow two-wire bus, a kind of I2C, to the
  memory modules' SPD EEPROMs (at addresses 50h to 57h), sensor chips
  and the like. Intel's controller and that of AMD's Fusion Controller
  Hub (FCH) both keep the register layout of Intel's PIIX4 at an I/O
  base: the host status, control, command, the target's address and two
  data bytes. A transaction is set up in them, then started; the status
  says when it is done, or that no device answered, another master took
  the bus or it failed.
- Intel's SMBus function (00:1f.3, or 00:1f.4 from the 100 Series PCHs
  on) has its base in BAR 4, decoded once HOSTC's HST_EN is set, which
  firmware may leave clear. Its INUSE_STS bit is a semaphore that
  firmware's SMI handler shares (the ICH9 and PCH datasheets).
- The FCH's (1022:790b from revision 49h, at 00:14.0) is enabled, and its
  base given, in the FCH's power management registers (DecodeEn), which
  Zen's FCH (from revision 51h) puts in memory at 0xfed80300 rather than
  behind the index and data ports 0xcd6 and 0xcd7. One controller drives
  four ports, which DecodeEn picks, and the FCH's microcontroller may use
  the bus too, which a host semaphore in the controller's register 8
  keeps apart (AMD's BKDGs and PPRs; Linux's `i2c-piix4`, GPL-2.0, is
  the reference for the port and the semaphore).
- The firmware's AML may drive the controller itself through its ports:
  many of Intel's DSDTs have an SMBus region and methods that use it.
  Linux's `i2c-i801` installs a handler for the AML's I/O, and from the
  first time the AML reaches the controller's ports leaves it to the
  firmware.
- Linux names the adapters `SMBus I801 adapter at <base>` and `SMBus
  PIIX4 adapter port 0 at <base>`, in `/sys/bus/i2c/devices/i2c-<n>/name`.

## Decision

- **`radix-smbus`**, a crate with no `unsafe` code whose tests run on the
  host against a simulated ICH9 and FCH, finds the controller on bus 0
  and drives it:
  - Intel's: the first SMBus-class function at 00:1f.3 or 00:1f.4, with
    I/O decoding and HST_EN turned on if firmware left them off; INUSE_STS
    is given back after each transaction, and a transaction that does
    not end is killed.
  - The FCH's, from revision 49h on: its power management registers
    through memory from revision 51h on, else the index and data ports;
    each transaction takes the host semaphore (the bus is busy if the
    microcontroller keeps it), picks port 0, where the memory modules
    are, and puts back the port firmware had picked.
  - Byte and word reads and byte writes, what SPD EEPROMs and sensor
    chips need. A transaction is polled for up to 200 ms, and its error
    says whether no device answered, the bus was busy or taken, it
    failed, or it did not end.
- **The x86-64 layer** reaches the controller's ports and the FCH's
  through the watchdog's rules
  ([ADR 0167](0167-newer-intel-chipsets-and-amds-fch-have-their-watchdog-too.md):
  device memory that is not RAM,
  and no port below 0x400 but the FCH's index and data), keeps it to one
  transaction at a time under a spin lock, and finds it after the PCI
  scan and the AML's start, logging it and the memory modules' EEPROMs
  that answer at 50h to 57h, for example `SMBus: Intel's controller
  (8086:2930) at port 0x700 (SMBus I801 adapter at 0700); memory
  modules' EEPROMs answer at 0x50, 0x51, ...`. A controller there but
  unusable is a warning, for example `SMBus: the FCH's SMBus controller
  is off or out of reach; it is not used`.
- **The firmware comes first**: every I/O port the AML reads or writes
  passes the x86-64 layer, and the first that is one of the controller's
  waits for the kernel's transaction in flight and leaves the controller
  to the firmware from then on, as Linux's `i2c-i801` does, for Intel's
  and the FCH's alike.
- **`/kernel/bus/i2c/devices/i2c-0/name`** names the adapter as Linux
  does.

## Consequences

- `hwinfo` (11e.5) can read the memory modules' SPD EEPROMs, and later
  drivers the sensor chips on the bus.
- A new boot test, `smbus`, finds q35's ICH9 controller at the port
  SeaBIOS gives it and QEMU's eight EEPROMs, and reads the adapter's name
  at the console; under UEFI the same boot finds it at 0x6000. The FCH's
  half is checked by the host tests and on the Framework Desktop's runs,
  the Dell's Intel PCH on its own.
- The kernel's own transactions run under a spin lock with the CPU
  waiting: at boot only, as nothing else uses the bus yet. A sleeping
  lock and the controller's interrupt come with the first user that
  reads at run time.
- An AML access to the controller's ports before the kernel finds it is
  not seen, as on Linux, whose handler is installed with the driver.
- Not yet: Intel's block transfers, host notify and SMBus alerts; the
  FCH's other three ports and its ASF controller; the controllers of
  older AMD chipsets (SB700 to SB900) and of Intel's PIIX4 itself; the
  AMD I2C (DesignWare) and GPIO controllers, which the touchpads of
  later machines need; an `i2c-dev` for programs.
