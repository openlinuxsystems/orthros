/*
 * SPDX-FileCopyrightText: 2026 The Dukovo contributors
 * SPDX-License-Identifier: Apache-2.0 OR MIT
 *
 * The SSDT the `acpi-power` boot test gives QEMU (-acpitable), which has
 * no battery, AC adapter, lid or power button of the namespace of its
 * own: a battery discharging at 9 W, three quarters full, an unplugged
 * adapter, an open lid and a power button. power.aml is made from it by
 * ACPICA's compiler:
 *
 *     iasl -p power power.asl
 */
DefinitionBlock ("power.aml", "SSDT", 2, "DUKOVO", "POWER", 1)
{
    Scope (\_SB)
    {
        Device (BAT0)
        {
            Name (_HID, EisaId ("PNP0C0A"))
            Name (_UID, 1)

            Method (_STA)
            {
                Return (0x1F)
            }

            Method (_BIX)
            {
                Return (Package ()
                {
                    1, 0, 50000, 48000, 1, 11400, 5000, 2000, 42, 95000,
                    0xFFFFFFFF, 0xFFFFFFFF, 0xFFFFFFFF, 0xFFFFFFFF, 10, 10,
                    "DUKOVO TEST", "0042", "LION", "Dukovo", 1
                })
            }

            Method (_BST)
            {
                Return (Package () { 1, 9000, 36000, 12100 })
            }
        }

        Device (ADP1)
        {
            Name (_HID, "ACPI0003")

            Method (_PSR)
            {
                Return (0)
            }
        }

        Device (LID0)
        {
            Name (_HID, EisaId ("PNP0C0D"))

            Method (_LID)
            {
                Return (1)
            }
        }

        Device (PWRB)
        {
            Name (_HID, EisaId ("PNP0C0C"))
        }
    }
}
