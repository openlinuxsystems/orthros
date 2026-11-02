// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Dukovo's disk tools' engine (ADR 0300).
//!
//! One crate does what rdisk(8), mkfs-fat(8), fsck-fat(8) and
//! mkfs-exfat(8) do, and builds for Linux too, so that `./ob image`
//! assembles images with the code that runs on the machines they boot:
//!
//! - [`table`] holds a partition table in memory as rdisk changes it;
//!   [`gpt`] and [`mbr`] take tables apart and put them together, and
//!   [`read`] and [`plan`] read one from a disk and write one to it.
//! - [`layout`] reads and writes layout files (rdisk-layout(5)), which
//!   the installer and `./ob image` apply with `rdisk --script`.
//! - [`fat`] makes FAT12, FAT16 and FAT32 filesystems, [`fatck`] checks
//!   and mends them, and [`exfat`] makes exFAT ones, from Microsoft's
//!   published specifications.
//! - [`types`] names the partition types, [`size`] reads and prints
//!   sizes, and [`guid`], [`crc32`] and [`rng`] are what GPT needs.
//!
//! What is read from a disk is checked before it is used, and nothing
//! read panics. Nothing is written until asked: [`plan`] says what a
//! write puts where, and [`apply`] writes it.

pub mod crc32;
pub mod device;
pub mod exfat;
pub mod fat;
pub mod fatck;
pub mod gpt;
pub mod guid;
pub mod layout;
pub mod mbr;
pub mod rng;
pub mod signatures;
pub mod size;
pub mod table;
pub mod types;

#[cfg(test)]
mod tests;

use std::io;

pub use device::Device;
pub use guid::Guid;
pub use rng::Rng;
pub use table::{End, New, Partition, Scheme, Table};
pub use types::Kind;

/// What is on a disk.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Found {
    /// Its partition table, if it has one.
    pub table: Option<Table>,
    /// What is odd about it, in words: a damaged copy of a GPT, a disk
    /// larger than its table says.
    pub notes: Vec<String>,
    /// The filesystems on the whole disk, which a table written over them
    /// erases ([`signatures::find`]).
    pub signatures: Vec<signatures::Signature>,
}

/// Why a disk's table could not be read.
#[derive(Debug)]
pub enum ReadError {
    /// The disk could not be read.
    Io(io::Error),
    /// The table is damaged beyond use: why.
    Damaged(String),
}

impl std::fmt::Display for ReadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ReadError::Io(err) => write!(f, "{err}"),
            ReadError::Damaged(why) => f.write_str(why),
        }
    }
}

impl std::error::Error for ReadError {}

impl From<io::Error> for ReadError {
    fn from(err: io::Error) -> ReadError {
        ReadError::Io(err)
    }
}

/// Reads the partition table of `disk`: a GPT, from whichever copy
/// checks, or an MBR, with the logical partitions of an extended one.
///
/// # Errors
///
/// If the disk cannot be read, or its GPT is damaged in both copies.
pub fn read(disk: &Device) -> Result<Found, ReadError> {
    let mut notes = Vec::new();
    let sectors = disk.sectors();
    let size = disk.sector_size();
    let signatures = signatures::find(disk);
    if sectors < 3 {
        return Ok(Found { table: None, notes, signatures });
    }
    let first = disk.read_sectors(0, 1)?;
    let Some(entries) = mbr::entries(&first) else {
        if gpt_header_at(disk, 1) {
            notes.push(
                "The disk has a GPT header but no protective MBR before it; rdisk leaves it \
                 alone, as Linux and the kernel do."
                    .to_string(),
            );
        }
        return Ok(Found { table: None, notes, signatures });
    };
    if mbr::is_boot_sector(&first) {
        return Ok(Found { table: None, notes, signatures });
    }
    if entries.iter().any(|e| e.kind == mbr::PROTECTIVE) {
        if entries.iter().filter(|e| e.kind != 0).count() > 1 {
            notes.push(
                "The protective MBR holds other partitions too (a hybrid MBR). Writing the table \
                 makes it a plain protective MBR."
                    .to_string(),
            );
        }
        let table = read_gpt(disk, first, &mut notes)?;
        return Ok(Found { table: Some(table), notes, signatures });
    }
    let mut read = |lba: u64| disk.read_sectors(lba, 1).ok();
    let table = mbr::table_of(&first, &entries, size, sectors, &mut read, &mut notes);
    Ok(Found { table: Some(table), notes, signatures })
}

/// Whether logical sector `lba` holds a GPT header's signature.
fn gpt_header_at(disk: &Device, lba: u64) -> bool {
    disk.read_sectors(lba, 1).is_ok_and(|s| &s[..8] == gpt::SIGNATURE)
}

/// One copy of a GPT, its header in logical sector `lba`.
fn gpt_copy(disk: &Device, lba: u64) -> Result<(gpt::Header, Vec<Partition>), &'static str> {
    let sector = disk.read_sectors(lba, 1).map_err(|_| "its header cannot be read")?;
    let header = gpt::parse_header(&sector, lba, disk.sectors())?;
    let entries = disk
        .read_sectors(header.entries_lba, header.entry_sectors(disk.sector_size()))
        .map_err(|_| "its entries cannot be read")?;
    let partitions = gpt::parse_entries(&header, &entries)?;
    Ok((header, partitions))
}

