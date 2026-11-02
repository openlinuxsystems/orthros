// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! The GUID partition table (UEFI 2.10 chapter 5): a protective MBR, a
//! header in the second logical sector and its entries after it, and a
//! backup of both at the disk's end, each header and entry array checked
//! by its CRC-32.
//!
//! [`parse`] takes both copies apart and keeps the one that checks, and
//! [`encode`] writes both from the table, so a table read from its backup
//! is repaired by writing it. Nothing read panics: every field is checked
//! against the disk before it is used.

use crate::crc32::crc32;
use crate::guid::Guid;
use crate::table::{Partition, Scheme, Table};
use crate::types::Kind;

/// What starts a header.
pub const SIGNATURE: &[u8; 8] = b"EFI PART";
/// The revision every tool writes: 1.0.
pub const REVISION: u32 = 0x0001_0000;
/// The size of the header's fields.
pub const HEADER_SIZE: u32 = 92;
/// The most bytes of entries read: 1 MiB, far more than any tool makes.
const MAX_ENTRY_BYTES: u64 = 1 << 20;

fn u32_at(b: &[u8], at: usize) -> u32 {
    u32::from_le_bytes([b[at], b[at + 1], b[at + 2], b[at + 3]])
}

fn u64_at(b: &[u8], at: usize) -> u64 {
    u64::from(u32_at(b, at)) | u64::from(u32_at(b, at + 4)) << 32
}

fn guid_at(b: &[u8], at: usize) -> Guid {
    let mut g = [0u8; 16];
    g.copy_from_slice(&b[at..at + 16]);
    Guid(g)
}

/// A GPT header's fields.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Header {
    /// Where this header is.
    pub current: u64,
    /// Where the other copy is.
    pub other: u64,
    /// The first sector partitions may have.
    pub first_usable: u64,
    /// The last sector partitions may have.
    pub last_usable: u64,
    /// The disk's GUID.
    pub disk_guid: Guid,
    /// Where its entries start.
    pub entries_lba: u64,
    /// How many entries there are.
    pub entry_count: u32,
    /// The size of each.
    pub entry_size: u32,
    /// The CRC-32 of its entries.
    pub entries_crc: u32,
}

impl Header {
    /// The logical sectors its entries take, in sectors of `sector`
    /// bytes.
    pub fn entry_sectors(&self, sector: u32) -> u64 {
        (u64::from(self.entry_count) * u64::from(self.entry_size)).div_ceil(u64::from(sector))
    }

    /// The bytes its entries take.
    pub fn entry_bytes(&self) -> usize {
        self.entry_count as usize * self.entry_size as usize
    }
}

/// The header in `sector`, read from logical sector `lba` of a disk of
/// `disk` sectors; why not, if it is not one or is damaged.
///
/// # Errors
///
/// In words, if the header is not one that can be used.
pub fn parse_header(sector: &[u8], lba: u64, disk: u64) -> Result<Header, &'static str> {
    if sector.len() < 512 || &sector[..8] != SIGNATURE {
        return Err("no GPT header is there");
    }
    let size = u32_at(sector, 12);
    if size < HEADER_SIZE || size as usize > sector.len() {
        return Err("its header's size is wrong");
    }
    let mut copy = sector[..size as usize].to_vec();
    copy[16..20].fill(0);
    if crc32(&copy) != u32_at(sector, 16) {
        return Err("its header's CRC-32 does not match");
    }
    if u32_at(sector, 8) >> 16 != 1 {
        return Err("its header is of a revision other than 1");
    }
    let header = Header {
        current: u64_at(sector, 24),
        other: u64_at(sector, 32),
        first_usable: u64_at(sector, 40),
        last_usable: u64_at(sector, 48),
        disk_guid: guid_at(sector, 56),
        entries_lba: u64_at(sector, 72),
        entry_count: u32_at(sector, 80),
        entry_size: u32_at(sector, 84),
        entries_crc: u32_at(sector, 88),
    };
    if header.current != lba {
        return Err("its header says it is elsewhere");
    }
    let entry_size = header.entry_size;
    if entry_size < 128 || !entry_size.is_power_of_two() {
        return Err("its entries are of a size other than 128 bytes times a power of two");
    }
    let bytes = u64::from(header.entry_count) * u64::from(entry_size);
    if header.entry_count == 0 || bytes > MAX_ENTRY_BYTES {
        return Err("its number of entries is wrong");
    }
    if header.first_usable > header.last_usable.saturating_add(1) || header.last_usable >= disk {
        return Err("its usable sectors are not on the disk");
    }
    let entries_end = header.entries_lba.saturating_add(bytes.div_ceil(sector.len() as u64));
    if header.entries_lba < 2 || header.entries_lba >= disk || entries_end > disk {
        return Err("its entries are not on the disk");
    }
    Ok(header)
}

