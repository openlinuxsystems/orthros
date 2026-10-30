/*
 * SPDX-FileCopyrightText: 2026 The Dukovo contributors
 * SPDX-License-Identifier: Apache-2.0 OR MIT
 *
 * The SSDT the `acpi-thermal` boot test gives QEMU (-acpitable), which has
 * no thermal zone of its own: a zone without a polling period, looked at
 * when it says it changed. It stays at 45 °C for its first five reads,
 * the kernel's two as it boots (finding the zone, then its thread's first
 * look) and the console's three (the zone's file, hwmon's, and the zone's
 * again). The fifth says it changed, and from then on it warms 5 °C a
 * read, saying so each time, past its passive (60 °C) and hot (70 °C)
 * trip points to its critical one (80 °C), where it stays.
 * thermal.aml is made from it by ACPICA's compiler:
 *
 *     iasl -p thermal thermal.asl
 */
DefinitionBlock ("thermal.aml", "SSDT", 2, "DUKOVO", "THERMAL", 1)
{
    Scope (\_TZ)
    {
        ThermalZone (TZ00)
        {
            Name (READ, 0)
            Name (TEMP, 3182)

            Method (_TMP, 0, Serialized)
            {
                READ++
                If ((READ > 5) && (TEMP < 3532))
                {
                    TEMP += 50
                }
                If ((READ >= 5) && (TEMP < 3532))
                {
                    Notify (TZ00, 0x80)
                }
                Return (TEMP)
            }

            Name (_PSV, 3332)
            Name (_HOT, 3432)
            Name (_CRT, 3532)
            Name (_TC1, 2)
            Name (_TC2, 5)
            Name (_TSP, 10)
        }
    }
}