fn read_gpt(disk: &Device, first: Vec<u8>, notes: &mut Vec<String>) -> Result<Table, ReadError> {
    let last = disk.sectors() - 1;
    let primary = gpt_copy(disk, 1);
    let backup_lba = match &primary {
        Ok((header, _)) if header.other > 1 && header.other <= last => header.other,
        _ => last,
    };
    let backup = gpt_copy(disk, backup_lba);
    let (header, partitions) = match (primary, backup) {
        (Ok(primary), Ok(_)) => primary,
        (Ok(primary), Err(why)) => {
            notes.push(format!(
                "The GPT's backup copy is damaged: {why}. Writing the table (w) repairs it."
            ));
            primary
        }
        (Err(why), Ok(backup)) => {
            notes.push(format!(
                "The GPT's primary copy is damaged: {why}. rdisk read the backup; writing the \
                 table (w) repairs the primary."
            ));
            backup
        }
        (Err(primary), Err(backup)) => {
            return Err(ReadError::Damaged(format!(
                "the disk's GPT is damaged: in its primary copy {primary}, and in its backup \
                 {backup}"
            )));
        }
    };
    let mut boot_code = first;
    boot_code.truncate(440);
    let mut table =
        gpt::table_of(&header, partitions, disk.sector_size(), disk.sectors(), boot_code, notes);
    table.partitions.retain(|p| {
        let fits = p.first <= p.last;
        if !fits {
            notes.push(format!("Entry {} ends before it starts; rdisk leaves it out.", p.number));
        }
        fits
    });
    Ok(table)
}

/// A write of a table: what goes where.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Write {
    /// The first logical sector written.
    pub lba: u64,
    /// What is written there: whole sectors.
    pub bytes: Vec<u8>,
    /// What it is, for `--dry-run`.
    pub what: String,
}

/// What writing `table` to `disk` puts where, in order: the table's
/// sectors, and zeros over what would otherwise be read as an old one
/// (a GPT's headers when an MBR replaces it) or as a filesystem on the
/// whole disk, where no partition of `table` is.
///
/// # Errors
///
/// If the disk cannot be read, or `table` was made for another disk.
pub fn plan(disk: &Device, table: &Table) -> io::Result<Vec<Write>> {
    if table.sector_size != disk.sector_size() || table.disk_sectors != disk.sectors() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "the table was made for a disk of another size",
        ));
    }
    let size = u64::from(table.sector_size);
    let mut writes = Vec::new();
    let in_partition =
        |first: u64, last: u64| table.partitions.iter().any(|p| first <= p.last && p.first <= last);
    let zero = |writes: &mut Vec<Write>, lba: u64, count: u64, what: String| {
        let last = lba + count - 1;
        if count > 0 && last < table.disk_sectors && !in_partition(lba, last) {
            writes.push(Write { lba, bytes: vec![0; (count * size) as usize], what });
        }
    };
    match table.scheme {
        Scheme::Gpt => {
            let entries = table.entry_sectors();
            let last = table.disk_sectors - 1;
            for (lba, bytes) in gpt::encode(table) {
                let what = if lba == 0 {
                    "the protective MBR, at sector 0".to_string()
                } else if lba == 1 {
                    "the GPT's header, at sector 1".to_string()
                } else if lba == 2 {
                    format!("its {} entries, at sectors 2 to {}", table.entry_count, 1 + entries)
                } else if lba == last {
                    format!("the backup header, at sector {last}")
                } else {
                    format!("the backup entries, at sectors {lba} to {}", lba + entries - 1)
                };
                writes.push(Write { lba, bytes, what });
            }
        }
        Scheme::Mbr => {
            // A GPT's headers go, so that nothing reads the old table.
            for lba in [1, table.disk_sectors - 1] {
                let Ok(sector) = disk.read_sectors(lba, 1) else { continue };
                if &sector[..8] != gpt::SIGNATURE {
                    continue;
                }
                let count = gpt::parse_header(&sector, lba, table.disk_sectors)
                    .map_or(1, |h| if lba == 1 { 1 + h.entry_sectors(table.sector_size) } else { 1 });
                let what = format!("zeros over the old GPT's header at sector {lba}");
                zero(&mut writes, lba, count, what);
            }
            for (lba, bytes) in mbr::encode(table) {
                writes.push(Write { lba, bytes, what: "the MBR, at sector 0".to_string() });
            }
        }
    }
    for found in signatures::find(disk) {
        let lba = found.offset / size;
        let end = (found.offset + found.len as u64).div_ceil(size);
        if lba == 0 || writes.iter().any(|w| w.lba <= lba && lba < w.lba + w.bytes.len() as u64 / size) {
            continue;
        }
        let what = format!("zeros over the {} signature at sector {lba}", found.name);
        zero(&mut writes, lba, end - lba, what);
    }
    Ok(writes)
}

/// Writes what [`plan`] said, then empties the disk's write cache.
///
/// # Errors
///
/// If a write fails: the table may then be half written, and the GPT's
/// backup, written first, is whole.
pub fn apply(disk: &Device, writes: &[Write]) -> io::Result<()> {
    for write in writes {
        disk.write_sectors(write.lba, &write.bytes)?;
    }
    disk.sync()
}
