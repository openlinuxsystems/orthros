// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! The firmware's SMBIOS tables (Phase 11e, ADR 0231): what the machine
//! is, as its maker named it, its firmware, its memory devices and its
//! processors.
//!
//! The firmware leaves an entry point where the system finds it: in the
//! UEFI configuration table, or on a 16-byte boundary of the BIOS area
//! from 0xf0000. Its 32-bit form, `_SM_` (SMBIOS 2.1 and later), gives
//! the structure table's address, length and count; its 64-bit form,
//! `_SM3_` (3.0 and later), the address and the most the table takes.
//! Each structure is a header (its type, length and handle), the rest of
//! its formatted area, then its strings, each ended by a zero byte and
//! the last by two; a field that names a string holds its number, from 1,
//! or 0 for none. Type 127 ends the table. Reference: the DMTF's System
//! Management BIOS Reference Specification (DSP0134), version 3.7.
//!
//! [`Ids`] reads the fields Linux keeps of the table, under the names of
//! its `/sys/class/dmi/id`; [`memory`] the memory arrays and devices,
//! and [`processor`] the processors. The architecture finds and copies
//! the table; this crate only reads its bytes, so its tests run on the
//! host.

#![cfg_attr(not(test), no_std)]

pub mod memory;
pub mod processor;

use core::fmt;

/// The 32-bit entry point's anchor, on a 16-byte boundary.
pub const ANCHOR_32: &[u8] = b"_SM_";
/// The 64-bit entry point's anchor, likewise.
pub const ANCHOR_64: &[u8] = b"_SM3_";
/// The most bytes an entry point takes: the 32-bit one's 31, its length
/// field being a byte.
pub const MAX_ENTRY: usize = 0x20;

/// Structure types this crate reads.
pub mod kind {
    /// BIOS information: the firmware's vendor, version and date.
    pub const BIOS: u8 = 0;
    /// System information: its maker, name, serial number and UUID.
    pub const SYSTEM: u8 = 1;
    /// Baseboard information: the mainboard's.
    pub const BOARD: u8 = 2;
    /// System enclosure or chassis.
    pub const CHASSIS: u8 = 3;
    /// Processor information.
    pub const PROCESSOR: u8 = 4;
    /// Physical memory array.
    pub const MEMORY_ARRAY: u8 = 16;
    /// Memory device: a module, or the chips of soldered memory.
    pub const MEMORY_DEVICE: u8 = 17;
    /// End of table.
    pub const END: u8 = 127;
}

/// An entry point: where the structure table is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Entry {
    /// The version of the specification the table follows: its major
    /// number.
    pub major: u8,
    /// Its minor number.
    pub minor: u8,
    /// Its revision (64-bit entry points only, else 0).
    pub docrev: u8,
    /// The structure table's physical address.
    pub address: u64,
    /// Its length: exact for a 32-bit entry point, the most it takes for
    /// a 64-bit one.
    pub length: u32,
    /// How many structures it holds (32-bit entry points only).
    pub count: Option<u16>,
    /// The entry point's own length, in bytes.
    pub size: usize,
}

/// Whether `bytes` sum to 0, as every checksum of the tables makes them.
fn sums_to_zero(bytes: &[u8]) -> bool {
    bytes.iter().fold(0u8, |sum, b| sum.wrapping_add(*b)) == 0
}

fn word(bytes: &[u8], at: usize) -> Option<u16> {
    Some(u16::from_le_bytes(bytes.get(at..at + 2)?.try_into().ok()?))
}

fn dword(bytes: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_le_bytes(bytes.get(at..at + 4)?.try_into().ok()?))
}

fn qword(bytes: &[u8], at: usize) -> Option<u64> {
    Some(u64::from_le_bytes(bytes.get(at..at + 8)?.try_into().ok()?))
}

