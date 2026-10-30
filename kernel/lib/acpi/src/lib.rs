// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Safe parser for the static ACPI tables the Radix kernel reads at boot.
//!
//! Phase 1 needs the tables that describe the CPUs, interrupt controllers
//! and timers: the RSDP and its root table (RSDT or XSDT), the MADT (APIC),
//! the HPET table, the MCFG (PCI Express configuration space) and the FADT
//! (for the ACPI timer, the reset register and legacy devices). Phase 2
//! adds the NUMA topology: the SRAT and the SLIT; Phase 9 the sleep types
//! that powering off needs, from the DSDT's AML; Phase 11 the DMAR,
//! which describes Intel's DMA remapping units. Everything
//! here works on byte slices and is unit-tested on the build host; the
//! kernel supplies a [`Memory`] that maps physical addresses for it. The
//! AML interpreter, which later phases need, is a separate decision (ADR
//! 0013).
//!
//! Reference: ACPI Specification 6.5, chapter 5 "ACPI Software Programming
//! Model".

#![cfg_attr(not(test), no_std)]
#![forbid(unsafe_code)]

mod dmar;
mod fadt;
mod madt;
mod numa;
mod sleep;

pub use dmar::{DeviceScope, Dmar, Reserved, ScopeKind, Unit as DmarUnit};
pub use fadt::{Fadt, GenericAddress, RegisterBlock};
pub use madt::{Madt, MadtEntry};
pub use numa::{Slit, Srat, SratEntry};
pub use sleep::sleep_types;

use core::fmt;

/// Access to physical memory for the parser.
pub trait Memory<'a> {
    /// The `len` bytes at physical address `addr`, or `None` if they cannot
    /// be reached.
    fn bytes(&self, addr: u64, len: usize) -> Option<&'a [u8]>;
}

/// Why a table was rejected.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
    /// The bytes do not start with the expected signature.
    BadSignature,
    /// The checksum does not add up to zero.
    BadChecksum,
    /// The table is shorter than its fixed fields.
    TooShort,
    /// The memory at this physical address could not be mapped.
    Unreachable(u64),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::BadSignature => f.write_str("wrong signature"),
            Error::BadChecksum => f.write_str("checksum mismatch"),
            Error::TooShort => f.write_str("table too short"),
            Error::Unreachable(addr) => write!(f, "cannot map physical address {addr:#x}"),
        }
    }
}

/// Reads a little-endian `u16` at `offset`, if in bounds.
pub(crate) fn u16_at(bytes: &[u8], offset: usize) -> Option<u16> {
    Some(u16::from_le_bytes(bytes.get(offset..offset + 2)?.try_into().ok()?))
}

/// Reads a little-endian `u32` at `offset`, if in bounds.
pub(crate) fn u32_at(bytes: &[u8], offset: usize) -> Option<u32> {
    Some(u32::from_le_bytes(bytes.get(offset..offset + 4)?.try_into().ok()?))
}

/// Reads a little-endian `u64` at `offset`, if in bounds.
pub(crate) fn u64_at(bytes: &[u8], offset: usize) -> Option<u64> {
    Some(u64::from_le_bytes(bytes.get(offset..offset + 8)?.try_into().ok()?))
}

/// Whether the bytes sum to zero modulo 256.
fn checksum_ok(bytes: &[u8]) -> bool {
    bytes.iter().fold(0u8, |sum, &b| sum.wrapping_add(b)) == 0
}

/// The Root System Description Pointer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Rsdp {
    /// ACPI revision: 0 for ACPI 1.0, 2 for ACPI 2.0 and later.
    pub revision: u8,
    /// OEM identifier.
    pub oem_id: [u8; 6],
    /// Physical address of the RSDT.
    pub rsdt_address: u32,
    /// Physical address of the XSDT (ACPI 2.0 and later), or 0.
    pub xsdt_address: u64,
}

impl Rsdp {
    /// Bytes in an ACPI 1.0 RSDP.
    pub const V1_LEN: usize = 20;
    /// Bytes in an ACPI 2.0 RSDP.
    pub const V2_LEN: usize = 36;

