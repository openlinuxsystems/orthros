/*
 * SPDX-FileCopyrightText: 2026 The Dukovo contributors
 * SPDX-License-Identifier: Apache-2.0 OR MIT
 *
 * The DSDT of the simulated machine the crate's tests run uACPI on
 * (src/tests.rs). dsdt.aml is made from it by ACPICA's compiler:
 *
 *     iasl -p dsdt dsdt.asl
 *
 * Its methods write what they did to I/O ports the test reads back. Its
 * embedded controller keeps a battery, an AC adapter and a lid; its GPIO
 * controller has events.
 */
DefinitionBlock ("dsdt.aml", "DSDT", 2, "DUKOVO", "TESTDSDT", 1)
{
    OperationRegion (DBG, SystemIO, 0x700, 4)
    Field (DBG, ByteAcc, NoLock, Preserve)
    {
        INIT, 8,    // _INI writes 0x42
        PTS, 8,     // _PTS writes the sleep state
        GPE, 8,     // _L00 counts its runs
        EVT, 8,     // GPI0's event methods write the pin
    }

    Name (_S5, Package () { 5, 5, 0, 0 })

    Method (_PTS, 1)
    {
        PTS = Arg0
    }

    Mutex (MTX0, 0)

    Scope (_GPE)
    {
        Method (_L00)
        {
            GPE += 1
        }
    }

    Scope (_SB)
    {
        Device (DEV0)
        {
            Name (_HID, "DKV0001")

            Method (_STA)
            {
                Return (0x0F)
            }

            Method (_INI)
            {
                INIT = 0x42
            }
        }

        Device (PCI0)
        {
            Name (_HID, EisaId ("PNP0A08"))
            Name (_CID, EisaId ("PNP0A03"))
            Name (_BBN, 0)

            // The host bridge's function, 00:00.0.
            Device (HB00)
            {
                Name (_ADR, 0)

                OperationRegion (CFG, PCI_Config, 0, 0x100)
                Field (CFG, WordAcc, NoLock, Preserve)
                {
                    VID, 16,
                    DID, 16,
                }

                Method (VEND)
                {
                    Return (VID)
                }
            }
        }

        // The embedded controller, at Linux's usual ports, raising GPE 1.
        Device (EC0)
        {
            Name (_HID, EisaId ("PNP0C09"))
            Name (_GPE, 1)
            Name (_CRS, ResourceTemplate ()
            {
                IO (Decode16, 0x62, 0x62, 0, 1)
                IO (Decode16, 0x66, 0x66, 0, 1)
            })

            // _REG(3, 1) once the kernel serves its region.
            Name (REGD, 0)
            Method (_REG, 2)
            {
                If (Arg0 == 3)
                {
                    REGD = Arg1
                }
            }

            OperationRegion (ECR, EmbeddedControl, 0, 0x100)
            Field (ECR, ByteAcc, NoLock, Preserve)
            {
                Offset (0x10),
                ACON, 1,    // the AC adapter is plugged in
                LIDO, 1,    // the lid is open
                    , 6,
                BSTA, 8,    // the battery's state bits
                BRTE, 16,   // its rate
                BREM, 16,   // what it has left
                BVOL, 16,   // its voltage
            }

            Method (_Q01)
            {
                Notify (\_SB.BAT0, 0x80)
            }

            Method (_Q02)
            {
                Notify (\_SB.LID0, 0x80)
            }

            // Stops charging: a write through the controller.
            Method (STOP)
            {
                BSTA = 0
                Return (BSTA)
            }
        }

        Device (BAT0)
        {
            Name (_HID, EisaId ("PNP0C0A"))
            Name (_UID, 0)

            Method (_STA)
            {
                Return (0x1F)
            }

            Name (BIXP, Package ()
            {
                1, 0, 50000, 48000, 1, 11400, 5000, 2000, 42, 95000,
                0xFFFFFFFF, 0xFFFFFFFF, 0xFFFFFFFF, 0xFFFFFFFF, 10, 10,
                "TEST 0001", "1234", "LION", "DUKOVO", 1
            })

            Method (_BIX)
            {
                Return (BIXP)
            }

            Method (_BST, 0, Serialized)
            {
                Name (PKG, Package (4) { 0, 0, 0, 0 })
                PKG [0] = \_SB.EC0.BSTA
                PKG [1] = \_SB.EC0.BRTE
                PKG [2] = \_SB.EC0.BREM
                PKG [3] = \_SB.EC0.BVOL
                Return (PKG)
            }
        }

        Device (ADP1)
        {
            Name (_HID, "ACPI0003")

            Method (_PSR)
            {
                Return (\_SB.EC0.ACON)
            }
        }

        Device (LID0)
        {
            Name (_HID, EisaId ("PNP0C0D"))

            Method (_LID)
            {
                Return (\_SB.EC0.LIDO)
            }
        }

        // A GPIO controller whose pins' events the firmware takes: an
        // edge on pin 9 runs _E09, and pin 0x2A's level _EVT.
        Device (GPI0)
        {
            Name (_HID, "AMDI0030")
            Name (_CRS, ResourceTemplate ()
            {
                Interrupt (ResourceConsumer, Level, ActiveLow, Shared, ,, ) {7}
                Memory32Fixed (ReadWrite, 0xFED81500, 0x00000400)
            })
            Name (_AEI, ResourceTemplate ()
            {
                GpioInt (Edge, ActiveLow, Exclusive, PullNone, 0x0000,
                    "\\_SB.GPI0", 0x00, ResourceConsumer, ,) {9}
                GpioInt (Level, ActiveHigh, Exclusive, PullNone, 0x0000,
                    "\\_SB.GPI0", 0x00, ResourceConsumer, ,) {0x2A}
            })

            Method (_E09)
            {
                EVT = 9
            }

            Method (_EVT, 1)
            {
                EVT = Arg0
            }
        }

        OperationRegion (MEM, SystemMemory, 0xFED00000, 0x10)
        Field (MEM, DWordAcc, NoLock, Preserve)
        {
            REG0, 32,
        }

        Method (MEMR)
        {
            Return (REG0)
        }

        // Takes a mutex ten times, which uACPI makes a semaphore of, and
        // sleeps a millisecond.
        Method (SUM, 0, Serialized)
        {
            Local0 = 0
            Local1 = 1
            While (Local1 <= 10)
            {
                Acquire (MTX0, 0xFFFF)
                Local0 += Local1
                Release (MTX0)
                Local1++
            }

            Sleep (1)
            Return (Local0)
        }
    }
}
