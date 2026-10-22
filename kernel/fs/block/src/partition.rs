// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Partition tables: the MBR (with logical partitions in an extended
//! one) and the GUID partition table (GPT, UEFI 2.10 chapter 5).
//!
//! [`read`] looks at a disk's first sectors and returns its partitions,
//! numbered as Linux numbers them: GPT entries by their place in the
//! table from 1, MBR primary partitions 1 to 4 and logical ones from 5.
//! A disk whose first sector is no partition table (a filesystem made on
//! the whole disk, as on a "superfloppy") has none, and is used whole.
//!
//! Everything read from the disk is checked before use: GPT headers and
//! entry arrays by their CRC-32, with the backup header at the disk's
//! last sector when the primary one is damaged, and every partition
//! against the disk's size ([`within`], which Kani proves never lets one
//! reach past the end). A damaged table yields an error, never a panic.

use crate::crc32::{Crc32, crc32};
use crate::mem::{self, KVec, NoMemory};

/// The size of a sector.
pub const SECTOR: usize = 512;

/// The most partitions read from one table.
pub const MAX_PARTITIONS: usize = 128;

/// The most logical partitions followed in an extended partition.
const MAX_LOGICAL: usize = 64;

/// What kind of partition an entry says it is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    /// An MBR entry's type byte.
    Mbr(u8),
    /// A GPT entry's type GUID, in its on-disk byte order.
    Gpt([u8; 16]),
}

/// A partition.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Partition {
    /// Its number: 1 for the first.
    pub number: u32,
    /// Its first sector.
    pub first: u64,
    /// Its size in sectors.
    pub sectors: u64,
    /// What kind it is.
    pub kind: Kind,
    /// Its name, for GPT: UTF-16, up to the first NUL.
    pub name: [u16; 36],
}

impl Partition {
    /// Its name, for GPT partitions that have one.
    pub fn name(&self) -> impl Iterator<Item = char> + '_ {
        let len = self.name.iter().position(|&c| c == 0).unwrap_or(self.name.len());
        char::decode_utf16(self.name[..len].iter().copied())
            .map(|c| c.unwrap_or(char::REPLACEMENT_CHARACTER))
    }
}

/// Which table a disk has.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Scheme {
    /// None: the disk is used whole.
    None,
    /// An MBR.
    Mbr,
    /// A GUID partition table.
    Gpt,
}

/// Why the table could not be read.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
    /// A sector could not be read.
    Io,
    /// The table is damaged: the detail says how.
    Damaged(&'static str),
    /// Out of memory for the list.
    NoMemory,
}

impl From<NoMemory> for Error {
    fn from(_: NoMemory) -> Error {
        Error::NoMemory
    }
}

impl core::fmt::Display for Error {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Error::Io => f.write_str("a sector of the partition table could not be read"),
            Error::Damaged(why) => write!(f, "the partition table is damaged: {why}"),
            Error::NoMemory => f.write_str("out of memory"),
        }
    }
}

/// Reads a sector of the disk: its number, and where to put it.
pub type ReadSector<'a> = &'a mut dyn FnMut(u64, &mut [u8; SECTOR]) -> Result<(), Error>;

/// Whether `sectors` sectors from `first` fit on a disk of `disk`
/// sectors, after its first (where the table is), and are not empty.
pub fn within(first: u64, sectors: u64, disk: u64) -> bool {
    sectors > 0 && first >= 1 && first.checked_add(sectors).is_some_and(|end| end <= disk)
}

fn u32_at(bytes: &[u8], at: usize) -> u32 {
    u32::from_le_bytes([bytes[at], bytes[at + 1], bytes[at + 2], bytes[at + 3]])
}

fn u64_at(bytes: &[u8], at: usize) -> u64 {
    u64::from(u32_at(bytes, at)) | (u64::from(u32_at(bytes, at + 4)) << 32)
}

/// An MBR entry: its type, first sector (relative) and size.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MbrEntry {
    /// The boot indicator: 0x80 or 0.
    pub status: u8,
    /// The type byte; 0 for an empty entry.
    pub kind: u8,
    /// The first sector, from the start of what holds the table.
    pub first: u32,
    /// The size in sectors.
    pub sectors: u32,
}

