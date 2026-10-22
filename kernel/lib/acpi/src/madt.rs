// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! The Multiple APIC Description Table (signature `APIC`).

use crate::{Error, Sdt, u16_at, u32_at, u64_at};

/// The parsed fixed part of the MADT; [`Madt::entries`] walks the rest.
#[derive(Clone, Copy, Debug)]
pub struct Madt<'a> {
    /// Physical address of the local APIC registers, after any 64-bit
    /// override entry.
    pub local_apic_address: u64,
    /// Whether the machine also has dual 8259 PICs (PCAT_COMPAT), which the
    /// kernel must mask.
    pub has_8259: bool,
    entries: &'a [u8],
}

/// One MADT entry the kernel understands.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MadtEntry {
    /// A CPU with an xAPIC (8-bit APIC ID).
    LocalApic {
        /// ACPI processor UID.
        processor_uid: u32,
        /// APIC ID.
        apic_id: u32,
        /// The CPU can be used now.
        enabled: bool,
        /// The CPU is disabled but could be brought online later.
        online_capable: bool,
    },
    /// An I/O APIC.
    IoApic {
        /// I/O APIC ID.
        id: u8,
        /// Physical address of its registers.
        address: u64,
        /// First global system interrupt it handles.
        gsi_base: u32,
    },
    /// An ISA interrupt wired to a different global system interrupt.
    InterruptSourceOverride {
        /// ISA IRQ number.
        source: u8,
        /// Global system interrupt it arrives on.
        gsi: u32,
        /// Polarity and trigger mode flags (MPS INTI flags).
        flags: u16,
    },
    /// A global system interrupt wired to NMI.
    NmiSource {
        /// MPS INTI flags.
        flags: u16,
        /// The interrupt.
        gsi: u32,
    },
    /// Which local APIC LINT pin carries NMI, for one CPU or all
    /// (`processor_uid == u32::MAX`).
    LocalApicNmi {
        /// ACPI processor UID, or `u32::MAX` for all.
        processor_uid: u32,
        /// MPS INTI flags.
        flags: u16,
        /// LINT0 or LINT1.
        lint: u8,
    },
}

/// Bit 0 of the processor flags.
const ENABLED: u32 = 1;
/// Bit 1 of the processor flags (ACPI 6.3).
const ONLINE_CAPABLE: u32 = 2;