    /// Parses and checks an RSDP, such as the copy GRUB passes in the boot
    /// information.
    pub fn parse(bytes: &[u8]) -> Result<Rsdp, Error> {
        let v1 = bytes.get(..Self::V1_LEN).ok_or(Error::TooShort)?;
        if &v1[..8] != b"RSD PTR " {
            return Err(Error::BadSignature);
        }
        if !checksum_ok(v1) {
            return Err(Error::BadChecksum);
        }
        let revision = v1[15];
        let mut rsdp = Rsdp {
            revision,
            oem_id: v1[9..15].try_into().map_err(|_| Error::TooShort)?,
            rsdt_address: u32_at(v1, 16).ok_or(Error::TooShort)?,
            xsdt_address: 0,
        };
        if revision >= 2 {
            let length = u32_at(bytes, 20).ok_or(Error::TooShort)? as usize;
            let v2 = bytes.get(..length.max(Self::V2_LEN)).ok_or(Error::TooShort)?;
            if !checksum_ok(v2) {
                return Err(Error::BadChecksum);
            }
            rsdp.xsdt_address = u64_at(v2, 24).ok_or(Error::TooShort)?;
        }
        Ok(rsdp)
    }

    /// Finds the RSDP by scanning legacy BIOS memory: the first KiB of the
    /// EBDA and 0xE0000 to 0xFFFFF, on 16-byte boundaries. `bios_area` is
    /// that memory as (physical start, bytes).
    pub fn scan(areas: &[(u64, &[u8])]) -> Option<(u64, Rsdp)> {
        areas.iter().find_map(|&(base, bytes)| {
            (0..bytes.len().saturating_sub(Self::V1_LEN)).step_by(16).find_map(|offset| {
                let candidate = &bytes[offset..];
                (candidate.starts_with(b"RSD PTR "))
                    .then(|| Rsdp::parse(candidate).ok())
                    .flatten()
                    .map(|rsdp| (base + offset as u64, rsdp))
            })
        })
    }
}

/// The 36-byte header shared by every system description table.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Sdt<'a> {
    /// Physical address of the table.
    pub address: u64,
    /// Four-character signature, such as `APIC`.
    pub signature: [u8; 4],
    /// Table revision.
    pub revision: u8,
    /// OEM identifier.
    pub oem_id: [u8; 6],
    /// OEM table identifier.
    pub oem_table_id: [u8; 8],
    /// The whole table, header included.
    pub bytes: &'a [u8],
}

impl<'a> Sdt<'a> {
    /// Bytes in the common header.
    pub const HEADER_LEN: usize = 36;

    /// Maps the table at `address`, checks its length and checksum, and
    /// optionally its signature.
    pub fn load(
        memory: &impl Memory<'a>,
        address: u64,
        signature: Option<&[u8; 4]>,
    ) -> Result<Sdt<'a>, Error> {
        let header = memory.bytes(address, Self::HEADER_LEN).ok_or(Error::Unreachable(address))?;
        if signature.is_some_and(|expected| header[..4] != expected[..]) {
            return Err(Error::BadSignature);
        }
        let length = u32_at(header, 4).ok_or(Error::TooShort)? as usize;
        if length < Self::HEADER_LEN {
            return Err(Error::TooShort);
        }
        let bytes = memory.bytes(address, length).ok_or(Error::Unreachable(address))?;
        Self::parse(address, bytes)
    }

    /// Checks a table already in memory.
    pub fn parse(address: u64, bytes: &'a [u8]) -> Result<Sdt<'a>, Error> {
        let length = u32_at(bytes, 4).ok_or(Error::TooShort)? as usize;
        if length < Self::HEADER_LEN || length > bytes.len() {
            return Err(Error::TooShort);
        }
        let bytes = &bytes[..length];
        if !checksum_ok(bytes) {
            return Err(Error::BadChecksum);
        }
        Ok(Sdt {
            address,
            signature: bytes[..4].try_into().map_err(|_| Error::TooShort)?,
            revision: bytes[8],
            oem_id: bytes[10..16].try_into().map_err(|_| Error::TooShort)?,
            oem_table_id: bytes[16..24].try_into().map_err(|_| Error::TooShort)?,
            bytes,
        })
    }

    /// The bytes after the common header.
    pub fn body(&self) -> &'a [u8] {
        &self.bytes[Self::HEADER_LEN..]
    }

    /// The signature as text.
    pub fn signature_str(&self) -> &str {
        core::str::from_utf8(&self.signature).unwrap_or("????")
    }
}

