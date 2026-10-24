// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! The Fixed ACPI Description Table (signature `FACP`).

use crate::{Error, Sdt, u16_at, u32_at, u64_at};

/// An ACPI Generic Address Structure: where a register lives.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct GenericAddress {
    /// Address space: 0 memory, 1 I/O port, 2 PCI configuration space.
    pub space: u8,
    /// Register width in bits.
    pub bit_width: u8,
    /// Offset of the register within the address, in bits.
    pub bit_offset: u8,
    /// Access size: 1 byte, 2 word, 3 dword, 4 qword, 0 undefined.
    pub access_size: u8,
    /// The address within the space.
    pub address: u64,
}

impl GenericAddress {
    /// System memory.
    pub const MEMORY: u8 = 0;
    /// Port I/O.
    pub const IO: u8 = 1;
    /// PCI configuration space.
    pub const PCI_CONFIG: u8 = 2;

    /// Parses the 12-byte structure.
    pub fn parse(bytes: &[u8]) -> Option<GenericAddress> {
        Some(GenericAddress {
            space: *bytes.first()?,
            bit_width: *bytes.get(1)?,
            bit_offset: *bytes.get(2)?,
            access_size: *bytes.get(3)?,
            address: u64_at(bytes, 4)?,
        })
    }
}

/// The FADT fields the kernel uses (Phase 1, the century register since
/// Phase 7, and what powering off needs since Phase 9).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Fadt {
    /// The ACPI power-management timer: (address, 32-bit counter). The
    /// timer runs at [`Fadt::PM_TIMER_HZ`].
    pub pm_timer: Option<(GenericAddress, bool)>,
    /// The register to write to reset the machine, and the value to write.
    pub reset: Option<(GenericAddress, u8)>,
    /// Whether an i8042 keyboard controller is present (IAPC_BOOT_ARCH).
    /// Many laptops leave this clear although they have one.
    pub has_8042: bool,
    /// Whether the machine has no ACPI fixed hardware (HW_REDUCED_ACPI).
    pub hardware_reduced: bool,
    /// Physical address of the DSDT.
    pub dsdt: u64,
    /// The CMOS RAM index of the real-time clock's century register, or 0
    /// if it has none.
    pub century: u8,
    /// The PM1a control register, which powering off writes (Phase 9).
    pub pm1a_control: Option<GenericAddress>,
    /// The PM1b control register, if the machine has a second one.
    pub pm1b_control: Option<GenericAddress>,
    /// The port that takes the machine from its firmware's mode into ACPI
    /// mode, and the value to write there; `None` if it is in ACPI mode
    /// for good.
    pub acpi_enable: Option<(u16, u8)>,
    /// The sleep control register, through which a hardware-reduced
    /// machine sleeps and powers off (ACPI 5 and later).
    pub sleep_control: Option<GenericAddress>,
    /// The sleep status register that goes with it.
    pub sleep_status: Option<GenericAddress>,
}

const FLAG_TMR_VAL_EXT: u32 = 1 << 8;
const FLAG_RESET_REG_SUP: u32 = 1 << 10;
const FLAG_HW_REDUCED_ACPI: u32 = 1 << 20;
const BOOT_ARCH_8042: u16 = 1 << 1;

impl Fadt {
    /// Frequency of the ACPI power-management timer.
    pub const PM_TIMER_HZ: u64 = 3_579_545;