impl<'a> Madt<'a> {
    /// Parses an `APIC` table.
    pub fn parse(sdt: &Sdt<'a>) -> Result<Madt<'a>, Error> {
        if &sdt.signature != b"APIC" {
            return Err(Error::BadSignature);
        }
        let body = sdt.body();
        let mut madt = Madt {
            local_apic_address: u64::from(u32_at(body, 0).ok_or(Error::TooShort)?),
            has_8259: u32_at(body, 4).ok_or(Error::TooShort)? & 1 != 0,
            entries: &body[8..],
        };
        for (kind, data) in madt.raw_entries() {
            if kind == 5
                && let Some(address) = u64_at(data, 4)
            {
                madt.local_apic_address = address;
            }
        }
        Ok(madt)
    }

    /// Every entry as (type, bytes including the two-byte entry header).
    fn raw_entries(&self) -> impl Iterator<Item = (u8, &'a [u8])> + 'a {
        let mut rest = self.entries;
        core::iter::from_fn(move || {
            let kind = *rest.first()?;
            let length = usize::from(*rest.get(1)?);
            if length < 2 || length > rest.len() {
                rest = &[];
                return None;
            }
            let (entry, tail) = rest.split_at(length);
            rest = tail;
            Some((kind, entry))
        })
    }

    /// The entries the kernel understands, in table order. Unknown or
    /// malformed entries are skipped.
    pub fn entries(&self) -> impl Iterator<Item = MadtEntry> + 'a {
        self.raw_entries().filter_map(|(kind, e)| match kind {
            0 => {
                let flags = u32_at(e, 4)?;
                Some(MadtEntry::LocalApic {
                    processor_uid: u32::from(*e.get(2)?),
                    apic_id: u32::from(*e.get(3)?),
                    enabled: flags & ENABLED != 0,
                    online_capable: flags & ONLINE_CAPABLE != 0,
                })
            }
            1 => Some(MadtEntry::IoApic {
                id: *e.get(2)?,
                address: u64::from(u32_at(e, 4)?),
                gsi_base: u32_at(e, 8)?,
            }),
            2 => Some(MadtEntry::InterruptSourceOverride {
                source: *e.get(3)?,
                gsi: u32_at(e, 4)?,
                flags: u16_at(e, 8)?,
            }),
            3 => Some(MadtEntry::NmiSource { flags: u16_at(e, 2)?, gsi: u32_at(e, 4)? }),
            4 => {
                let uid = *e.get(2)?;
                Some(MadtEntry::LocalApicNmi {
                    processor_uid: if uid == 0xff { u32::MAX } else { u32::from(uid) },
                    flags: u16_at(e, 3)?,
                    lint: *e.get(5)?,
                })
            }
            9 => {
                let flags = u32_at(e, 8)?;
                Some(MadtEntry::LocalApic {
                    processor_uid: u32_at(e, 12)?,
                    apic_id: u32_at(e, 4)?,
                    enabled: flags & ENABLED != 0,
                    online_capable: flags & ONLINE_CAPABLE != 0,
                })
            }
            0xa => Some(MadtEntry::LocalApicNmi {
                processor_uid: u32_at(e, 4)?,
                flags: u16_at(e, 2)?,
                lint: *e.get(8)?,
            }),
            _ => None,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::table;

    fn madt_bytes() -> Vec<u8> {
        let mut body = Vec::new();
        body.extend_from_slice(&0xfee0_0000u32.to_le_bytes());
        body.extend_from_slice(&1u32.to_le_bytes());
        // CPU 0 (enabled), CPU 1 (online capable only).
        body.extend_from_slice(&[0, 8, 0, 0, 1, 0, 0, 0]);
        body.extend_from_slice(&[0, 8, 1, 2, 2, 0, 0, 0]);
        // I/O APIC 4 at 0xfec00000, GSI base 0.
        body.extend_from_slice(&[1, 12, 4, 0]);
        body.extend_from_slice(&0xfec0_0000u32.to_le_bytes());
        body.extend_from_slice(&0u32.to_le_bytes());
        // ISA IRQ 0 -> GSI 2.
        body.extend_from_slice(&[2, 10, 0, 0]);
        body.extend_from_slice(&2u32.to_le_bytes());
        body.extend_from_slice(&0u16.to_le_bytes());
        // LINT1 is NMI on all CPUs.
        body.extend_from_slice(&[4, 6, 0xff, 5, 0, 1]);
        // An x2APIC CPU with ID 300.
        body.extend_from_slice(&[9, 16, 0, 0]);
        body.extend_from_slice(&300u32.to_le_bytes());
        body.extend_from_slice(&1u32.to_le_bytes());
        body.extend_from_slice(&7u32.to_le_bytes());
        // An unknown entry, then a 64-bit APIC address override.
        body.extend_from_slice(&[0x7f, 4, 0, 0]);
        body.extend_from_slice(&[5, 12, 0, 0]);
        body.extend_from_slice(&0x1_fee0_0000u64.to_le_bytes());
        table(b"APIC", 4, &body)
    }

    #[test]
    fn parses_entries() {
        let bytes = madt_bytes();
        let sdt = Sdt::parse(0x1000, &bytes).unwrap();
        let madt = Madt::parse(&sdt).unwrap();
        assert!(madt.has_8259);
        assert_eq!(madt.local_apic_address, 0x1_fee0_0000);
        let entries: Vec<_> = madt.entries().collect();
        assert_eq!(
            entries,
            [
                MadtEntry::LocalApic {
                    processor_uid: 0,
                    apic_id: 0,
                    enabled: true,
                    online_capable: false
                },
                MadtEntry::LocalApic {
                    processor_uid: 1,
                    apic_id: 2,
                    enabled: false,
                    online_capable: true
                },
                MadtEntry::IoApic { id: 4, address: 0xfec0_0000, gsi_base: 0 },
                MadtEntry::InterruptSourceOverride { source: 0, gsi: 2, flags: 0 },
                MadtEntry::LocalApicNmi { processor_uid: u32::MAX, flags: 5, lint: 1 },
                MadtEntry::LocalApic {
                    processor_uid: 7,
                    apic_id: 300,
                    enabled: true,
                    online_capable: false
                },
            ]
        );
    }

    #[test]
    fn stops_at_a_malformed_entry() {
        let mut body = Vec::new();
        body.extend_from_slice(&0xfee0_0000u32.to_le_bytes());
        body.extend_from_slice(&0u32.to_le_bytes());
        body.extend_from_slice(&[0, 8, 0, 0, 1, 0, 0, 0]);
        body.extend_from_slice(&[1, 0, 9, 9]);
        let bytes = table(b"APIC", 4, &body);
        let sdt = Sdt::parse(0, &bytes).unwrap();
        let madt = Madt::parse(&sdt).unwrap();
        assert_eq!(madt.entries().count(), 1);
        assert!(!madt.has_8259);
    }
}