/// Most tables the root table may list.
pub const MAX_TABLES: usize = 64;

/// One entry of the root table, as found.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TableEntry {
    /// Physical address.
    pub address: u64,
    /// Signature, if the header could be read.
    pub signature: [u8; 4],
    /// Length from the header.
    pub length: u32,
    /// Whether the table passed its checks.
    pub status: Result<(), Error>,
}

impl TableEntry {
    /// The signature as text.
    pub fn signature_str(&self) -> &str {
        core::str::from_utf8(&self.signature).unwrap_or("????")
    }
}

/// The tables listed in the RSDT or XSDT.
#[derive(Clone, Copy, Debug)]
pub struct Tables {
    root: TableEntry,
    entries: [Option<TableEntry>; MAX_TABLES],
    /// How many the root table lists, kept or not.
    listed: usize,
}

impl Tables {
    /// Reads the root table the RSDP points to (the XSDT when there is one)
    /// and the header of every table it lists.
    pub fn load<'a>(rsdp: &Rsdp, memory: &impl Memory<'a>) -> Result<Tables, Error> {
        let (address, signature, width) = if rsdp.revision >= 2 && rsdp.xsdt_address != 0 {
            (rsdp.xsdt_address, b"XSDT", 8)
        } else {
            (u64::from(rsdp.rsdt_address), b"RSDT", 4)
        };
        let root = Sdt::load(memory, address, Some(signature))?;
        let mut tables = Tables {
            root: TableEntry {
                address,
                signature: *signature,
                length: root.bytes.len() as u32,
                status: Ok(()),
            },
            entries: [None; MAX_TABLES],
            listed: root.body().len() / width,
        };
        let pointers = root.body().chunks_exact(width).map(|chunk| match width {
            8 => u64_at(chunk, 0).unwrap_or(0),
            _ => u64::from(u32_at(chunk, 0).unwrap_or(0)),
        });
        for (slot, table_address) in tables.entries.iter_mut().zip(pointers) {
            *slot = Some(match memory.bytes(table_address, Sdt::HEADER_LEN) {
                None => TableEntry {
                    address: table_address,
                    signature: *b"????",
                    length: 0,
                    status: Err(Error::Unreachable(table_address)),
                },
                Some(header) => TableEntry {
                    address: table_address,
                    signature: header[..4].try_into().unwrap_or(*b"????"),
                    length: u32_at(header, 4).unwrap_or(0),
                    status: Sdt::load(memory, table_address, None).map(|_| ()),
                },
            });
        }
        Ok(tables)
    }

    /// The root table itself.
    pub fn root(&self) -> &TableEntry {
        &self.root
    }

    /// Every listed table, up to [`MAX_TABLES`].
    pub fn iter(&self) -> impl Iterator<Item = &TableEntry> {
        self.entries.iter().flatten()
    }

    /// How many tables the root table lists past the first
    /// [`MAX_TABLES`], which are not read.
    pub fn dropped(&self) -> usize {
        self.listed.saturating_sub(MAX_TABLES)
    }

    /// Loads the first valid table with `signature`.
    pub fn find<'a>(&self, memory: &impl Memory<'a>, signature: &[u8; 4]) -> Option<Sdt<'a>> {
        self.iter()
            .filter(|entry| &entry.signature == signature && entry.status.is_ok())
            .find_map(|entry| Sdt::load(memory, entry.address, Some(signature)).ok())
    }
}

/// The HPET description table.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Hpet {
    /// Physical address of the register block.
    pub address: u64,
    /// HPET number, for machines with several.
    pub number: u8,
    /// Minimum clock ticks for periodic interrupts without lost interrupts.
    pub minimum_tick: u16,
}

impl Hpet {
    /// Parses an `HPET` table.
    pub fn parse(sdt: &Sdt<'_>) -> Result<Hpet, Error> {
        if &sdt.signature != b"HPET" {
            return Err(Error::BadSignature);
        }
        let body = sdt.body();
        let base = GenericAddress::parse(body.get(4..16).ok_or(Error::TooShort)?)
            .ok_or(Error::TooShort)?;
        Ok(Hpet {
            address: base.address,
            number: *body.get(16).ok_or(Error::TooShort)?,
            minimum_tick: u16_at(body, 17).ok_or(Error::TooShort)?,
        })
    }
}