    /// Parses a `FACP` table. Fields past the end of an older, shorter
    /// table are treated as absent.
    pub fn parse(sdt: &Sdt<'_>) -> Result<Fadt, Error> {
        if &sdt.signature != b"FACP" {
            return Err(Error::BadSignature);
        }
        let t = sdt.bytes;
        let flags = u32_at(t, 112).unwrap_or(0);
        let gas = |offset: usize| {
            t.get(offset..offset + 12)
                .and_then(GenericAddress::parse)
                .filter(|gas| gas.address != 0)
        };

        let extended = flags & FLAG_TMR_VAL_EXT != 0;
        let legacy_timer = u32_at(t, 76).filter(|&port| port != 0).map(|port| GenericAddress {
            space: GenericAddress::IO,
            bit_width: 32,
            bit_offset: 0,
            access_size: 3,
            address: u64::from(port),
        });
        let timer_length_ok = t.get(91).is_some_and(|&len| len == 4);
        let pm_timer = gas(208)
            .or(legacy_timer)
            .filter(|_| timer_length_ok)
            .map(|address| (address, extended));

        let reset =
            (flags & FLAG_RESET_REG_SUP != 0).then(|| gas(116).zip(t.get(128).copied())).flatten();

        // A PM1 control block: the extended address, or else the 32-bit
        // port, two bytes wide (PM1_CNT_LEN).
        let control_block = |extended: usize, legacy: usize| {
            gas(extended).or_else(|| {
                u32_at(t, legacy).filter(|&port| port != 0).map(|port| GenericAddress {
                    space: GenericAddress::IO,
                    bit_width: 16,
                    bit_offset: 0,
                    access_size: 2,
                    address: u64::from(port),
                })
            })
        };

        Ok(Fadt {
            pm_timer,
            reset,
            has_8042: u16_at(t, 109).is_some_and(|arch| arch & BOOT_ARCH_8042 != 0),
            hardware_reduced: flags & FLAG_HW_REDUCED_ACPI != 0,
            dsdt: u64_at(t, 140)
                .filter(|&x| x != 0)
                .or_else(|| u32_at(t, 40).map(u64::from))
                .unwrap_or(0),
            century: t.get(108).copied().unwrap_or(0),
            pm1a_control: control_block(172, 64),
            pm1b_control: control_block(184, 68),
            acpi_enable: u32_at(t, 48)
                .filter(|&port| port != 0 && port <= 0xffff)
                .zip(t.get(52).copied().filter(|&value| value != 0))
                .map(|(port, value)| (port as u16, value)),
            sleep_control: gas(244),
            sleep_status: gas(256),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::table;

    fn fadt_body(len: usize) -> Vec<u8> {
        let mut t = vec![0u8; len];
        let body = |offset: usize| offset - 36;
        t[body(40)..body(44)].copy_from_slice(&0x7fe0_0000u32.to_le_bytes());
        t[body(76)..body(80)].copy_from_slice(&0x608u32.to_le_bytes());
        t[body(48)..body(52)].copy_from_slice(&0xb2u32.to_le_bytes());
        t[body(52)] = 0xf1;
        t[body(64)..body(68)].copy_from_slice(&0x604u32.to_le_bytes());
        t[body(91)] = 4;
        t[body(109)..body(111)].copy_from_slice(&BOOT_ARCH_8042.to_le_bytes());
        let flags = FLAG_TMR_VAL_EXT | FLAG_RESET_REG_SUP;
        t[body(112)..body(116)].copy_from_slice(&flags.to_le_bytes());
        if len >= body(129) {
            t[body(116)] = GenericAddress::IO;
            t[body(117)] = 8;
            t[body(120)..body(128)].copy_from_slice(&0xcf9u64.to_le_bytes());
            t[body(128)] = 0x06;
        }
        t
    }

    #[test]
    fn reads_timer_reset_and_8042() {
        let bytes = table(b"FACP", 6, &fadt_body(276 - 36));
        let fadt = Fadt::parse(&Sdt::parse(0, &bytes).unwrap()).unwrap();
        let (timer, extended) = fadt.pm_timer.unwrap();
        assert_eq!((timer.space, timer.address, extended), (GenericAddress::IO, 0x608, true));
        let (reset, value) = fadt.reset.unwrap();
        assert_eq!((reset.space, reset.address, value), (GenericAddress::IO, 0xcf9, 0x06));
        assert!(fadt.has_8042);
        assert!(!fadt.hardware_reduced);
        assert_eq!(fadt.dsdt, 0x7fe0_0000);
        let control = fadt.pm1a_control.unwrap();
        assert_eq!(
            (control.space, control.address, control.bit_width),
            (GenericAddress::IO, 0x604, 16)
        );
        assert_eq!(fadt.pm1b_control, None);
        assert_eq!(fadt.acpi_enable, Some((0xb2, 0xf1)));
    }

    #[test]
    fn short_tables_lack_newer_fields() {
        let bytes = table(b"FACP", 1, &fadt_body(116 - 36));
        let fadt = Fadt::parse(&Sdt::parse(0, &bytes).unwrap()).unwrap();
        assert!(fadt.pm_timer.is_some());
        assert_eq!(fadt.reset, None);
        let bytes = table(b"FACP", 6, &fadt_body(276 - 36));
        assert_eq!(Fadt::parse(&Sdt::parse(0, &bytes).unwrap()).unwrap().sleep_control, None);
    }

    #[test]
    fn reads_the_sleep_registers_of_acpi_5() {
        let mut body = fadt_body(276 - 36);
        body[112 - 36..116 - 36]
            .copy_from_slice(&(FLAG_HW_REDUCED_ACPI | FLAG_RESET_REG_SUP).to_le_bytes());
        for (at, port) in [(244 - 36, 0x620u64), (256 - 36, 0x621)] {
            body[at] = GenericAddress::IO;
            body[at + 1] = 8;
            body[at + 3] = 1;
            body[at + 4..at + 12].copy_from_slice(&port.to_le_bytes());
        }
        let bytes = table(b"FACP", 6, &body);
        let fadt = Fadt::parse(&Sdt::parse(0, &bytes).unwrap()).unwrap();
        assert!(fadt.hardware_reduced);
        let control = fadt.sleep_control.unwrap();
        assert_eq!(
            (control.space, control.bit_width, control.address),
            (GenericAddress::IO, 8, 0x620)
        );
        assert_eq!(fadt.sleep_status.unwrap().address, 0x621);
    }
}