/// The four entries of an MBR or extended boot record, if `sector` is
/// one: it ends with 0x55 0xaa and every entry's boot indicator is 0 or
/// 0x80.
pub fn mbr_entries(sector: &[u8; SECTOR]) -> Option<[MbrEntry; 4]> {
    if sector[510] != 0x55 || sector[511] != 0xaa {
        return None;
    }
    let mut entries = [MbrEntry { status: 0, kind: 0, first: 0, sectors: 0 }; 4];
    for (i, entry) in entries.iter_mut().enumerate() {
        let at = 446 + 16 * i;
        *entry = MbrEntry {
            status: sector[at],
            kind: sector[at + 4],
            first: u32_at(sector, at + 8),
            sectors: u32_at(sector, at + 12),
        };
        if entry.status != 0 && entry.status != 0x80 {
            return None;
        }
    }
    Some(entries)
}

/// The type of a protective MBR's entry, which says the disk has a GPT.
const PROTECTIVE: u8 = 0xee;

fn is_extended(kind: u8) -> bool {
    matches!(kind, 0x05 | 0x0f | 0x85)
}

/// Reads the partition table of a disk of `disk` sectors, reading sectors
/// with `read`.
pub fn read(disk: u64, read: ReadSector<'_>) -> Result<(Scheme, KVec<Partition>), Error> {
    let mut found = mem::vec();
    if disk < 2 {
        return Ok((Scheme::None, found));
    }
    let mut sector = [0u8; SECTOR];
    read(0, &mut sector)?;
    let Some(entries) = mbr_entries(&sector) else { return Ok((Scheme::None, found)) };
    if entries.iter().any(|e| e.kind == PROTECTIVE) {
        gpt(disk, read, &mut found)?;
        return Ok((Scheme::Gpt, found));
    }
    if entries.iter().all(|e| e.kind == 0) {
        return Ok((Scheme::None, found));
    }
    for (i, entry) in entries.iter().enumerate() {
        if entry.kind == 0 {
            continue;
        }
        let (first, sectors) = (u64::from(entry.first), u64::from(entry.sectors));
        if !within(first, sectors, disk) {
            return Err(Error::Damaged("an MBR partition reaches past the end of the disk"));
        }
        mem::push(&mut found, mbr_partition(i as u32 + 1, first, sectors, entry.kind))?;
        if is_extended(entry.kind) {
            logical(disk, first, sectors, read, &mut found)?;
        }
    }
    Ok((Scheme::Mbr, found))
}

fn mbr_partition(number: u32, first: u64, sectors: u64, kind: u8) -> Partition {
    Partition { number, first, sectors, kind: Kind::Mbr(kind), name: [0; 36] }
}

/// The logical partitions of the extended partition at `base`: a chain of
/// extended boot records, each with a partition (relative to it) and the
/// next record (relative to `base`).
fn logical(
    disk: u64,
    base: u64,
    size: u64,
    read: ReadSector<'_>,
    found: &mut KVec<Partition>,
) -> Result<(), Error> {
    let mut record = base;
    let mut sector = [0u8; SECTOR];
    for number in 5..5 + MAX_LOGICAL as u32 {
        read(record, &mut sector)?;
        let Some(entries) = mbr_entries(&sector) else {
            return Err(Error::Damaged("an extended boot record has no signature"));
        };
        let this = entries[0];
        if this.kind != 0 {
            let first =
                record.checked_add(u64::from(this.first)).ok_or(Error::Damaged("overflow"))?;
            let sectors = u64::from(this.sectors);
            if !within(first, sectors, disk) || !within(first, sectors, base + size) {
                return Err(Error::Damaged("a logical partition reaches past its extended one"));
            }
            mem::push(found, mbr_partition(number, first, sectors, this.kind))?;
        }
        let next = entries[1];
        if !is_extended(next.kind) || next.first == 0 {
            return Ok(());
        }
        let at = base + u64::from(next.first);
        if at <= record || at >= base + size {
            return Err(Error::Damaged("an extended boot record points backwards or outside"));
        }
        record = at;
    }
    Ok(())
}

/// A GPT header, checked.
struct Header {
    first_usable: u64,
    last_usable: u64,
    entries_at: u64,
    entries: u32,
    entry_size: u32,
    entries_crc: u32,
}

const GPT_SIGNATURE: &[u8; 8] = b"EFI PART";

