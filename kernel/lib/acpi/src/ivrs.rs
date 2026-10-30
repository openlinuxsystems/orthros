// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! The I/O Virtualization Reporting Structure (signature `IVRS`), which
//! describes an AMD machine's IOMMUs: where each one's registers are, its
//! own PCI function, which device IDs it translates, and which memory the
//! firmware keeps for devices' own use (the *AMD I/O Virtualization
//! Technology (IOMMU) Specification*, revision 3.10, chapter 5).
//!
//! A device ID is a PCI function's bus, device and function, the
//! requester ID its DMA carries.

use crate::{Error, Sdt, u16_at, u32_at, u64_at};

/// The parsed fixed part of the IVRS; [`Ivrs::iommus`] and
/// [`Ivrs::memory`] walk its blocks.
#[derive(Clone, Copy, Debug)]
pub struct Ivrs<'a> {
    /// The IVinfo field: the machine's address sizes and what its IOMMUs
    /// share.
    pub info: u32,
    blocks: &'a [u8],
}

/// An IOMMU (an IVHD block): its registers, its own PCI function and the
/// devices it translates.
#[derive(Clone, Copy, Debug)]
pub struct Iommu<'a> {
    /// The block's type: 0x10, 0x11 or 0x40.
    pub kind: u8,
    /// Its flags: HtTunEn, PassPW, ResPassPW, Isoc, IotlbSup, Coherent,
    /// PrefSup and PPRSup, from bit 0.
    pub flags: u8,
    /// The device ID of the IOMMU's own PCI function.
    pub device_id: u16,
    /// Where its capability block is in that function's configuration
    /// space.
    pub capability: u16,
    /// The physical address of its registers.
    pub registers: u64,
    /// The PCI segment its devices are on.
    pub segment: u16,
    /// The IOMMU info field: its MSI message number and unit ID.
    pub info: u16,
    /// A copy of its extended feature register, in a block of type 0x11
    /// or 0x40.
    pub features: Option<u64>,
    entries: &'a [u8],
}

/// Devices an IOMMU's block names.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Devices {
    /// Every device ID.
    All,
    /// The device IDs from `first` to `last`, whose requests carry
    /// `alias` if they carry another.
    Range {
        /// The first.
        first: u16,
        /// The last.
        last: u16,
        /// The device ID their requests carry instead of their own.
        alias: Option<u16>,
    },
    /// An I/O APIC or HPET that sends interrupts through the IOMMU.
    Special {
        /// 1 for an I/O APIC, 2 for an HPET.
        variety: u8,
        /// Its I/O APIC ID or HPET number.
        handle: u8,
        /// The device ID its interrupts carry.
        id: u16,
    },
    /// A device of the ACPI namespace, by the device ID it uses.
    Namespace(u16),
}

/// Memory the firmware keeps for some devices' DMA (an IVMD block): they
/// must still reach it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Reserved {
    /// The device IDs it is for: `None` for every one.
    pub devices: Option<(u16, u16)>,
    /// The PCI segment of those devices.
    pub segment: u16,
    /// Its flags: Unity, IR, IW and ExclusionRange, from bit 0.
    pub flags: u8,
    /// Its first byte's physical address.
    pub base: u64,
    /// Its length in bytes.
    pub len: u64,
}

impl Reserved {
    /// The devices may read it.
    pub fn readable(&self) -> bool {
        self.flags & 2 != 0
    }

    /// The devices may write it.
    pub fn writable(&self) -> bool {
        self.flags & 4 != 0
    }

    /// It is an exclusion range: the devices reach it untranslated, to
    /// read and to write.
    pub fn excluded(&self) -> bool {
        self.flags & 8 != 0
    }
}

/// An IVHD block's type is one this parser reads.
fn is_iommu(kind: u8) -> bool {
    matches!(kind, 0x10 | 0x11 | 0x40)
}