impl Entry {
    /// The entry point at the start of `bytes`, of either form, if its
    /// anchor and checksums are right.
    pub fn parse(bytes: &[u8]) -> Option<Entry> {
        if bytes.starts_with(ANCHOR_64) {
            let size = usize::from(*bytes.get(6)?);
            if !(0x18..=MAX_ENTRY).contains(&size) || !sums_to_zero(bytes.get(..size)?) {
                return None;
            }
            return Some(Entry {
                major: bytes[7],
                minor: bytes[8],
                docrev: bytes[9],
                address: qword(bytes, 0x10)?,
                length: dword(bytes, 0x0c)?,
                count: None,
                size,
            });
        }
        if !bytes.starts_with(ANCHOR_32) {
            return None;
        }
        // Some firmware says 0x1e, a byte short (Linux takes it too).
        let size = usize::from(*bytes.get(5)?);
        if !(0x1e..=0x1f).contains(&size) || !sums_to_zero(bytes.get(..size)?) {
            return None;
        }
        // The intermediate entry point, `_DMI_`, has its own checksum.
        if bytes.get(0x10..0x15)? != b"_DMI_" || !sums_to_zero(bytes.get(0x10..0x1f)?) {
            return None;
        }
        // Versions some firmware get wrong, which Linux mends the same way.
        let (major, minor) = match (bytes[6], bytes[7]) {
            (2, 0x1f | 0x21) => (2, 3),
            (2, 0x33) => (2, 6),
            version => version,
        };
        Some(Entry {
            major,
            minor,
            docrev: 0,
            address: u64::from(dword(bytes, 0x18)?),
            length: u32::from(word(bytes, 0x16)?),
            count: Some(word(bytes, 0x1c)?),
            size,
        })
    }

    /// Its version as one number, `0x030700` for 3.7.0, to compare.
    pub fn version(&self) -> u32 {
        u32::from(self.major) << 16 | u32::from(self.minor) << 8 | u32::from(self.docrev)
    }
}

impl fmt::Display for Entry {
    /// `SMBIOS 3.7.0`, or `SMBIOS 2.8` for a 32-bit entry point, as Linux
    /// says it.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "SMBIOS {}.{}", self.major, self.minor)?;
        if self.count.is_none() {
            write!(f, ".{}", self.docrev)?;
        }
        Ok(())
    }
}

/// A string of a structure, as its bytes are. Shown, a byte that is not
/// part of UTF-8, or a control character, is `?`.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct Str<'a>(pub &'a [u8]);

impl fmt::Display for Str<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        use fmt::Write;
        for chunk in self.0.utf8_chunks() {
            for c in chunk.valid().chars() {
                f.write_char(if c.is_control() { '?' } else { c })?;
            }
            if !chunk.invalid().is_empty() {
                f.write_char('?')?;
            }
        }
        Ok(())
    }
}

impl fmt::Debug for Str<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "\"{self}\"")
    }
}

/// One structure of the table.
#[derive(Clone, Copy, Debug)]
pub struct Structure<'a> {
    /// Its type.
    pub kind: u8,
    /// Its handle, by which other structures name it.
    pub handle: u16,
    /// Its formatted area, the header included.
    formatted: &'a [u8],
    /// Its strings, each ended by a zero byte.
    strings: &'a [u8],
}

impl<'a> Structure<'a> {
    /// The length of its formatted area: which fields it has, by the
    /// version it follows.
    pub fn len(&self) -> usize {
        self.formatted.len()
    }

    /// Whether its formatted area is no more than its header.
    pub fn is_empty(&self) -> bool {
        self.formatted.len() <= 4
    }

    /// The byte at `offset` of its formatted area, if it has one there.
    pub fn byte(&self, offset: usize) -> Option<u8> {
        self.formatted.get(offset).copied()
    }

    /// The little-endian word at `offset`.
    pub fn word(&self, offset: usize) -> Option<u16> {
        word(self.formatted, offset)
    }

    /// The little-endian double word at `offset`.
    pub fn dword(&self, offset: usize) -> Option<u32> {
        dword(self.formatted, offset)
    }

    /// The `len` bytes at `offset`.
    pub fn bytes(&self, offset: usize, len: usize) -> Option<&'a [u8]> {
        self.formatted.get(offset..offset.checked_add(len)?)
    }

    /// The string the byte at `offset` names; none for 0, for a number
    /// past its strings, or for a string of spaces alone, as Linux has it.
    pub fn string(&self, offset: usize) -> Option<Str<'a>> {
        let number = usize::from(self.byte(offset)?);
        let text = self.strings.split(|b| *b == 0).nth(number.checked_sub(1)?)?;
        text.iter().any(|b| *b != b' ').then_some(Str(text))
    }
}

