// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! A partition table as rdisk(8) changes it: in memory, until it is
//! written ([`crate::write`]).
//!
//! Partitions are numbered as Linux and Radix number them: a GPT entry by
//! its place in the table from 1, an MBR's primary partitions 1 to 4 and
//! its logical ones from 5. Sectors are the disk's logical sectors, and a
//! partition's `last` is its last sector, as fdisk shows them. New
//! partitions start on a whole MiB unless asked otherwise, as fdisk's and
//! Windows' do, so that they fall on the boundaries of SSDs' pages and
//! erase blocks and of 4 KiB sectors.

use std::fmt;

use crate::guid::Guid;
use crate::rng::Rng;
use crate::types::Kind;

/// The kind of table.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Scheme {
    /// A GUID partition table (UEFI 2.10 chapter 5), with its protective
    /// MBR: up to 128 partitions, of any size.
    Gpt,
    /// A master boot record: up to four primary partitions within the
    /// first 2 TiB of a disk of 512-byte sectors.
    Mbr,
}

impl Scheme {
    /// Its name in layout files and rdisk's messages: `gpt` or `mbr`.
    pub fn name(self) -> &'static str {
        match self {
            Scheme::Gpt => "gpt",
            Scheme::Mbr => "mbr",
        }
    }
}

/// A partition.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Partition {
    /// Its number: 1 for the first entry.
    pub number: u32,
    /// Its first logical sector.
    pub first: u64,
    /// Its last logical sector.
    pub last: u64,
    /// What its entry says it holds.
    pub kind: Kind,
    /// Its unique GUID, in a GPT; zero in an MBR.
    pub guid: Guid,
    /// Its name, in a GPT: up to 36 UTF-16 units.
    pub name: String,
    /// Its attribute bits, in a GPT ([`ATTRIBUTES`]).
    pub attributes: u64,
    /// Whether its MBR entry marks it bootable (0x80).
    pub bootable: bool,
    /// Whether it is a logical partition of an MBR, in an extended one:
    /// shown and kept as it is, never changed.
    pub logical: bool,
}

impl Partition {
    /// How many logical sectors it has.
    pub fn sectors(&self) -> u64 {
        self.last - self.first + 1
    }
}

/// GPT's attribute bits, by the names layout files give them (UEFI 2.10
/// table 5.8, and Microsoft's for basic data partitions).
pub const ATTRIBUTES: [(&str, u32); 6] = [
    ("required", 0),
    ("no-block-io", 1),
    ("legacy-bios-bootable", 2),
    ("read-only", 60),
    ("hidden", 62),
    ("no-automount", 63),
];

/// The most UTF-16 units a GPT partition's name holds.
pub const NAME_UNITS: usize = 36;

/// A table that was changed in a way it cannot be, said in words.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Error(pub String);

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for Error {}

fn error<T>(text: impl Into<String>) -> Result<T, Error> {
    Err(Error(text.into()))
}

/// Where a new partition ends.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum End {
    /// After this many sectors.
    Sectors(u64),
    /// At this sector.
    Last(u64),
    /// At the end of the free space it starts in.
    Rest,
}

/// A partition to add.
#[derive(Clone, Debug)]
pub struct New {
    /// Its number, or the first free one.
    pub number: Option<u32>,
    /// Its first sector, or the first whole MiB of the first free space it
    /// fits in.
    pub first: Option<u64>,
    /// Where it ends.
    pub end: End,
    /// What its entry says it holds.
    pub kind: Kind,
    /// Its name, in a GPT.
    pub name: String,
    /// Its unique GUID, in a GPT, or a random one.
    pub guid: Option<Guid>,
    /// Its attribute bits, in a GPT.
    pub attributes: u64,
    /// Whether it is bootable, in an MBR.
    pub bootable: bool,
}

impl New {
    /// A partition of `kind` that ends as `end` says, with nothing else
    /// asked.
    pub fn of(kind: Kind, end: End) -> New {
        New {
            number: None,
            first: None,
            end,
            kind,
            name: String::new(),
            guid: None,
            attributes: 0,
            bootable: false,
        }
    }
}

