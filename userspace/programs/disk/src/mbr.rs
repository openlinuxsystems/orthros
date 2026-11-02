// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! The master boot record: four primary entries in the disk's first
//! sector, after the boot loader's 440 bytes and the disk signature, and
//! the logical partitions of an extended one in a chain of extended boot
//! records.
//!
//! rdisk writes only the first sector: it makes primary partitions, and
//! keeps the logical ones a disk has as they are, so their records are
//! never written.

use crate::guid::Guid;
use crate::table::{Partition, Scheme, Table};
use crate::types::Kind;

/// The type of a protective MBR's entry: the disk has a GPT.
pub const PROTECTIVE: u8 = 0xee;
/// The most logical partitions followed, as the kernel follows them.
const MAX_LOGICAL: u32 = 64;

/// An entry of an MBR or an extended boot record.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Entry {
    /// 0x80 for a bootable partition, else 0.
    pub status: u8,
    /// Its type; 0 for an empty entry.
    pub kind: u8,
    /// Its first sector, from what holds the record.
    pub first: u32,
    /// How many sectors it has.
    pub sectors: u32,
}

fn u32_at(b: &[u8], at: usize) -> u32 {
    u32::from_le_bytes([b[at], b[at + 1], b[at + 2], b[at + 3]])
}

/// Whether `sector` is a filesystem's boot sector rather than a table:
/// FAT, exFAT and NTFS put their names where they are, and end it with
/// 0x55 0xAA as a table does.
pub fn is_boot_sector(sector: &[u8]) -> bool {
    let jump = matches!(sector[0], 0xeb | 0xe9);
    let named = &sector[3..11] == b"NTFS    "
        || &sector[3..11] == b"EXFAT   "
        || &sector[54..57] == b"FAT"
        || &sector[82..87] == b"FAT32";
    jump && named
}

/// The four entries of `sector`, if it is an MBR or an extended boot
/// record: it ends with 0x55 0xAA and each status is 0 or 0x80.
pub fn entries(sector: &[u8]) -> Option<[Entry; 4]> {
    if sector.len() < 512 || sector[510] != 0x55 || sector[511] != 0xaa {
        return None;
    }
    let mut found = [Entry { status: 0, kind: 0, first: 0, sectors: 0 }; 4];
    for (i, entry) in found.iter_mut().enumerate() {
        let at = 446 + 16 * i;
        *entry = Entry {
            status: sector[at],
            kind: sector[at + 4],
            first: u32_at(sector, at + 8),
            sectors: u32_at(sector, at + 12),
        };
        if entry.status != 0 && entry.status != 0x80 {
            return None;
        }
    }
    Some(found)
}

/// The partitions of the MBR `first`, the disk's first sector, of a disk
/// of `disk` sectors of `sector` bytes, reading the extended boot records
/// of an extended partition with `read`. What is odd but kept is added to
/// `notes`.
pub fn table_of(
    first: &[u8],
    entries: &[Entry; 4],
    sector: u32,
    disk: u64,
    read: &mut dyn FnMut(u64) -> Option<Vec<u8>>,
    notes: &mut Vec<String>,
) -> Table {
    let mut table = Table {
        scheme: Scheme::Mbr,
        sector_size: sector,
        disk_sectors: disk,
        disk_guid: Guid::ZERO,
        signature: u32_at(first, 440),
        entry_count: 4,
        entry_size: 16,
        first_usable: 1,
        last_usable: disk.saturating_sub(1).min(u64::from(u32::MAX)),
        partitions: Vec::new(),
        boot_code: first[..440].to_vec(),
    };
    for (i, entry) in entries.iter().enumerate() {
        if entry.kind == 0 || entry.sectors == 0 {
            continue;
        }
        let (start, size) = (u64::from(entry.first), u64::from(entry.sectors));
        let number = i as u32 + 1;
        let kind = Kind::Mbr(entry.kind);
        table.partitions.push(Partition {
            number,
            first: start,
            last: start + size - 1,
            kind,
            guid: Guid::ZERO,
            name: String::new(),
            attributes: 0,
            bootable: entry.status == 0x80,
            logical: false,
        });
        if kind.is_extended() {
            logical(&mut table, start, size, read, notes);
        }
    }
    table.partitions.sort_by_key(|p| p.number);
    table
}