/// The partitions in `entries`, the entry array of `header`, if their
/// CRC-32 matches.
///
/// # Errors
///
/// In words, if the array is damaged.
pub fn parse_entries(header: &Header, entries: &[u8]) -> Result<Vec<Partition>, &'static str> {
    let bytes = header.entry_bytes();
    if entries.len() < bytes {
        return Err("its entries could not all be read");
    }
    if crc32(&entries[..bytes]) != header.entries_crc {
        return Err("its entries' CRC-32 does not match");
    }
    let mut found = Vec::new();
    for (i, entry) in entries[..bytes].chunks_exact(header.entry_size as usize).enumerate() {
        let kind = guid_at(entry, 0);
        if kind.is_zero() {
            continue;
        }
        let units: Vec<u16> =
            entry[56..128].as_chunks::<2>().0.iter().map(|&c| u16::from_le_bytes(c)).collect();
        let len = units.iter().position(|&u| u == 0).unwrap_or(units.len());
        found.push(Partition {
            number: i as u32 + 1,
            first: u64_at(entry, 32),
            last: u64_at(entry, 40),
            kind: Kind::Gpt(kind),
            guid: guid_at(entry, 16),
            name: char::decode_utf16(units[..len].iter().copied())
                .map(|c| c.unwrap_or(char::REPLACEMENT_CHARACTER))
                .collect(),
            attributes: u64_at(entry, 48),
            bootable: false,
            logical: false,
        });
    }
    Ok(found)
}

/// The entry array of `table`.
pub fn encode_entries(table: &Table) -> Vec<u8> {
    let size = table.entry_size as usize;
    let mut bytes = vec![0u8; table.entry_count as usize * size];
    for p in &table.partitions {
        let Kind::Gpt(kind) = p.kind else { continue };
        let at = (p.number as usize - 1) * size;
        let Some(entry) = bytes.get_mut(at..at + 128) else { continue };
        entry[0..16].copy_from_slice(&kind.0);
        entry[16..32].copy_from_slice(&p.guid.0);
        entry[32..40].copy_from_slice(&p.first.to_le_bytes());
        entry[40..48].copy_from_slice(&p.last.to_le_bytes());
        entry[48..56].copy_from_slice(&p.attributes.to_le_bytes());
        for (i, unit) in p.name.encode_utf16().take(36).enumerate() {
            entry[56 + 2 * i..58 + 2 * i].copy_from_slice(&unit.to_le_bytes());
        }
    }
    bytes
}

/// A header's sector, of `sector` bytes.
pub fn encode_header(header: &Header, sector: u32) -> Vec<u8> {
    let mut b = vec![0u8; sector as usize];
    b[..8].copy_from_slice(SIGNATURE);
    b[8..12].copy_from_slice(&REVISION.to_le_bytes());
    b[12..16].copy_from_slice(&HEADER_SIZE.to_le_bytes());
    b[24..32].copy_from_slice(&header.current.to_le_bytes());
    b[32..40].copy_from_slice(&header.other.to_le_bytes());
    b[40..48].copy_from_slice(&header.first_usable.to_le_bytes());
    b[48..56].copy_from_slice(&header.last_usable.to_le_bytes());
    b[56..72].copy_from_slice(&header.disk_guid.0);
    b[72..80].copy_from_slice(&header.entries_lba.to_le_bytes());
    b[80..84].copy_from_slice(&header.entry_count.to_le_bytes());
    b[84..88].copy_from_slice(&header.entry_size.to_le_bytes());
    b[88..92].copy_from_slice(&header.entries_crc.to_le_bytes());
    let crc = crc32(&b[..HEADER_SIZE as usize]);
    b[16..20].copy_from_slice(&crc.to_le_bytes());
    b
}