/// The structure table.
#[derive(Clone, Copy, Debug)]
pub struct Table<'a> {
    bytes: &'a [u8],
    /// How many structures the entry point says it has, if it says.
    count: Option<u16>,
}

impl<'a> Table<'a> {
    /// The table in `bytes`, of `count` structures if the entry point
    /// says how many.
    pub fn new(bytes: &'a [u8], count: Option<u16>) -> Table<'a> {
        Table { bytes, count }
    }

    /// Its structures, in order: up to the end-of-table structure, the
    /// count, the end of its bytes, or a structure cut short.
    pub fn structures(&self) -> Structures<'a> {
        Structures { bytes: self.bytes, at: 0, left: self.count.map_or(usize::MAX, usize::from) }
    }

    /// Its first structure of type `kind`.
    pub fn first(&self, kind: u8) -> Option<Structure<'a>> {
        self.structures().find(|s| s.kind == kind)
    }

    /// Its structures of type `kind`.
    pub fn of(&self, kind: u8) -> impl Iterator<Item = Structure<'a>> + 'a {
        self.structures().filter(move |s| s.kind == kind)
    }

    /// The bytes the structures take, up to the end of the last.
    pub fn used(&self) -> usize {
        let mut it = self.structures();
        while it.next().is_some() {}
        it.at
    }
}

/// The structures of a [`Table`].
pub struct Structures<'a> {
    bytes: &'a [u8],
    at: usize,
    left: usize,
}

impl<'a> Iterator for Structures<'a> {
    type Item = Structure<'a>;

    fn next(&mut self) -> Option<Structure<'a>> {
        if self.left == 0 {
            return None;
        }
        let rest = self.bytes.get(self.at..)?;
        let (&kind, &len) = (rest.first()?, rest.get(1)?);
        let len = usize::from(len);
        if len < 4 || rest.len() < len {
            self.left = 0;
            return None;
        }
        // The strings end at the first two zero bytes in a row; a
        // structure without strings has just the two.
        let Some(end) = rest[len..].windows(2).position(|pair| pair == [0, 0]) else {
            self.left = 0;
            return None;
        };
        let structure = Structure {
            kind,
            handle: word(rest, 2)?,
            formatted: &rest[..len],
            strings: &rest[len..=len + end],
        };
        self.at += len + end + 2;
        self.left = if kind == kind::END { 0 } else { self.left - 1 };
        Some(structure)
    }
}

/// A UUID as the system's structure gives it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Uuid {
    bytes: [u8; 16],
    /// Whether its first three fields are little-endian, as SMBIOS 2.6
    /// and later have them.
    little: bool,
}

impl fmt::Display for Uuid {
    /// `4c4c4544-0042-3910-8051-b7c04f4e3332`, as Linux writes it.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let b = &self.bytes;
        let order: [usize; 8] =
            if self.little { [3, 2, 1, 0, 5, 4, 7, 6] } else { [0, 1, 2, 3, 4, 5, 6, 7] };
        for (i, at) in order.into_iter().enumerate() {
            if i == 4 || i == 6 {
                f.write_str("-")?;
            }
            write!(f, "{:02x}", b[at])?;
        }
        write!(f, "-{:02x}{:02x}-", b[8], b[9])?;
        for byte in &b[10..] {
            write!(f, "{byte:02x}")?;
        }
        Ok(())
    }
}

/// A release, `major.minor`, of the firmware or of its embedded
/// controller.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Release(pub u8, pub u8);

impl fmt::Display for Release {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}.{}", self.0, self.1)
    }
}