/// One PCI Express configuration space window from the MCFG.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct McfgEntry {
    /// Physical base address of the window.
    pub base: u64,
    /// PCI segment group.
    pub segment: u16,
    /// First bus number decoded.
    pub start_bus: u8,
    /// Last bus number decoded.
    pub end_bus: u8,
}

/// Parses the windows of an `MCFG` table.
pub fn mcfg_entries<'a>(sdt: &Sdt<'a>) -> impl Iterator<Item = McfgEntry> + 'a {
    let body = if &sdt.signature == b"MCFG" { sdt.body() } else { &[] };
    // Eight reserved bytes precede the 16-byte entries.
    body.get(8..).unwrap_or(&[]).as_chunks::<16>().0.iter().filter_map(|entry| {
        Some(McfgEntry {
            base: u64_at(entry, 0)?,
            segment: u16_at(entry, 8)?,
            start_bus: entry[10],
            end_bus: entry[11],
        })
    })
}

#[cfg(test)]
pub(crate) mod testutil {
    use super::*;
    use std::collections::BTreeMap;

    /// Physical memory made of separate byte blobs.
    #[derive(Default)]
    pub struct FakeMemory {
        pub blobs: BTreeMap<u64, &'static [u8]>,
    }

    impl FakeMemory {
        pub fn insert(&mut self, address: u64, bytes: Vec<u8>) {
            self.blobs.insert(address, Box::leak(bytes.into_boxed_slice()));
        }
    }

    impl Memory<'static> for FakeMemory {
        fn bytes(&self, addr: u64, len: usize) -> Option<&'static [u8]> {
            let (&base, blob) = self.blobs.range(..=addr).next_back()?;
            let start = (addr - base) as usize;
            blob.get(start..start + len)
        }
    }

    /// Builds a table with a correct header and checksum.
    pub fn table(signature: [u8; 4], revision: u8, body: &[u8]) -> Vec<u8> {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&signature);
        bytes.extend_from_slice(&((36 + body.len()) as u32).to_le_bytes());
        bytes.push(revision);
        bytes.push(0); // checksum, fixed below
        bytes.extend_from_slice(b"RADIX ");
        bytes.extend_from_slice(b"TESTTBL ");
        bytes.extend_from_slice(&1u32.to_le_bytes());
        bytes.extend_from_slice(b"RDX ");
        bytes.extend_from_slice(&1u32.to_le_bytes());
        bytes.extend_from_slice(body);
        fix_checksum(&mut bytes, 9);
        bytes
    }

    pub fn fix_checksum(bytes: &mut [u8], at: usize) {
        bytes[at] = 0;
        let sum = bytes.iter().fold(0u8, |s, &b| s.wrapping_add(b));
        bytes[at] = 0u8.wrapping_sub(sum);
    }

    pub fn rsdp_v2(xsdt: u64) -> Vec<u8> {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(b"RSD PTR ");
        bytes.push(0);
        bytes.extend_from_slice(b"RADIX ");
        bytes.push(2);
        bytes.extend_from_slice(&0u32.to_le_bytes());
        bytes.extend_from_slice(&36u32.to_le_bytes());
        bytes.extend_from_slice(&xsdt.to_le_bytes());
        bytes.push(0);
        bytes.extend_from_slice(&[0; 3]);
        let mut v1 = bytes[..20].to_vec();
        fix_checksum(&mut v1, 8);
        bytes[8] = v1[8];
        fix_checksum(&mut bytes, 32);
        bytes
    }
}

#[cfg(test)]
mod tests {
    use super::testutil::*;
    use super::*;