impl<'a> Ivrs<'a> {
    /// The table's bytes after the header must hold the 12 fixed bytes.
    pub fn parse(sdt: &Sdt<'a>) -> Result<Ivrs<'a>, Error> {
        if &sdt.signature != b"IVRS" {
            return Err(Error::BadSignature);
        }
        let body = sdt.body();
        let info = u32_at(body, 0).ok_or(Error::TooShort)?;
        let blocks = body.get(12..).ok_or(Error::TooShort)?;
        Ok(Ivrs { info, blocks })
    }

    /// The widest physical address the machine's DMA can reach, in bits.
    pub fn physical_address_bits(&self) -> u8 {
        ((self.info >> 8) & 0x7f) as u8
    }

    /// Its blocks: (type, the whole block, its 4-byte header included).
    fn blocks(&self) -> impl Iterator<Item = (u8, &'a [u8])> + 'a {
        let mut rest = self.blocks;
        core::iter::from_fn(move || {
            let kind = *rest.first()?;
            let len = usize::from(u16_at(rest, 2)?);
            if len < 4 || len > rest.len() {
                return None;
            }
            let (this, after) = rest.split_at(len);
            rest = after;
            Some((kind, this))
        })
    }

    /// Every IOMMU, each from the block of the newest type that describes
    /// it: firmware lists an IOMMU once for each type it writes.
    pub fn iommus(&self) -> impl Iterator<Item = Iommu<'a>> + 'a {
        let all = *self;
        self.blocks().filter(|&(kind, _)| is_iommu(kind)).filter_map(move |(kind, bytes)| {
            let iommu = Iommu::parse(kind, bytes)?;
            let newer = all
                .blocks()
                .filter(|&(other, _)| is_iommu(other) && other > kind)
                .filter_map(|(other, bytes)| Iommu::parse(other, bytes))
                .any(|other| other.device_id == iommu.device_id && other.segment == iommu.segment);
            (!newer).then_some(iommu)
        })
    }

    /// The memory the firmware keeps for devices.
    pub fn memory(&self) -> impl Iterator<Item = Reserved> + 'a {
        self.blocks().filter_map(|(kind, bytes)| {
            let devices = match kind {
                0x20 => None,
                0x21 => {
                    let id = u16_at(bytes, 4)?;
                    Some((id, id))
                }
                0x22 => Some((u16_at(bytes, 4)?, u16_at(bytes, 6)?)),
                _ => return None,
            };
            Some(Reserved {
                devices,
                segment: u16_at(bytes, 8)?,
                flags: bytes[1],
                base: u64_at(bytes, 16)?,
                len: u64_at(bytes, 24)?,
            })
        })
    }
}

impl<'a> Iommu<'a> {
    fn parse(kind: u8, bytes: &'a [u8]) -> Option<Iommu<'a>> {
        let fixed = if kind == 0x10 { 24 } else { 40 };
        Some(Iommu {
            kind,
            flags: *bytes.get(1)?,
            device_id: u16_at(bytes, 4)?,
            capability: u16_at(bytes, 6)?,
            registers: u64_at(bytes, 8)?,
            segment: u16_at(bytes, 16)?,
            info: u16_at(bytes, 18)?,
            features: if kind == 0x10 { None } else { Some(u64_at(bytes, 24)?) },
            entries: bytes.get(fixed..)?,
        })
    }

    /// The devices it translates, as its device entries name them, in
    /// order. An entry of a kind the parser does not know, or a range left
    /// open, ends the walk.
    pub fn devices(&self) -> impl Iterator<Item = Devices> + 'a {
        let mut rest = self.entries;
        core::iter::from_fn(move || {
            loop {
                let kind = *rest.first()?;
                let len = match kind {
                    0..=63 => 4,
                    64..=127 => 8,
                    0xf0 => 22 + usize::from(*rest.get(21)?),
                    _ => return None,
                };
                let entry = rest.get(..len)?;
                rest = &rest[len..];
                let id = u16_at(entry, 1)?;
                let found = match kind {
                    1 => Devices::All,
                    2 => Devices::Range { first: id, last: id, alias: None },
                    0x42 => Devices::Range { first: id, last: id, alias: Some(u16_at(entry, 5)?) },
                    0x46 => Devices::Range { first: id, last: id, alias: None },
                    3 | 0x43 | 0x47 => {
                        // The range ends at the next entry, of type 4.
                        let end = rest.get(..4).filter(|end| end[0] == 4)?;
                        rest = &rest[4..];
                        let alias = if kind == 0x43 { Some(u16_at(entry, 5)?) } else { None };
                        Devices::Range { first: id, last: u16_at(end, 1)?, alias }
                    }
                    0x48 => Devices::Special {
                        variety: entry[7],
                        handle: entry[4],
                        id: u16_at(entry, 5)?,
                    },
                    0xf0 => Devices::Namespace(id),
                    // Padding, and kinds of a known length this parser
                    // has no use for.
                    _ => continue,
                };
                return Some(found);
            }
        })
    }