/// A partition table.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Table {
    /// GPT or MBR.
    pub scheme: Scheme,
    /// The disk's logical sector size: 512 or 4096.
    pub sector_size: u32,
    /// How many logical sectors the disk has.
    pub disk_sectors: u64,
    /// A GPT's disk GUID.
    pub disk_guid: Guid,
    /// An MBR's disk signature, at byte 440.
    pub signature: u32,
    /// How many entries a GPT has room for: 128.
    pub entry_count: u32,
    /// The size of each GPT entry: 128 bytes.
    pub entry_size: u32,
    /// The first sector partitions may have.
    pub first_usable: u64,
    /// The last sector partitions may have.
    pub last_usable: u64,
    /// The partitions, by number.
    pub partitions: Vec<Partition>,
    /// The first 440 bytes of the disk's first sector, a boot loader's
    /// (GRUB's `boot.img`), kept when the table is written.
    pub boot_code: Vec<u8>,
}

/// The entries a GPT has room for, as every tool makes one.
pub const GPT_ENTRIES: u32 = 128;
/// The size of a GPT entry.
pub const GPT_ENTRY_SIZE: u32 = 128;

impl Table {
    /// A new, empty table of `scheme` for a disk of `disk_sectors`
    /// logical sectors of `sector_size` bytes, with a random GUID or
    /// signature.
    ///
    /// # Errors
    ///
    /// If the disk is too small for a table and a partition of 1 MiB.
    pub fn new(scheme: Scheme, sector_size: u32, disk_sectors: u64, rng: &mut Rng) -> Result<Table, Error> {
        if !matches!(sector_size, 512 | 1024 | 2048 | 4096) {
            return error(format!("rdisk does not know disks of {sector_size}-byte sectors"));
        }
        let mut table = Table {
            scheme,
            sector_size,
            disk_sectors,
            disk_guid: Guid::ZERO,
            signature: 0,
            entry_count: GPT_ENTRIES,
            entry_size: GPT_ENTRY_SIZE,
            first_usable: 0,
            last_usable: 0,
            partitions: Vec::new(),
            boot_code: vec![0; 440],
        };
        match scheme {
            Scheme::Gpt => {
                table.disk_guid = Guid::random(rng);
                let entries = table.entry_sectors();
                table.first_usable = 2 + entries;
                table.last_usable = disk_sectors.saturating_sub(2 + entries);
            }
            Scheme::Mbr => {
                table.signature = rng.next_u32() | 1;
                table.first_usable = 1;
                table.last_usable = disk_sectors.saturating_sub(1).min(u64::from(u32::MAX));
            }
        }
        // Room for the table, a partition of a MiB on a whole MiB, and the
        // backup at the end.
        if table.last_usable < 2 * table.alignment() + table.first_usable {
            return error(format!(
                "the disk is too small for a partition table ({})",
                crate::size::human(disk_sectors * u64::from(sector_size))
            ));
        }
        Ok(table)
    }

    /// The sectors a GPT's entries take: 32 of 512 bytes.
    pub fn entry_sectors(&self) -> u64 {
        (u64::from(self.entry_count) * u64::from(self.entry_size)).div_ceil(u64::from(self.sector_size))
    }

    /// The sectors in a MiB, which new partitions start on.
    pub fn alignment(&self) -> u64 {
        ((1 << 20) / u64::from(self.sector_size)).max(1)
    }

    /// The partitions the table has room for, by number.
    pub fn slots(&self) -> u32 {
        match self.scheme {
            Scheme::Gpt => self.entry_count,
            Scheme::Mbr => 4,
        }
    }

    /// Partition `number`.
    pub fn get(&self, number: u32) -> Option<&Partition> {
        self.partitions.iter().find(|p| p.number == number)
    }

    fn get_mut(&mut self, number: u32) -> Result<&mut Partition, Error> {
        let Some(found) = self.partitions.iter_mut().find(|p| p.number == number) else {
            return error(format!("there is no partition {number}"));
        };
        if found.logical {
            return error(format!(
                "partition {number} is a logical partition, which rdisk keeps as it is; it \
                 changes only an MBR's four primary partitions"
            ));
        }
        Ok(found)
    }

    /// The first number free, if one is.
    pub fn free_number(&self) -> Option<u32> {
        (1..=self.slots()).find(|&n| self.get(n).is_none())
    }

