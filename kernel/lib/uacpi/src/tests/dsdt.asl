/*
 * SPDX-FileCopyrightText: 2026 The Dukovo contributors
 * SPDX-License-Identifier: Apache-2.0 OR MIT
 *
 * The DSDT of the simulated machine the crate's tests run uACPI on
 * (src/tests.rs). dsdt.aml is made from it by ACPICA's compiler:
 *
 *     iasl -p dsdt dsdt.asl
 *
 * Its methods write what they did to I/O ports the test reads back.
 */
DefinitionBlock ("dsdt.aml", "DSDT", 2, "DUKOVO", "TESTDSDT", 1)
{
    OperationRegion (DBG, SystemIO, 0x700, 4)
    Field (DBG, ByteAcc, NoLock, Preserve)
    {
        INIT, 8,    // _INI writes 0x42
        PTS, 8,     // _PTS writes the sleep state
        GPE, 8,     // _L00 counts its runs
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
