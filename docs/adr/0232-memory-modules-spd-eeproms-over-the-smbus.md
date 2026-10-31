<!--
SPDX-FileCopyrightText: 2026 The Dukovo contributors
SPDX-License-Identifier: Apache-2.0 OR MIT
-->

# 0232. Memory modules' SPD EEPROMs are read over the SMBus, when first opened

- Status: Accepted
- Date: 2026-10-31

## Context

Phase 11e.5b, its second half. The SMBIOS tables (ADR 0231) say what the
firmware thinks of the memory, and much firmware leaves the maker, part
number and speed out. The module's own word is in its SPD EEPROM (Serial
Presence Detect), on the chipset's SMBus at 50h to 57h, one a slot,
which the kernel drives (ADR 0206): its kind, form, size and timings, its
maker, date, serial number and part number, laid out by JEDEC (JESD21-C
Annex K for DDR3, Annex L for DDR4, JESD400-5 for DDR5).

- The EEPROM depends on the memory's kind. DDR3 and older have a plain
  256-byte EEPROM. DDR4 has an EE1004 (JEDEC's EE1004-v): 512 bytes in
  two pages, which a Send Byte to 36h or 37h picks for every module on
  the bus at once; some modules take it without acknowledging, and a
  Receive Byte at 36h is acknowledged on page 0 alone. DDR5 has an
  SPD5118 hub (JESD300-5), whose MR0 and MR1 read 51h and 18h: 1024 bytes
  in eight pages of 128, which its register MR11 picks.
- Linux's `i2c_register_spd` (GPL-2.0) chooses by the memory kind the
  SMBIOS tables give, only for one kind across the filled slots and up to
  eight slots, and probes the addresses from 50h on with a Receive Byte,
  which writes nothing to an EEPROM, until as many answer as slots are
  filled. Each module is then `/sys/bus/i2c/devices/0-0050`, with `name`
  (`spd`, `ee1004` or `spd5118`) and `eeprom`, its bytes, which
  `decode-dimms` (i2c-tools, GPL-2.0) reads and decodes.
- A byte takes one transaction, about half a millisecond on the bus, so
  a DDR5 module's whole EEPROM takes over half a second. ADR 0206 ran
  the kernel's transactions at boot only, under a spin lock with the CPU
  waiting, and left a sleeping lock and the controller's interrupt to
  the first user at run time.

## Decision

- **A new crate, `radix-spd`**, with no code of Linux's or i2c-tools':
  - The EEPROM a memory kind has (SMBIOS type 17's numbers: DDR to DDR3
    and LPDDR to LPDDR3 the plain one, DDR4 and LPDDR4 the EE1004, DDR5
    and LPDDR5 the SPD5118), and the reads of each over a `Bus` of the
    four SMBus transactions it takes. An EE1004's page is picked, and
    read back if the pick is not acknowledged, and is left on page 0,
    where modules start; an SPD5118 is read only with MR11 set for
    one-byte offsets, and is left on the page it was on.
  - The decoding of DDR3's, DDR4's and DDR5's layouts: the memory kind
    (byte 2), the form, the size (with a 3DS stack's dies), the speed
    grade from the shortest clock period, the maker from JEP106's codes
    (a short list of module and chip makers; others are named by their
    bank and code), the date, serial number, part number and the CRC-16
    checksums.
- **The SMBus's transactions may sleep.** The controller's lock in the
  x86-64 layer is a mutex, whose waiters sleep, and a transaction polls
  the controller every 250 us with a wait the kernel gives it, asleep
  where the caller may sleep. The HAL gains `smbus_transfer`, for one
  transaction, and the SMBus driver Send Byte and Receive Byte. The
  firmware's AML, which runs in threads that may sleep, still takes the
  controller over as before. The controller's interrupt is not used:
  Intel's is a legacy PCI interrupt, which the kernel does not route,
  and Linux polls the FCH's too.
- **The kernel finds the EEPROMs at boot as Linux does**, by the SMBIOS
  tables, logs them (`SMBus: the DDR4 memory modules' SPD EEPROMs
  (ee1004) at 0x50, 0x52`), or says why it reads none: a kind with no
  EEPROM it reads (`SMBus: RAM memory (kind 0x07) has no SPD EEPROM the
  kernel reads`), kinds mixed, more than eight slots, or none that
  answers, as on the Framework Desktop's soldered LPDDR5X.
- **`/kernel/bus/i2c/devices/0-00<address>`** has Linux's `name` and
  `eeprom`, for anyone to read, as Linux's are. The EEPROM is read whole
  the first time its file is opened, not at boot, which it would slow by
  a second or more on a machine with four modules, and kept; an EEPROM
  that cannot be read is an I/O error and a warning, and is tried again
  at the next open. The kernel keeps 8 KiB for the eight it can find.
- **hwinfo**'s memory section adds a line for each module's SPD data,
  after the SMBIOS memory devices, as the slot each module is in is not
  known: for example `SPD at 0x50: 8 GiB DDR4 SO-DIMM, 2666 MT/s,
  Samsung M471A1K43CB1-CTD, made in week 12 of 2019`, `blank`, or
  `unreadable`; the serial number with `-s`, and a checksum that does not
  match.

## Consequences

- `decode-dimms` and other Linux tools that read the `eeprom` files work
  on Dukovo's paths too.
- The `smbus` boot test gives QEMU an SMBIOS memory device of DDR4, so
  the kernel reads QEMU's EEPROM at 50h as an EE1004, two of QEMU's DDC
  devices at 36h and 37h taking its page selects; QEMU has no EE1004 or
  SPD5118, and its EEPROMs are zeros, so the reads of each kind, the
  page logic and the decoding of real layouts are the host tests'. The
  `smbios` test's QEMU memory, of kind RAM, has none. The Dell's DDR4
  SO-DIMMs are read on the hardware runs.
- A transaction is no longer a busy wait: SPD reads at run time leave
  the CPU to other threads between polls.
- Not yet: an SPD5118 on Intel chipsets whose SPD Write Disable bit
  blocks writes to 50h to 57h (its page cannot be picked, so its read
  fails), I2C block reads (32 bytes a transaction), the temperature
  sensors on DDR4 and DDR5 modules (Linux's `jc42` and `spd5118`
  hwmon), and the decoding of LPDDR layouts.