    /// The runs of sectors no partition has, first and last, in order.
    /// What an extended partition covers counts as taken.
    pub fn free(&self) -> Vec<(u64, u64)> {
        let mut taken: Vec<(u64, u64)> =
            self.partitions.iter().filter(|p| !p.logical).map(|p| (p.first, p.last)).collect();
        taken.sort_unstable();
        let mut free = Vec::new();
        let mut at = self.first_usable;
        for (first, last) in taken {
            if first > at {
                free.push((at, (first - 1).min(self.last_usable)));
            }
            at = at.max(last.saturating_add(1));
            if at > self.last_usable {
                break;
            }
        }
        if at <= self.last_usable {
            free.push((at, self.last_usable));
        }
        free.retain(|&(first, last)| first <= last);
        free
    }

    /// `sector` rounded up to a whole MiB.
    fn aligned(&self, sector: u64) -> u64 {
        sector.div_ceil(self.alignment()).saturating_mul(self.alignment())
    }

    /// Adds `new`; returns its number.
    ///
    /// # Errors
    ///
    /// If its number is taken or out of range, it does not fit in free
    /// space, or its name is too long.
    pub fn add(&mut self, new: New, rng: &mut Rng) -> Result<u32, Error> {
        let number = match new.number {
            Some(n) if n == 0 || n > self.slots() => {
                return error(format!(
                    "a {} has partitions 1 to {}, not {n}",
                    self.scheme_words(),
                    self.slots()
                ));
            }
            Some(n) if self.get(n).is_some() => {
                return error(format!("partition {n} is there already"));
            }
            Some(n) => n,
            None => match self.free_number() {
                Some(n) => n,
                None => {
                    return error(format!(
                        "the {} has no room for another partition: it has {} already",
                        self.scheme_words(),
                        self.slots()
                    ));
                }
            },
        };
        if new.kind.is_extended() {
            return error(
                "rdisk makes no extended partitions: an MBR holds four primary ones, and a GPT \
                 128 of any size",
            );
        }
        if let Kind::Mbr(0) = new.kind {
            return error("an MBR partition of type 0 would be empty");
        }
        match (self.scheme, new.kind) {
            (Scheme::Gpt, Kind::Gpt(guid)) if !guid.is_zero() => {}
            (Scheme::Mbr, Kind::Mbr(_)) => {}
            _ => return error(format!("that is not a type of a {}", self.scheme_words())),
        }
        let free = self.free();
        let (first, room_end) = match new.first {
            Some(first) => {
                let Some(&(_, end)) = free.iter().find(|&&(a, b)| a <= first && first <= b) else {
                    return error(format!(
                        "sector {first} is not free: it is {}",
                        self.where_is(first)
                    ));
                };
                (first, end)
            }
            None => {
                let wanted = match new.end {
                    End::Sectors(n) => n,
                    End::Last(_) | End::Rest => 1,
                };
                let found = free.iter().find_map(|&(a, b)| {
                    let start = self.aligned(a);
                    (start <= b && b - start + 1 >= wanted).then_some((start, b))
                });
                match found {
                    Some(found) => found,
                    None => {
                        return error(format!(
                            "there is no free space of {} on a whole MiB",
                            crate::size::human(wanted.saturating_mul(u64::from(self.sector_size)))
                        ));
                    }
                }
            }
        };
        let last = match new.end {
            End::Sectors(0) => return error("a partition needs at least one sector"),
            End::Sectors(n) => first.saturating_add(n - 1),
            End::Last(last) => last,
            End::Rest => room_end,
        };
        if last < first {
            return error(format!("the last sector, {last}, is before the first, {first}"));
        }
        if last > room_end {
            return error(format!(
                "the partition does not fit: from sector {first}, the free space ends at {room_end} \
                 ({} free)",
                crate::size::human((room_end - first + 1) * u64::from(self.sector_size))
            ));
        }
        if self.scheme == Scheme::Mbr && last - first + 1 > u64::from(u32::MAX) {
            return error("an MBR partition holds at most 2 TiB of 512-byte sectors");
        }
        let name_units = new.name.encode_utf16().count();
        if self.scheme == Scheme::Gpt && name_units > NAME_UNITS {
            return error(format!("a name holds at most {NAME_UNITS} characters"));
        }
        let partition = Partition {
            number,
            first,
            last,
            kind: new.kind,
            guid: match self.scheme {
                Scheme::Gpt => new.guid.unwrap_or_else(|| Guid::random(rng)),
                Scheme::Mbr => Guid::ZERO,
            },
            name: if self.scheme == Scheme::Gpt { new.name } else { String::new() },
            attributes: if self.scheme == Scheme::Gpt { new.attributes } else { 0 },
            bootable: self.scheme == Scheme::Mbr && new.bootable,
            logical: false,
        };
        let at = self.partitions.iter().position(|p| p.number > number).unwrap_or(self.partitions.len());
        self.partitions.insert(at, partition);
        Ok(number)
    }