/// The GPT header in `sector`, read from sector `at` of a disk of `disk`
/// sectors, if it is one and its CRC matches.
fn header(sector: &[u8; SECTOR], at: u64, disk: u64) -> Option<Header> {
    if &sector[0..8] != GPT_SIGNATURE {
        return None;
    }
    let size = u32_at(sector, 12) as usize;
    if !(92..=SECTOR).contains(&size) {
        return None;
    }
    let mut crc = Crc32::new();
    crc.update(&sector[..16]);
    crc.update(&[0; 4]);
    crc.update(&sector[20..size]);
    if crc.finish() != u32_at(sector, 16) || u64_at(sector, 24) != at {
        return None;
    }
    let header = Header {
        first_usable: u64_at(sector, 40),
        last_usable: u64_at(sector, 48),
        entries_at: u64_at(sector, 72),
        entries: u32_at(sector, 80),
        entry_size: u32_at(sector, 84),
        entries_crc: u32_at(sector, 88),
    };
    let sane = header.first_usable <= header.last_usable
        && header.last_usable < disk
        && header.entry_size >= 128
        && header.entry_size.is_power_of_two()
        && header.entry_size as usize <= SECTOR
        && header.entries <= 4096
        && header.entries_at >= 1
        && header.entries_at < disk;
    sane.then_some(header)
}

fn gpt(disk: u64, read: ReadSector<'_>, found: &mut KVec<Partition>) -> Result<(), Error> {
    let mut sector = [0u8; SECTOR];
    for at in [1, disk - 1] {
        read(at, &mut sector)?;
        let Some(header) = header(&sector, at, disk) else { continue };
        match gpt_entries(disk, &header, read, found) {
            Ok(()) => return Ok(()),
            Err(Error::Damaged(_)) => found.clear(),
            Err(err) => return Err(err),
        }
    }
    Err(Error::Damaged("neither GPT header nor its entries are intact"))
}

fn gpt_entries(
    disk: u64,
    header: &Header,
    read: ReadSector<'_>,
    found: &mut KVec<Partition>,
) -> Result<(), Error> {
    let size = header.entry_size as usize;
    let per_sector = SECTOR / size;
    let sectors = (header.entries as usize).div_ceil(per_sector) as u64;
    if !within(header.entries_at, sectors, disk) {
        return Err(Error::Damaged("the GPT entries reach past the end of the disk"));
    }
    let mut crc = Crc32::new();
    let mut sector = [0u8; SECTOR];
    let mut left = header.entries as usize;
    for s in 0..sectors {
        read(header.entries_at + s, &mut sector)?;
        let here = left.min(per_sector);
        crc.update(&sector[..here * size]);
        for i in 0..here {
            let entry = &sector[i * size..(i + 1) * size];
            let number = (s as usize * per_sector + i) as u32 + 1;
            let mut kind = [0u8; 16];
            kind.copy_from_slice(&entry[0..16]);
            if kind == [0; 16] {
                continue;
            }
            let (first, last) = (u64_at(entry, 32), u64_at(entry, 40));
            if first < header.first_usable || last > header.last_usable || last < first {
                return Err(Error::Damaged("a GPT partition is outside the usable sectors"));
            }
            let sectors = last - first + 1;
            if !within(first, sectors, disk) {
                return Err(Error::Damaged("a GPT partition reaches past the end of the disk"));
            }
            let mut name = [0u16; 36];
            for (j, c) in name.iter_mut().enumerate() {
                *c = u16::from_le_bytes([entry[56 + 2 * j], entry[57 + 2 * j]]);
            }
            if found.len() < MAX_PARTITIONS {
                mem::push(
                    found,
                    Partition { number, first, sectors, kind: Kind::Gpt(kind), name },
                )?;
            }
        }
        left -= here;
    }
    if crc.finish() != header.entries_crc {
        return Err(Error::Damaged("the GPT entries' CRC does not match"));
    }
    Ok(())
}

/// The CRC of a GPT header's first `size` bytes with its CRC field as 0,
/// for tests that build tables.
pub fn header_crc(header: &[u8]) -> u32 {
    let mut copy = [0u8; SECTOR];
    copy[..header.len()].copy_from_slice(header);
    copy[16..20].fill(0);
    crc32(&copy[..header.len()])
}
