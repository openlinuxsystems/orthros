/*
 * SPDX-FileCopyrightText: 2026 The Dukovo contributors
 * SPDX-License-Identifier: Apache-2.0 OR MIT
 *
 * The SSDT the `i2c` boot test gives QEMU (-acpitable), with QEMU's
 * pc-testdev (-device pc-testdev), whose 64 KiB of memory at 0xff000000
 * hold the registers of two DesignWare I2C controllers: an Intel Sunrise
 * Point one in the firmware's ACPI mode (INT3443) at 0xff000000, with
 * LPSS's registers after the controller's, and an AMD one (AMDI0010) at
 * 0xff001000. Their _INI methods give the registers a DesignWare
 * controller reads as at reset: its type, its version (1.21a) and its
 * FIFOs' depths, and LPSS's capabilities (an I2C function with a DMA
 * engine).
 *
 * The Intel controller has the firmware's counts for fast mode (FMCN) and
 * a _PS0, and a touchpad on its bus at 2Ch that allows 400 kHz; a device
 * that is not there, which allows only 100 kHz, must not slow the bus.
 * The AMD controller's bus has a sensor at 48h that allows 100 kHz, from
 * outside its scope. As memory does not answer as a controller, the
 * Intel one's says a stop was seen, so that a transfer that only writes
 * ends at once, and the AMD one's that a transfer was aborted because no
 * device acknowledged its address. Each controller's REGS method reads
 * back the registers `selftest=i2c` checks. i2c.aml is made from it by
 * ACPICA's compiler:
 *
 *     iasl -p i2c i2c.asl
 */
DefinitionBlock ("i2c.aml", "SSDT", 2, "DUKOVO", "I2C", 1)
{
    Scope (\_SB)
    {
        Device (I2C1)
        {
            Name (_HID, "INT3443")
            Name (_UID, 1)
            Name (_CRS, ResourceTemplate ()
            {
                Memory32Fixed (ReadWrite, 0xFF000000, 0x00001000)
            })
            Name (PWR0, 0)

            Method (_PS0)
            {
                PWR0 = 1
            }

            Method (_PS3)
            {
                PWR0 = 3
            }

            Method (FMCN)
            {
                Return (Package () { 0x88, 0xA8, 0x1E })
            }

            OperationRegion (MMIO, SystemMemory, 0xFF000000, 0x1000)
            Field (MMIO, DWordAcc, NoLock, Preserve)
            {
                ICON, 32,
                ITAR, 32,
                Offset (0x10),
                DCMD, 32,
                Offset (0x1C),
                FSHC, 32,
                FSLC, 32,
                Offset (0x30),
                MASK, 32,
                RAWS, 32,
                Offset (0x7C),
                HOLD, 32,
                Offset (0xF4),
                PAR1, 32,
                VERS, 32,
                TYPE, 32,
                Offset (0x204),
                RSTS, 32,
                Offset (0x240),
                RMPL, 32,
                RMPH, 32,
                Offset (0x2FC),
                CAPS, 32,
            }

            // 64-byte FIFOs; every interrupt unmasked, as at reset; a stop
            // seen.
            Method (_INI)
            {
                TYPE = 0x44570140
                VERS = 0x3132312A
                PAR1 = 0x003F3F00
                MASK = 0x08FF
                RAWS = 0x0200
                CAPS = 0
            }

            Method (REGS, 0, Serialized)
            {
                Local0 = Package (10) {}
                Local0[0] = ICON
                Local0[1] = ITAR
                Local0[2] = DCMD
                Local0[3] = FSHC
                Local0[4] = FSLC
                Local0[5] = HOLD
                Local0[6] = MASK
                Local0[7] = RSTS
                Local0[8] = RMPL
                Local0[9] = RMPH
                Return (Local0)
            }

            Device (TPD0)
            {
                Name (_HID, "DUKV0001")
                Name (_CID, "PNP0C50")
                Name (_CRS, ResourceTemplate ()
                {
                    I2cSerialBusV2 (0x002C, ControllerInitiated, 400000,
                        AddressingMode7Bit, "\\_SB.I2C1", 0x00,
                        ResourceConsumer, , Exclusive, )
                })
            }

            Device (GONE)
            {
                Name (_HID, "DUKV0003")
                Name (_CRS, ResourceTemplate ()
                {
                    I2cSerialBusV2 (0x0010, ControllerInitiated, 100000,
                        AddressingMode7Bit, "\\_SB.I2C1", 0x00,
                        ResourceConsumer, , Exclusive, )
                })

                Method (_STA)
                {
                    Return (0)
                }
            }
        }

        Device (I2CB)
        {
            Name (_HID, "AMDI0010")
            Name (_UID, 2)
            Name (_CRS, ResourceTemplate ()
            {
                Memory32Fixed (ReadWrite, 0xFF001000, 0x00001000)
            })

            OperationRegion (MMIO, SystemMemory, 0xFF001000, 0x100)
            Field (MMIO, DWordAcc, NoLock, Preserve)
            {
                ICON, 32,
                Offset (0x14),
                SSHC, 32,
                SSLC, 32,
                Offset (0x34),
                RAWS, 32,
                Offset (0x7C),
                HOLD, 32,
                ABRT, 32,
                Offset (0xF4),
                PAR1, 32,
                VERS, 32,
                TYPE, 32,
            }

            // 16-byte FIFOs; an SDA hold of 5 cycles; a transfer aborted
            // for want of an acknowledgement of its address.
            Method (_INI)
            {
                TYPE = 0x44570140
                VERS = 0x3132312A
                PAR1 = 0x000F0F00
                HOLD = 5
                RAWS = 0x40
                ABRT = 1
            }

            Method (REGS, 0, Serialized)
            {
                Local0 = Package (4) {}
                Local0[0] = ICON
                Local0[1] = SSHC
                Local0[2] = SSLC
                Local0[3] = HOLD
                Return (Local0)
            }
        }

        Device (SNSR)
        {
            Name (_HID, "DUKV0002")
            Name (_CRS, ResourceTemplate ()
            {
                I2cSerialBusV2 (0x0048, ControllerInitiated, 100000,
                    AddressingMode7Bit, "\\_SB.I2CB", 0x00,
                    ResourceConsumer, , Exclusive, )
            })
        }
    }
}
