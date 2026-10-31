/*
 * SPDX-FileCopyrightText: 2026 The Dukovo contributors
 * SPDX-License-Identifier: Apache-2.0 OR MIT
 *
 * The SSDT the `gpio` boot test gives QEMU (-acpitable), with QEMU's
 * pc-testdev (-device pc-testdev), whose 64 KiB of memory at 0xff000000
 * hold the registers of two GPIO controllers: an AMD one at 0xff000000,
 * its interrupt the test device's ISA interrupt 5, and an Intel Sunrise
 * Point-LP one with its three communities at 0xff001000, 0xff002000 and
 * 0xff003000, its interrupt ISA interrupt 6, whose _INI sets their PADBAR
 * and gives pad 53 to the operating system's driver.
 *
 * The firmware takes events on three of their pins, as their _AEI say: an
 * edge on the AMD controller's pin 9 runs _E09, a level on its pin 42
 * _EVT, and a level on the Intel one's pad 53 (35h) _L35. Each records the
 * pin in LAST and counts itself in RUNS. The controllers' FIRE methods
 * play the hardware for `selftest=gpio`: they set a pin's interrupt
 * status (and the AMD controller's summary bit for its group of four),
 * then pulse the controller's interrupt; RPIN and RENA read back the
 * registers it then checks. gpio.aml is made from it by ACPICA's
 * compiler:
 *
 *     iasl -p gpio gpio.asl
 */
DefinitionBlock ("gpio.aml", "SSDT", 2, "DUKOVO", "GPIO", 1)
{
    Scope (\_SB)
    {
        Name (LAST, 0)
        Name (RUNS, 0)

        OperationRegion (TIRQ, SystemIO, 0x2005, 2)
        Field (TIRQ, ByteAcc, NoLock, Preserve)
        {
            IRQ5, 8,
            IRQ6, 8,
        }

        Device (GPIO)
        {
            Name (_HID, "AMDI0030")
            Name (_UID, 0)
            Name (_CRS, ResourceTemplate ()
            {
                Interrupt (ResourceConsumer, Edge, ActiveHigh, Exclusive, ,, ) {5}
                Memory32Fixed (ReadWrite, 0xFF000000, 0x00000400)
            })
            Name (_AEI, ResourceTemplate ()
            {
                GpioInt (Edge, ActiveLow, Exclusive, PullNone, 0x0000,
                    "\\_SB.GPIO", 0x00, ResourceConsumer, ,) {9}
                GpioInt (Level, ActiveHigh, Exclusive, PullNone, 0x0000,
                    "\\_SB.GPIO", 0x00, ResourceConsumer, ,) {42}
            })

            Method (_E09)
            {
                LAST = 9
                RUNS++
            }

            Method (_EVT, 1)
            {
                LAST = Arg0
                RUNS++
            }

            OperationRegion (REGS, SystemMemory, 0xFF000000, 0x0400)
            Field (REGS, DWordAcc, NoLock, Preserve)
            {
                Offset (0x24),
                PIN9, 32,
                Offset (0xA8),
                PN42, 32,
                Offset (0x2F8),
                SUM0, 32,
            }

            // Pin 9's group is 2, pin 42's 10.
            Method (FIRE, 1, Serialized)
            {
                If (Arg0 == 9)
                {
                    PIN9 |= 0x10000000
                    SUM0 = 0x04
                }
                Else
                {
                    PN42 |= 0x10000000
                    SUM0 = 0x0400
                }

                IRQ5 = 1
                IRQ5 = 0
            }

            // Pin 42's register, for the self-test: a field read into a
            // local is its value, where the field itself is not.
            Method (RPIN)
            {
                Local0 = PN42
                Return (Local0)
            }
        }

        Device (GPI0)
        {
            Name (_HID, "INT344B")
            Name (_UID, 1)
            Name (_CRS, ResourceTemplate ()
            {
                Memory32Fixed (ReadWrite, 0xFF001000, 0x00001000)
                Memory32Fixed (ReadWrite, 0xFF002000, 0x00001000)
                Memory32Fixed (ReadWrite, 0xFF003000, 0x00001000)
                Interrupt (ResourceConsumer, Edge, ActiveHigh, Exclusive, ,, ) {6}
            })
            Name (_AEI, ResourceTemplate ()
            {
                GpioInt (Level, ActiveLow, Exclusive, PullUp, 0x0000,
                    "\\_SB.GPI0", 0x00, ResourceConsumer, ,) {53}
            })

            Method (_L35)
            {
                LAST = 0x35
                RUNS++
            }

            OperationRegion (REGS, SystemMemory, 0xFF001000, 0x3000)
            Field (REGS, DWordAcc, NoLock, Preserve)
            {
                Offset (0x000C),
                BAR0, 32,
                Offset (0x100C),
                BAR1, 32,
                Offset (0x10D0),
                OWN1, 32,
                Offset (0x1100),
                STS1, 32,
                Offset (0x1120),
                ENA1, 32,
                Offset (0x200C),
                BAR2, 32,
            }

            // Pads' registers at 400h; pad 53 is community 1's sixth.
            Method (_INI)
            {
                BAR0 = 0x0400
                BAR1 = 0x0400
                BAR2 = 0x0400
                OWN1 = 0x20
            }

            Method (FIRE, 0, Serialized)
            {
                STS1 |= 0x20
                IRQ6 = 1
                IRQ6 = 0
            }

            // Community 1's first group's enables, for the self-test.
            Method (RENA)
            {
                Local0 = ENA1
                Return (Local0)
            }
        }
    }
}