    /// Whether it translates the device `id`, and the device ID its
    /// requests then carry.
    pub fn translates(&self, id: u16) -> Option<u16> {
        self.devices().find_map(|devices| match devices {
            Devices::All => Some(id),
            Devices::Range { first, last, alias } if (first..=last).contains(&id) => {
                Some(alias.unwrap_or(id))
            }
            _ => None,
        })
    }

    /// The highest device ID it translates.
    pub fn last_device(&self) -> u16 {
        self.devices()
            .map(|devices| match devices {
                Devices::All => u16::MAX,
                Devices::Range { last, alias, .. } => last.max(alias.unwrap_or(0)),
                Devices::Special { id, .. } | Devices::Namespace(id) => id,
            })
            .max()
            .unwrap_or(0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::table;

    fn iommu(kind: u8, device_id: u16, registers: u64, entries: &[u8]) -> Vec<u8> {
        let fixed = if kind == 0x10 { 24 } else { 40 };
        let mut bytes = vec![kind, 0x20];
        bytes.extend_from_slice(&((fixed + entries.len()) as u16).to_le_bytes());
        bytes.extend_from_slice(&device_id.to_le_bytes());
        bytes.extend_from_slice(&0x40u16.to_le_bytes());
        bytes.extend_from_slice(&registers.to_le_bytes());
        bytes.extend_from_slice(&0u16.to_le_bytes());
        bytes.extend_from_slice(&0u16.to_le_bytes());
        bytes.extend_from_slice(&0u32.to_le_bytes());
        if kind != 0x10 {
            bytes.extend_from_slice(&0x2_0000_0044u64.to_le_bytes());
            bytes.extend_from_slice(&0u64.to_le_bytes());
        }
        bytes.extend_from_slice(entries);
        bytes
    }

    fn memory(kind: u8, flags: u8, first: u16, last: u16, base: u64, len: u64) -> Vec<u8> {
        let mut bytes = vec![kind, flags];
        bytes.extend_from_slice(&32u16.to_le_bytes());
        bytes.extend_from_slice(&first.to_le_bytes());
        bytes.extend_from_slice(&last.to_le_bytes());
        // The segment, then reserved bytes.
        bytes.extend_from_slice(&[0; 8]);
        bytes.extend_from_slice(&base.to_le_bytes());
        bytes.extend_from_slice(&len.to_le_bytes());
        bytes
    }

    /// An IVRS as a desktop's firmware writes one: one IOMMU, in a block of
    /// type 0x10 and again of type 0x11, its devices in ranges, with a
    /// device behind a PCIe-to-PCI bridge, the I/O APIC and HPET, and
    /// memory kept for the USB controllers.
    fn desktop() -> Vec<u8> {
        let mut entries = Vec::new();
        // Select 00:00.0, then 00:01.0 to 00:07.7.
        entries.extend_from_slice(&[2, 0x00, 0x00, 0]);
        entries.extend_from_slice(&[3, 0x08, 0x00, 0, 4, 0x3f, 0x00, 0]);
        // Padding.
        entries.extend_from_slice(&[0, 0, 0, 0]);
        // 05:00.0 to 05:1f.7 carry the bridge's ID, 04:00.0.
        entries.extend_from_slice(&[0x43, 0x00, 0x05, 0, 0, 0x00, 0x04, 0]);
        entries.extend_from_slice(&[4, 0xff, 0x05, 0]);
        // Extended select of 06:00.0.
        entries.extend_from_slice(&[0x46, 0x00, 0x06, 0, 0, 0, 0, 0]);
        // The I/O APIC 33 as 00:14.0, the HPET 0 as 00:14.0.
        entries.extend_from_slice(&[0x48, 0, 0, 0xd7, 33, 0xa0, 0x00, 1]);
        entries.extend_from_slice(&[0x48, 0, 0, 0, 0, 0xa0, 0x00, 2]);
        // An ACPI device with a 4-byte UID.
        let mut hid = vec![0xf0, 0xa5, 0x00, 0x40];
        hid.extend_from_slice(b"AMDI0020");
        hid.extend_from_slice(&[0; 8]);
        hid.extend_from_slice(&[2, 4]);
        hid.extend_from_slice(b"ID00");
        entries.extend_from_slice(&hid);
        let mut body = (40u32 << 8 | 48 << 15 | 1).to_le_bytes().to_vec();
        body.extend_from_slice(&[0; 8]);
        body.extend(iommu(0x10, 0x0002, 0xfd20_0000, &entries[..12]));
        body.extend(iommu(0x11, 0x0002, 0xfd20_0000, &entries));
        body.extend(memory(0x21, 0b111, 0x0098, 0, 0x9d00_0000, 0x1_0000));
        body.extend(memory(0x22, 0b011, 0x00a0, 0x00a7, 0x9e00_0000, 0x2000));
        body.extend(memory(0x20, 0b1001, 0, 0, 0xa000_0000, 0x1000));
        table(*b"IVRS", 2, &body)
    }

    #[test]
    fn the_newest_block_of_each_iommu_is_read() {
        let bytes = desktop();
        let ivrs = Ivrs::parse(&Sdt::parse(0x1000, &bytes).unwrap()).unwrap();
        assert_eq!(ivrs.physical_address_bits(), 40);
        let iommus: Vec<Iommu<'_>> = ivrs.iommus().collect();
        assert_eq!(iommus.len(), 1);
        let iommu = iommus[0];
        assert_eq!((iommu.kind, iommu.device_id, iommu.capability), (0x11, 2, 0x40));
        assert_eq!((iommu.registers, iommu.segment, iommu.flags), (0xfd20_0000, 0, 0x20));
        assert_eq!(iommu.features, Some(0x2_0000_0044));
        let devices: Vec<Devices> = iommu.devices().collect();
        assert_eq!(
            devices,
            [
                Devices::Range { first: 0, last: 0, alias: None },
                Devices::Range { first: 8, last: 0x3f, alias: None },
                Devices::Range { first: 0x500, last: 0x5ff, alias: Some(0x400) },
                Devices::Range { first: 0x600, last: 0x600, alias: None },
                Devices::Special { variety: 1, handle: 33, id: 0xa0 },
                Devices::Special { variety: 2, handle: 0, id: 0xa0 },
                Devices::Namespace(0xa5),
            ]
        );
        assert_eq!(iommu.translates(0x10), Some(0x10));
        assert_eq!(iommu.translates(0x512), Some(0x400));
        assert_eq!(iommu.translates(0x40), None);
        assert_eq!(iommu.last_device(), 0x600);
    }

    #[test]
    fn memory_kept_for_devices_is_found() {
        let bytes = desktop();
        let ivrs = Ivrs::parse(&Sdt::parse(0x1000, &bytes).unwrap()).unwrap();
        let kept: Vec<Reserved> = ivrs.memory().collect();
        assert_eq!(kept.len(), 3);
        assert_eq!(kept[0].devices, Some((0x98, 0x98)));
        assert!(kept[0].readable() && kept[0].writable());
        assert_eq!((kept[0].base, kept[0].len), (0x9d00_0000, 0x1_0000));
        assert_eq!(kept[1].devices, Some((0xa0, 0xa7)));
        assert!(kept[1].readable() && !kept[1].writable());
        assert_eq!(kept[2].devices, None);
        assert!(kept[2].excluded() && !kept[0].excluded());
        assert!(kept.iter().all(|kept| kept.segment == 0));
    }

    #[test]
    fn all_devices_and_damaged_entries() {
        let mut entries = vec![1, 0, 0, 0];
        // A range left open ends the walk.
        entries.extend_from_slice(&[3, 0x00, 0x01, 0, 2, 0x10, 0x00, 0]);
        let mut body = 0u32.to_le_bytes().to_vec();
        body.extend_from_slice(&[0; 8]);
        body.extend(iommu(0x10, 0x0010, 0xfeb8_0000, &entries));
        // A block that claims more bytes than the table has.
        body.extend_from_slice(&[0x10, 0, 200, 0]);
        let bytes = table(*b"IVRS", 2, &body);
        let ivrs = Ivrs::parse(&Sdt::parse(0, &bytes).unwrap()).unwrap();
        let iommus: Vec<Iommu<'_>> = ivrs.iommus().collect();
        assert_eq!(iommus.len(), 1);
        assert_eq!(iommus[0].features, None);
        assert_eq!(iommus[0].devices().collect::<Vec<_>>(), [Devices::All]);
        assert_eq!(iommus[0].translates(0x1234), Some(0x1234));
        assert_eq!(iommus[0].last_device(), u16::MAX);
        let other = table(*b"DMAR", 1, &[0; 12]);
        assert!(Ivrs::parse(&Sdt::parse(0, &other).unwrap()).is_err());
    }
}