/// The logical partitions in the extended partition from `base`, of
/// `size` sectors: a chain of extended boot records, each with a
/// partition (from it) and the next record (from `base`). A record that
/// cannot be read or is not one, or a link outside or backwards, ends the
/// chain, as on Linux and in the kernel.
fn logical(
    table: &mut Table,
    base: u64,
    size: u64,
    read: &mut dyn FnMut(u64) -> Option<Vec<u8>>,
    notes: &mut Vec<String>,
) {
    let mut record = base;
    for number in 5..5 + MAX_LOGICAL {
        let Some(sector) = read(record) else { return };
        let Some(entries) = entries(&sector) else {
            if number == 5 && sector.iter().any(|&b| b != 0) {
                notes.push("The extended partition's first record is not one.".to_string());
            }
            return;
        };
        let this = entries[0];
        if this.kind != 0 && this.sectors > 0 {
            let first = record + u64::from(this.first);
            table.partitions.push(Partition {
                number,
                first,
                last: first + u64::from(this.sectors) - 1,
                kind: Kind::Mbr(this.kind),
                guid: Guid::ZERO,
                name: String::new(),
                attributes: 0,
                bootable: this.status == 0x80,
                logical: true,
            });
        }
        let next = entries[1];
        if next.kind == 0 || !Kind::Mbr(next.kind).is_extended() {
            return;
        }
        let at = base + u64::from(next.first);
        if at <= record || at >= base + size {
            notes.push(format!("The chain of logical partitions breaks after partition {number}."));
            return;
        }
        record = at;
    }
}

/// CHS for `lba` in the geometry every tool assumes now, 255 heads of 63
/// sectors, or the largest CHS for one past it, as fdisk writes them.
fn chs(lba: u64) -> [u8; 3] {
    let cylinder = lba / (255 * 63);
    if cylinder > 1023 {
        return [0xfe, 0xff, 0xff];
    }
    let head = (lba / 63) % 255;
    let sector = lba % 63 + 1;
    [head as u8, sector as u8 | ((cylinder >> 2) as u8 & 0xc0), cylinder as u8]
}

/// The disk's first sector with `table`, an MBR: the boot code kept, the
/// disk signature, and the primary partitions in their entries.
pub fn encode(table: &Table) -> Vec<(u64, Vec<u8>)> {
    debug_assert_eq!(table.scheme, Scheme::Mbr);
    let mut b = vec![0u8; table.sector_size as usize];
    b[..440].copy_from_slice(&table.boot_code[..440]);
    b[440..444].copy_from_slice(&table.signature.to_le_bytes());
    for p in table.partitions.iter().filter(|p| !p.logical && (1..=4).contains(&p.number)) {
        let Kind::Mbr(kind) = p.kind else { continue };
        let at = 446 + 16 * (p.number as usize - 1);
        let entry = &mut b[at..at + 16];
        entry[0] = if p.bootable { 0x80 } else { 0 };
        entry[1..4].copy_from_slice(&chs(p.first));
        entry[4] = kind;
        entry[5..8].copy_from_slice(&chs(p.last));
        entry[8..12].copy_from_slice(&(p.first as u32).to_le_bytes());
        entry[12..16].copy_from_slice(&(p.sectors() as u32).to_le_bytes());
    }
    b[510] = 0x55;
    b[511] = 0xaa;
    vec![(0, b)]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chs_as_fdisk_writes_it() {
        assert_eq!(chs(2048), [32, 33, 0]);
        assert_eq!(chs(0), [0, 1, 0]);
        assert_eq!(chs(16_450_559), [254, 63 | 0xc0, 0xff]);
        assert_eq!(chs(16_450_560), [0xfe, 0xff, 0xff]);
    }
}
