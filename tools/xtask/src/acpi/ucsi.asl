/*
 * SPDX-FileCopyrightText: 2026 The Dukovo contributors
 * SPDX-License-Identifier: Apache-2.0 OR MIT
 *
 * The SSDT the `usb-c` boot test gives QEMU (-acpitable), with QEMU's
 * pc-testdev (-device pc-testdev), whose memory at 0xff000000 holds the
 * mailbox of a UCSI 1.2 device (USBC000). Its _DSM's function 1 runs a
 * simulated PPM on what the kernel wrote to CONTROL: a laptop's two USB-C
 * ports, a 65 W charger on port 1 that gives 20 V at 3 A by USB Power
 * Delivery and charges the battery, and nothing on port 2. Once the
 * kernel has read port 2 at boot and acknowledged it, a phone is plugged
 * into port 2, which the machine powers at USB Type-C's 1.5 A: the PPM
 * shows the change in the CCI and notifies the device (0x80), as a
 * laptop's firmware does. ucsi.aml is made from it by ACPICA's compiler:
 *
 *     iasl -p ucsi ucsi.asl
 */
DefinitionBlock ("ucsi.aml", "SSDT", 2, "DUKOVO", "UCSI", 1)
{
    Scope (\_SB)
    {
        Device (UBTC)
        {
            Name (_HID, "USBC000")
            Name (_CID, "PNP0CA0")
            Name (_UID, Zero)
            Name (_CRS, ResourceTemplate ()
            {
                Memory32Fixed (ReadWrite, 0xFF000000, 0x00001000)
            })

            OperationRegion (MBOX, SystemMemory, 0xFF000000, 0x30)
            Field (MBOX, ByteAcc, NoLock, Preserve)
            {
                VER,    16,
                Offset (0x04),
                CCI,    32,
                /*
                 * CONTROL, in two halves: QEMU's DSDT has 32-bit integers,
                 * and every field the PPM reads is in the low one.
                 */
                CTL,    32,
                CTLH,   32,
                MSGI,   128
            }

            /* The connector whose change is not acknowledged yet. */
            Name (CHG, Zero)
            /*
             * Port 2: 0 empty, 1 empty and read at boot, 2 the phone
             * plugged in.
             */
            Name (P2ST, Zero)

            Method (_INI)
            {
                VER = 0x0120
            }

            /* A command completed, with Arg0 bytes in MESSAGE IN. */
            Method (DONE, 1)
            {
                CCI = (0x80000000 | (Arg0 << 8) | (CHG << 1))
            }

            Method (RUN, 0, Serialized)
            {
                Local0 = CTL
                Local1 = (Local0 & 0xFF)
                Local2 = ((Local0 >> 16) & 0x7F)
                If (Local1 == 0x01)
                {
                    /* PPM_RESET */
                    CHG = Zero
                    P2ST = Zero
                    VER = 0x0120
                    CCI = 0x08000000
                }
                ElseIf (Local1 == 0x04)
                {
                    /* ACK_CC_CI */
                    If (Local0 & 0x00010000)
                    {
                        CHG = Zero
                    }

                    If ((Local0 & 0x00020000) && (P2ST == One))
                    {
                        P2ST = 2
                        CHG = 2
                        Notify (UBTC, 0x80)
                    }

                    CCI = (0x20000000 | (CHG << 1))
                }
                ElseIf (Local1 == 0x05)
                {
                    /* SET_NOTIFICATION_ENABLE */
                    DONE (Zero)
                }
                ElseIf (Local1 == 0x06)
                {
                    /*
                     * GET_CAPABILITY: battery charging and Power Delivery,
                     * 2 connectors, Power Delivery 3.0, USB Type-C 2.0.
                     */
                    MSGI = Buffer (0x10)
                    {
                        0x06, 0x00, 0x00, 0x00, 0x02, 0x00, 0x00, 0x00,
                        0x00, 0x00, 0x20, 0x01, 0x00, 0x03, 0x00, 0x02
                    }
                    DONE (0x10)
                }
                ElseIf (Local1 == 0x07)
                {
                    /*
                     * GET_CONNECTOR_CAPABILITY: dual role, USB 2 and 3,
                     * alternate modes; gives and takes power.
                     */
                    MSGI = Buffer (0x10)
                    {
                        0xE4, 0x03
                    }
                    DONE (0x04)
                }
                ElseIf (Local1 == 0x12)
                {
                    /* GET_CONNECTOR_STATUS */
                    If (Local2 == One)
                    {
                        /*
                         * Connected, Power Delivery, the charger a DFP;
                         * its RDO for object 2 at 3 A; charging.
                         */
                        MSGI = Buffer (0x10)
                        {
                            0x00, 0x00, 0x0B, 0x20, 0x2C, 0xB1, 0x04, 0x20,
                            0x01
                        }
                    }
                    ElseIf (P2ST == 2)
                    {
                        /*
                         * Connected (changed), USB Type-C at 1.5 A, the
                         * machine the provider, the phone a UFP with USB.
                         */
                        MSGI = Buffer (0x10)
                        {
                            0x00, 0x40, 0x3C, 0x40
                        }
                    }
                    Else
                    {
                        MSGI = Buffer (0x10) {}
                        If (Local2 == 2)
                        {
                            P2ST = One
                        }
                    }

                    DONE (0x09)
                }
                ElseIf (Local1 == 0x10)
                {
                    /*
                     * GET_PDOS: the charger's source objects, 5 V at 3 A,
                     * 20 V at 3.25 A, 9 V at 3 A, and a programmable 3.3
                     * to 21 V at 3 A.
                     */
                    If (Local2 == One)
                    {
                        MSGI = Buffer (0x10)
                        {
                            0x2C, 0x91, 0x01, 0x00, 0x45, 0x41, 0x06, 0x00,
                            0x2C, 0xD1, 0x02, 0x00, 0x3C, 0x21, 0xA4, 0xC1
                        }
                        DONE (0x10)
                    }
                    Else
                    {
                        DONE (Zero)
                    }
                }
                Else
                {
                    /* Completed, not supported. */
                    CCI = 0x82000000
                }
            }

            Method (_DSM, 4, Serialized)
            {
                If ((Arg0 == ToUUID ("6f8398c2-7ca4-11e4-ad36-631042b5008f")))
                {
                    If ((Arg2 == Zero))
                    {
                        Return (Buffer (One)
                        {
                            0x07
                        })
                    }

                    If ((Arg2 == One))
                    {
                        RUN ()
                    }

                    Return (Zero)
                }

                Return (Buffer (One)
                {
                    0x00
                })
            }
        }
    }
}
