/*
 * SPDX-FileCopyrightText: 2026 The Dukovo contributors
 * SPDX-License-Identifier: Apache-2.0 OR MIT
 *
 * The SSDT the `framework-ec` boot test gives QEMU (-acpitable): the ACPI
 * device that names a Framework machine's embedded controller, which QEMU
 * does not have behind it, so that the kernel finds the device, checks
 * the EC's memory map at port 0xe00 and leaves the EC alone.
 * crosec.aml is made from it by ACPICA's compiler:
 *
 *     iasl -p crosec crosec.asl
 */
DefinitionBlock ("crosec.aml", "SSDT", 2, "DUKOVO", "CROSEC", 1)
{
    Scope (\_SB)
    {
        Device (CREC)
        {
            Name (_HID, "FRMWC004")
            Name (_UID, 1)
            Name (_STA, 0x0F)
        }
    }
}