/// The headers of `table`: the primary one, in the second sector with
/// its entries after it, and the backup, in the last with its entries
/// before it.
pub fn headers(table: &Table, entries: &[u8]) -> (Header, Header) {
    let last = table.disk_sectors - 1;
    let backup_entries = last - table.entry_sectors();
    let primary = Header {
        current: 1,
        other: last,
        first_usable: table.first_usable,
        last_usable: table.last_usable,
        disk_guid: table.disk_guid,
        entries_lba: 2,
        entry_count: table.entry_count,
        entry_size: table.entry_size,
        entries_crc: crc32(entries),
    };
    let backup = Header { current: last, other: 1, entries_lba: backup_entries, ..primary };
    (primary, backup)
}

/// The protective MBR (UEFI 2.10 section 5.2.3): `boot_code` kept, and
/// one partition of type 0xEE from sector 1 over the disk, or as much of
/// it as 32 bits count.
pub fn protective_mbr(table: &Table) -> Vec<u8> {
    let mut b = vec![0u8; table.sector_size as usize];
    b[..440].copy_from_slice(&table.boot_code[..440]);
    let entry = &mut b[446..462];
    entry[1..4].copy_from_slice(&[0x00, 0x02, 0x00]);
    entry[4] = 0xee;
    entry[5..8].copy_from_slice(&[0xff, 0xff, 0xff]);
    entry[8..12].copy_from_slice(&1u32.to_le_bytes());
    let size = (table.disk_sectors - 1).min(u64::from(u32::MAX)) as u32;
    entry[12..16].copy_from_slice(&size.to_le_bytes());
    b[510] = 0x55;
    b[511] = 0xaa;
    b
}

/// What writing `table` puts where, in the order it is written: the
/// backup first, then the primary copy and the protective MBR, as UEFI
/// asks, so that a write cut short leaves one copy whole.
pub fn encode(table: &Table) -> Vec<(u64, Vec<u8>)> {
    debug_assert_eq!(table.scheme, Scheme::Gpt);
    let mut entries = encode_entries(table);
    let (primary, backup) = headers(table, &entries);
    let padded = (table.entry_sectors() * u64::from(table.sector_size)) as usize;
    entries.resize(padded, 0);
    vec![
        (backup.entries_lba, entries.clone()),
        (backup.current, encode_header(&backup, table.sector_size)),
        (primary.entries_lba, entries),
        (primary.current, encode_header(&primary, table.sector_size)),
        (0, protective_mbr(table)),
    ]
}

/// A table made from `header` and its `partitions`, for a disk of
/// `disk` sectors of `sector` bytes; what changed to make it fit the
/// disk is added to `notes`.
pub fn table_of(
    header: &Header,
    partitions: Vec<Partition>,
    sector: u32,
    disk: u64,
    boot_code: Vec<u8>,
    notes: &mut Vec<String>,
) -> Table {
    let mut table = Table {
        scheme: Scheme::Gpt,
        sector_size: sector,
        disk_sectors: disk,
        disk_guid: header.disk_guid,
        signature: 0,
        entry_count: header.entry_count,
        entry_size: header.entry_size,
        first_usable: header.first_usable.max(2 + header.entry_sectors(sector)),
        last_usable: header.last_usable,
        partitions,
        boot_code,
    };
    // Written again, the backup goes at the disk's end with its entries
    // before it: what is between becomes usable, or, if the disk shrank,
    // what is past it is not.
    let end = disk.saturating_sub(2 + table.entry_sectors());
    let backup_at = if header.current == 1 { header.other } else { header.current };
    if backup_at != disk - 1 {
        notes.push(if backup_at < disk - 1 {
            format!(
                "The disk is larger than the table says (its backup is at sector {backup_at}, not \
                 {}). Writing the table puts the backup at the end and makes the space after the \
                 last partition usable.",
                disk - 1
            )
        } else {
            format!(
                "The disk is smaller than the table says (its backup would be at sector \
                 {backup_at}). Partitions past sector {end} do not fit."
            )
        });
    }
    table.last_usable = if backup_at < disk - 1 { end } else { table.last_usable.min(end) };
    table
}