/// What Linux keeps of the table, under its names in `/sys/class/dmi/id`:
/// each from the first structure of its type that has it.
#[derive(Clone, Copy, Debug, Default)]
pub struct Ids<'a> {
    /// The firmware's vendor.
    pub bios_vendor: Option<Str<'a>>,
    /// The firmware's version.
    pub bios_version: Option<Str<'a>>,
    /// The date the firmware was released, as `MM/DD/YYYY`.
    pub bios_date: Option<Str<'a>>,
    /// The firmware's release, from SMBIOS 2.4.
    pub bios_release: Option<Release>,
    /// The embedded controller's firmware release, from SMBIOS 2.4.
    pub ec_firmware_release: Option<Release>,
    /// Who made the machine.
    pub sys_vendor: Option<Str<'a>>,
    /// The machine's name.
    pub product_name: Option<Str<'a>>,
    /// The machine's version.
    pub product_version: Option<Str<'a>>,
    /// The machine's serial number, which only root reads.
    pub product_serial: Option<Str<'a>>,
    /// The machine's UUID, which only root reads.
    pub product_uuid: Option<Uuid>,
    /// The machine's SKU, from SMBIOS 2.4.
    pub product_sku: Option<Str<'a>>,
    /// The machine's family, from SMBIOS 2.4.
    pub product_family: Option<Str<'a>>,
    /// Who made the main board.
    pub board_vendor: Option<Str<'a>>,
    /// The main board's name.
    pub board_name: Option<Str<'a>>,
    /// The main board's version.
    pub board_version: Option<Str<'a>>,
    /// The main board's serial number, which only root reads.
    pub board_serial: Option<Str<'a>>,
    /// The main board's asset tag.
    pub board_asset_tag: Option<Str<'a>>,
    /// Who made the chassis.
    pub chassis_vendor: Option<Str<'a>>,
    /// The chassis's type, a number of the specification's: 3 a desktop,
    /// 10 a notebook (DSP0134's 7.4.1).
    pub chassis_type: Option<u8>,
    /// The chassis's version.
    pub chassis_version: Option<Str<'a>>,
    /// The chassis's serial number, which only root reads.
    pub chassis_serial: Option<Str<'a>>,
    /// The chassis's asset tag.
    pub chassis_asset_tag: Option<Str<'a>>,
}

/// Sets `slot` to `value` unless it has one already.
fn keep<T>(slot: &mut Option<T>, value: Option<T>) {
    if slot.is_none() {
        *slot = value;
    }
}

/// A release at `offset` of `s`, none where both its bytes are 0xff.
fn release(s: &Structure<'_>, offset: usize) -> Option<Release> {
    let (major, minor) = (s.byte(offset)?, s.byte(offset + 1)?);
    (major != 0xff || minor != 0xff).then_some(Release(major, minor))
}

impl<'a> Ids<'a> {
    /// What `table`, of SMBIOS `version` ([`Entry::version`]), says.
    pub fn read(table: &Table<'a>, version: u32) -> Ids<'a> {
        let mut ids = Ids::default();
        for s in table.structures() {
            match s.kind {
                kind::BIOS => {
                    keep(&mut ids.bios_vendor, s.string(0x04));
                    keep(&mut ids.bios_version, s.string(0x05));
                    keep(&mut ids.bios_date, s.string(0x08));
                    keep(&mut ids.bios_release, release(&s, 0x14));
                    keep(&mut ids.ec_firmware_release, release(&s, 0x16));
                }
                kind::SYSTEM => {
                    keep(&mut ids.sys_vendor, s.string(0x04));
                    keep(&mut ids.product_name, s.string(0x05));
                    keep(&mut ids.product_version, s.string(0x06));
                    keep(&mut ids.product_serial, s.string(0x07));
                    keep(&mut ids.product_uuid, uuid(&s, version));
                    keep(&mut ids.product_sku, s.string(0x19));
                    keep(&mut ids.product_family, s.string(0x1a));
                }
                kind::BOARD => {
                    keep(&mut ids.board_vendor, s.string(0x04));
                    keep(&mut ids.board_name, s.string(0x05));
                    keep(&mut ids.board_version, s.string(0x06));
                    keep(&mut ids.board_serial, s.string(0x07));
                    keep(&mut ids.board_asset_tag, s.string(0x08));
                }
                kind::CHASSIS => {
                    keep(&mut ids.chassis_vendor, s.string(0x04));
                    keep(&mut ids.chassis_type, s.byte(0x05).map(|t| t & 0x7f));
                    keep(&mut ids.chassis_version, s.string(0x06));
                    keep(&mut ids.chassis_serial, s.string(0x07));
                    keep(&mut ids.chassis_asset_tag, s.string(0x08));
                }
                _ => {}
            }
        }
        ids
    }

    /// Each field there is, under the name of Linux's file for it in
    /// `/sys/class/dmi/id`, in the order of the names. The serial numbers
    /// and the UUID, which Linux shows to root alone, are there only if
    /// `private`.
    pub fn fields(&self, private: bool) -> impl Iterator<Item = (&'static str, Field<'a>)> + 'a {
        let ids = *self;
        let text = |s: Option<Str<'a>>| s.map(Field::Text);
        let release = |r: Option<Release>| r.map(Field::Release);
        [
            ("bios_date", false, text(ids.bios_date)),
            ("bios_release", false, release(ids.bios_release)),
            ("bios_vendor", false, text(ids.bios_vendor)),
            ("bios_version", false, text(ids.bios_version)),
            ("board_asset_tag", false, text(ids.board_asset_tag)),
            ("board_name", false, text(ids.board_name)),
            ("board_serial", true, text(ids.board_serial)),
            ("board_vendor", false, text(ids.board_vendor)),
            ("board_version", false, text(ids.board_version)),
            ("chassis_asset_tag", false, text(ids.chassis_asset_tag)),
            ("chassis_serial", true, text(ids.chassis_serial)),
            ("chassis_type", false, ids.chassis_type.map(Field::Number)),
            ("chassis_vendor", false, text(ids.chassis_vendor)),
            ("chassis_version", false, text(ids.chassis_version)),
            ("ec_firmware_release", false, release(ids.ec_firmware_release)),
            ("product_family", false, text(ids.product_family)),
            ("product_name", false, text(ids.product_name)),
            ("product_serial", true, text(ids.product_serial)),
            ("product_sku", false, text(ids.product_sku)),
            ("product_uuid", true, ids.product_uuid.map(Field::Uuid)),
            ("product_version", false, text(ids.product_version)),
            ("sys_vendor", false, text(ids.sys_vendor)),
        ]
        .into_iter()
        .filter(move |(_, hidden, _)| private || !hidden)
        .filter_map(|(name, _, field)| Some((name, field?)))
    }
}