    /// What `sector` is in, for messages.
    fn where_is(&self, sector: u64) -> String {
        if sector < self.first_usable {
            return format!("before the first usable sector, {}", self.first_usable);
        }
        if sector > self.last_usable {
            return format!("past the last usable sector, {}", self.last_usable);
        }
        match self.partitions.iter().find(|p| p.first <= sector && sector <= p.last) {
            Some(p) => format!("in partition {}", p.number),
            None => "free".to_string(),
        }
    }

    fn scheme_words(&self) -> &'static str {
        match self.scheme {
            Scheme::Gpt => "GPT",
            Scheme::Mbr => "MBR",
        }
    }

    /// Takes partition `number` out; an extended one's logical
    /// partitions go with it.
    ///
    /// # Errors
    ///
    /// If there is none, or it is a logical one.
    pub fn delete(&mut self, number: u32) -> Result<Partition, Error> {
        self.get_mut(number)?;
        let at = self.partitions.iter().position(|p| p.number == number).unwrap_or_default();
        let gone = self.partitions.remove(at);
        if gone.kind.is_extended() {
            self.partitions.retain(|p| !p.logical);
        }
        Ok(gone)
    }

    /// Sets partition `number`'s type.
    ///
    /// # Errors
    ///
    /// If there is none, it is logical, or `kind` is not of this table.
    pub fn set_kind(&mut self, number: u32, kind: Kind) -> Result<(), Error> {
        let scheme = self.scheme;
        let words = self.scheme_words();
        let partition = self.get_mut(number)?;
        match (scheme, kind) {
            (Scheme::Gpt, Kind::Gpt(guid)) if !guid.is_zero() => {}
            (Scheme::Mbr, Kind::Mbr(byte)) if byte != 0 && !kind.is_extended() => {}
            (Scheme::Mbr, Kind::Mbr(_)) => {
                return error("rdisk sets no MBR type of 0 or of an extended partition");
            }
            _ => return error(format!("that is not a type of a {words}")),
        }
        if partition.kind.is_extended() {
            return error(format!("partition {number} is extended: it holds logical partitions"));
        }
        partition.kind = kind;
        Ok(())
    }

    /// Sets partition `number`'s name, in a GPT.
    ///
    /// # Errors
    ///
    /// If there is none, the table is an MBR, or the name is too long.
    pub fn set_name(&mut self, number: u32, name: &str) -> Result<(), Error> {
        if self.scheme != Scheme::Gpt {
            return error("MBR partitions have no names; a GPT's do");
        }
        if name.encode_utf16().count() > NAME_UNITS {
            return error(format!("a name holds at most {NAME_UNITS} characters"));
        }
        if name.contains('\0') {
            return error("a name holds no NUL");
        }
        self.get_mut(number)?.name = name.to_string();
        Ok(())
    }

    /// Sets whether partition `number` is bootable, in an MBR.
    ///
    /// # Errors
    ///
    /// If there is none, or the table is a GPT.
    pub fn set_bootable(&mut self, number: u32, bootable: bool) -> Result<(), Error> {
        if self.scheme != Scheme::Mbr {
            return error(
                "a GPT marks no partition bootable; set the legacy-bios-bootable attribute with a \
                 layout file instead",
            );
        }
        self.get_mut(number)?.bootable = bootable;
        Ok(())
    }

    /// Sets partition `number`'s unique GUID, in a GPT.
    ///
    /// # Errors
    ///
    /// If there is none, or the table is an MBR.
    pub fn set_guid(&mut self, number: u32, guid: Guid) -> Result<(), Error> {
        if self.scheme != Scheme::Gpt || guid.is_zero() {
            return error("only a GPT partition has a GUID, and it is not zero");
        }
        self.get_mut(number)?.guid = guid;
        Ok(())
    }

    /// Sets partition `number`'s attribute bits, in a GPT.
    ///
    /// # Errors
    ///
    /// If there is none, or the table is an MBR.
    pub fn set_attributes(&mut self, number: u32, attributes: u64) -> Result<(), Error> {
        if self.scheme != Scheme::Gpt {
            return error("only GPT partitions have attributes");
        }
        self.get_mut(number)?.attributes = attributes;
        Ok(())
    }

    /// What is wrong with the table, worst first, in words: partitions
    /// that overlap or reach outside the usable sectors, and then what
    /// works but is slower or odd: partitions not on a whole MiB, more
    /// than one bootable, GUIDs used twice.
    pub fn verify(&self) -> Vec<String> {
        let mut problems = Vec::new();
        let primary: Vec<&Partition> = self.partitions.iter().filter(|p| !p.logical).collect();
        for p in &self.partitions {
            if p.last < p.first {
                problems.push(format!("Partition {} ends before it starts.", p.number));
            } else if p.first < self.first_usable || p.last > self.last_usable {
                problems.push(format!(
                    "Partition {} is not within the usable sectors {} to {}.",
                    p.number, self.first_usable, self.last_usable
                ));
            }
        }
        for (i, a) in primary.iter().enumerate() {
            for b in &primary[i + 1..] {
                if a.first <= b.last && b.first <= a.last {
                    problems.push(format!("Partitions {} and {} overlap.", a.number, b.number));
                }
            }
        }
        let logical: Vec<&Partition> = self.partitions.iter().filter(|p| p.logical).collect();
        for (i, a) in logical.iter().enumerate() {
            for b in &logical[i + 1..] {
                if a.first <= b.last && b.first <= a.last {
                    problems.push(format!("Partitions {} and {} overlap.", a.number, b.number));
                }
            }
        }
        for p in &self.partitions {
            if !p.first.is_multiple_of(self.alignment()) && !p.logical {
                problems.push(format!(
                    "Partition {} does not start on a whole MiB (sector {}), which may make it \
                     slower.",
                    p.number, p.first
                ));
            }
        }
        if self.scheme == Scheme::Mbr {
            let bootable = self.partitions.iter().filter(|p| p.bootable).count();
            if bootable > 1 {
                problems.push(format!("{bootable} partitions are marked bootable; one should be."));
            }
            let limit = u64::from(u32::MAX) + 1;
            if self.disk_sectors > limit {
                problems.push(format!(
                    "An MBR reaches only the first {} of this disk; a GPT reaches all of it.",
                    crate::size::human(limit * u64::from(self.sector_size))
                ));
            }
        }
        if self.scheme == Scheme::Gpt {
            for (i, a) in self.partitions.iter().enumerate() {
                if a.guid.is_zero() {
                    problems.push(format!("Partition {} has no GUID.", a.number));
                }
                if self.partitions[i + 1..].iter().any(|b| b.guid == a.guid) {
                    problems.push(format!("Partition {}'s GUID is used twice.", a.number));
                }
            }
        }
        problems
    }

    /// How many usable sectors no partition has.
    pub fn unallocated(&self) -> u64 {
        self.free().iter().map(|&(a, b)| b - a + 1).sum()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{self, Kind};

    fn kind(alias: &str, scheme: Scheme) -> Kind {
        types::parse(alias, scheme).unwrap()
    }

    #[test]
    fn a_gpt_has_room_for_its_backup() {
        let mut rng = Rng::seeded(1);
        let table = Table::new(Scheme::Gpt, 512, 41_943_040, &mut rng).unwrap();
        assert_eq!((table.first_usable, table.last_usable), (34, 41_943_006));
        assert_eq!(table.free(), vec![(34, 41_943_006)]);
        let table = Table::new(Scheme::Gpt, 4096, 1 << 20, &mut rng).unwrap();
        assert_eq!((table.first_usable, table.last_usable, table.alignment()), (6, (1 << 20) - 6, 256));
        let table = Table::new(Scheme::Mbr, 512, 1 << 33, &mut rng).unwrap();
        assert_eq!((table.first_usable, table.last_usable), (1, u64::from(u32::MAX)));
        assert!(Table::new(Scheme::Gpt, 512, 4096, &mut rng).is_err());
    }

    #[test]
    fn partitions_start_on_a_whole_mib_and_fill_the_rest() {
        let mut rng = Rng::seeded(2);
        let mut table = Table::new(Scheme::Gpt, 512, 41_943_040, &mut rng).unwrap();
        let boot = New::of(kind("bios-boot", Scheme::Gpt), End::Sectors(2048));
        assert_eq!(table.add(boot, &mut rng), Ok(1));
        let efi = New::of(kind("efi", Scheme::Gpt), End::Sectors(512 << 11));
        assert_eq!(table.add(efi, &mut rng), Ok(2));
        let root = New::of(kind("dukovo-root", Scheme::Gpt), End::Rest);
        assert_eq!(table.add(root, &mut rng), Ok(3));
        let spans: Vec<(u64, u64)> = table.partitions.iter().map(|p| (p.first, p.last)).collect();
        assert_eq!(spans, vec![(2048, 4095), (4096, 1_052_671), (1_052_672, 41_943_006)]);
        assert!(table.free().is_empty() || table.free() == vec![(34, 2047)]);
        assert!(table.verify().is_empty(), "{:?}", table.verify());
        let more = New::of(kind("linux", Scheme::Gpt), End::Sectors(2048));
        assert!(table.add(more, &mut rng).unwrap_err().0.contains("no free space"));
        table.delete(2).unwrap();
        let mut again = New::of(kind("efi", Scheme::Gpt), End::Rest);
        again.number = Some(9);
        assert_eq!(table.add(again, &mut rng), Ok(9));
        assert_eq!(table.get(9).map(|p| (p.first, p.last)), Some((4096, 1_052_671)));
        assert_eq!(table.partitions.iter().map(|p| p.number).collect::<Vec<_>>(), [1, 3, 9]);
    }

    #[test]
    fn mistakes_are_refused_with_reasons() {
        let mut rng = Rng::seeded(3);
        let mut table = Table::new(Scheme::Mbr, 512, 1 << 21, &mut rng).unwrap();
        for _ in 0..4 {
            table.add(New::of(Kind::Mbr(0x83), End::Sectors(2048)), &mut rng).unwrap();
        }
        let fifth = table.add(New::of(Kind::Mbr(0x83), End::Sectors(2048)), &mut rng);
        assert!(fifth.unwrap_err().0.contains("no room"));
        table.delete(4).unwrap();
        let extended = table.add(New::of(Kind::Mbr(0x05), End::Rest), &mut rng);
        assert!(extended.unwrap_err().0.contains("extended"));
        let mut inside = New::of(Kind::Mbr(0x83), End::Rest);
        inside.first = Some(3000);
        assert!(table.add(inside, &mut rng).unwrap_err().0.contains("in partition 1"));
        assert!(table.set_name(1, "EFI").unwrap_err().0.contains("no names"));
        assert!(table.set_kind(1, Kind::Gpt(Guid::ZERO)).is_err());
        table.set_bootable(1, true).unwrap();
        table.set_bootable(2, true).unwrap();
        assert!(table.verify().iter().any(|p| p.contains("2 partitions are marked bootable")));
        let mut gpt = Table::new(Scheme::Gpt, 512, 1 << 21, &mut rng).unwrap();
        gpt.add(New::of(kind("linux", Scheme::Gpt), End::Rest), &mut rng).unwrap();
        assert!(gpt.set_name(1, &"x".repeat(37)).is_err());
        gpt.set_name(1, "Dukovo").unwrap();
        assert_eq!(gpt.get(1).unwrap().name, "Dukovo");
    }

    #[test]
    fn verify_finds_overlaps_and_odd_starts() {
        let mut rng = Rng::seeded(4);
        let mut table = Table::new(Scheme::Gpt, 512, 1 << 21, &mut rng).unwrap();
        table.add(New::of(kind("linux", Scheme::Gpt), End::Sectors(4096)), &mut rng).unwrap();
        let mut second = table.partitions[0].clone();
        second.number = 2;
        second.first = 3000;
        second.last = 9000;
        second.guid = table.partitions[0].guid;
        table.partitions.push(second);
        let problems = table.verify();
        assert!(problems.iter().any(|p| p == "Partitions 1 and 2 overlap."), "{problems:?}");
        assert!(problems.iter().any(|p| p.contains("does not start on a whole MiB")));
        assert!(problems.iter().any(|p| p.contains("GUID is used twice")));
    }
}