    fn machine() -> (Rsdp, FakeMemory) {
        let mut memory = FakeMemory::default();
        let mut hpet = vec![0u8; 20];
        hpet[4] = 0; // system memory
        hpet[5] = 64;
        hpet[8..16].copy_from_slice(&0xfed0_0000u64.to_le_bytes());
        hpet[16] = 0;
        hpet[17..19].copy_from_slice(&0x80u16.to_le_bytes());
        memory.insert(0x2000, table(*b"HPET", 1, &hpet));

        let mut mcfg = vec![0u8; 8];
        mcfg.extend_from_slice(&0xb000_0000u64.to_le_bytes());
        mcfg.extend_from_slice(&0u16.to_le_bytes());
        mcfg.extend_from_slice(&[0, 0xff, 0, 0, 0, 0]);
        memory.insert(0x3000, table(*b"MCFG", 1, &mcfg));

        let mut broken = table(*b"BRKN", 1, &[1, 2, 3]);
        broken[9] ^= 1;
        memory.insert(0x4000, broken);

        let mut xsdt = Vec::new();
        for address in [0x2000u64, 0x3000, 0x4000, 0x9000] {
            xsdt.extend_from_slice(&address.to_le_bytes());
        }
        memory.insert(0x1000, table(*b"XSDT", 1, &xsdt));
        let rsdp = Rsdp::parse(&rsdp_v2(0x1000)).unwrap();
        (rsdp, memory)
    }

    #[test]
    fn parses_rsdp_versions() {
        let bytes = rsdp_v2(0x1234_5678_9000);
        let rsdp = Rsdp::parse(&bytes).unwrap();
        assert_eq!(rsdp.revision, 2);
        assert_eq!(rsdp.xsdt_address, 0x1234_5678_9000);
        assert_eq!(&rsdp.oem_id, b"RADIX ");

        let mut bad = bytes.clone();
        bad[30] ^= 1;
        assert_eq!(Rsdp::parse(&bad), Err(Error::BadChecksum));
        assert_eq!(Rsdp::parse(b"RSD PTX 0000000000000000"), Err(Error::BadSignature));
        assert_eq!(Rsdp::parse(b"short"), Err(Error::TooShort));
    }

    #[test]
    fn scans_bios_memory() {
        let mut area = vec![0u8; 256];
        area[0x40..0x40 + 36].copy_from_slice(&rsdp_v2(0x1000));
        let (address, rsdp) = Rsdp::scan(&[(0xe0000, &area)]).unwrap();
        assert_eq!(address, 0xe0040);
        assert_eq!(rsdp.xsdt_address, 0x1000);
        assert!(Rsdp::scan(&[(0xe0000, &[0u8; 64])]).is_none());
    }

    #[test]
    fn lists_and_finds_tables() {
        let (rsdp, memory) = machine();
        let tables = Tables::load(&rsdp, &memory).unwrap();
        assert_eq!(tables.root().signature_str(), "XSDT");
        let found: Vec<_> =
            tables.iter().map(|t| (t.signature_str().to_string(), t.status)).collect();
        assert_eq!(found[0], ("HPET".to_string(), Ok(())));
        assert_eq!(found[2], ("BRKN".to_string(), Err(Error::BadChecksum)));
        assert_eq!(found[3].1, Err(Error::Unreachable(0x9000)));

        let hpet = Hpet::parse(&tables.find(&memory, b"HPET").unwrap()).unwrap();
        assert_eq!(hpet.address, 0xfed0_0000);
        assert_eq!(hpet.minimum_tick, 0x80);

        let mcfg = tables.find(&memory, b"MCFG").unwrap();
        let windows: Vec<_> = mcfg_entries(&mcfg).collect();
        assert_eq!(
            windows,
            [McfgEntry { base: 0xb000_0000, segment: 0, start_bus: 0, end_bus: 0xff }]
        );
        assert!(tables.find(&memory, b"BRKN").is_none());
        assert!(tables.find(&memory, b"APIC").is_none());
        assert_eq!(tables.dropped(), 0);
    }

    #[test]
    fn tables_past_the_most_kept_are_counted() {
        let mut memory = FakeMemory::default();
        let mut xsdt = Vec::new();
        for n in 0..MAX_TABLES as u64 + 6 {
            xsdt.extend_from_slice(&(0x10_0000 + n * 0x1000).to_le_bytes());
        }
        memory.insert(0x1000, table(*b"XSDT", 1, &xsdt));
        let rsdp = Rsdp::parse(&rsdp_v2(0x1000)).unwrap();
        let tables = Tables::load(&rsdp, &memory).unwrap();
        assert_eq!(tables.iter().count(), MAX_TABLES);
        assert_eq!(tables.dropped(), 6);
    }
}