/// A value of [`Ids`], as [`Ids::fields`] gives it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Field<'a> {
    /// A string of the firmware's.
    Text(Str<'a>),
    /// A release, `major.minor`.
    Release(Release),
    /// A number: the chassis's type.
    Number(u8),
    /// The system's UUID.
    Uuid(Uuid),
}

impl fmt::Display for Field<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Field::Text(text) => write!(f, "{text}"),
            Field::Release(release) => write!(f, "{release}"),
            Field::Number(number) => write!(f, "{number}"),
            Field::Uuid(uuid) => write!(f, "{uuid}"),
        }
    }
}

/// The system's UUID, none where its bytes are all 0 (not set) or all
/// 0xff (none).
fn uuid(s: &Structure<'_>, version: u32) -> Option<Uuid> {
    let bytes: [u8; 16] = s.bytes(0x08, 16)?.try_into().ok()?;
    if bytes.iter().all(|b| *b == 0) || bytes.iter().all(|b| *b == 0xff) {
        return None;
    }
    Some(Uuid { bytes, little: version >= 0x020600 })
}

/// The name of chassis type `kind` (DSP0134's 7.4.1), as dmidecode and
/// the specification give it.
pub fn chassis_name(kind: u8) -> Option<&'static str> {
    const NAMES: [&str; 36] = [
        "Other",
        "Unknown",
        "Desktop",
        "Low Profile Desktop",
        "Pizza Box",
        "Mini Tower",
        "Tower",
        "Portable",
        "Laptop",
        "Notebook",
        "Hand Held",
        "Docking Station",
        "All In One",
        "Sub Notebook",
        "Space-saving",
        "Lunch Box",
        "Main Server Chassis",
        "Expansion Chassis",
        "Sub Chassis",
        "Bus Expansion Chassis",
        "Peripheral Chassis",
        "RAID Chassis",
        "Rack Mount Chassis",
        "Sealed-case PC",
        "Multi-system",
        "CompactPCI",
        "AdvancedTCA",
        "Blade",
        "Blade Enclosing",
        "Tablet",
        "Convertible",
        "Detachable",
        "IoT Gateway",
        "Embedded PC",
        "Mini PC",
        "Stick PC",
    ];
    NAMES.get(usize::from(kind).checked_sub(1)?).copied()
}

#[cfg(test)]
pub(crate) mod tests;
