// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Directory entries (the specification's "Directory Structure").
//!
//! A directory is an array of 32-byte entries. The first byte of each is
//! its type: the low five bits a code, then whether it is benign (one a
//! driver may skip if it does not know it) and whether it is secondary,
//! and the top bit whether it is in use. A primary entry starts a set and
//! says how many secondary entries follow it; a removed set has the top
//! bit cleared in each of its entries, and an entry of type 0 ends the
//! directory: it and every entry after it are unused.
//!
//! A file or directory is a set: its file entry (attributes and times),
//! its stream extension (where its clusters are, its size, the length
//! and a hash of its name) and its name, fifteen UTF-16 units to each
//! file name entry. The file entry carries a checksum of the whole set,
//! so a set half written is told from a whole one. The root directory
//! also holds the allocation bitmap's entry, the up-case table's and the
//! volume label's.
//!
//! Times are FAT's dates and times packed into 32 bits, in two-second
//! steps from 1980, with hundredths of a second added and an offset from
//! UTC. Radix writes UTC and says so.

use radix_fat::time::{from_fat, to_fat};

/// The size of a directory entry.
pub const ENTRY: usize = 32;

/// The types of entries, in use.
pub mod kind {
    /// The end of the directory (type 0: not in use).
    pub const END: u8 = 0x00;
    /// The allocation bitmap.
    pub const BITMAP: u8 = 0x81;
    /// The up-case table.
    pub const UPCASE: u8 = 0x82;
    /// The volume label.
    pub const LABEL: u8 = 0x83;
    /// A file or directory: its attributes and times.
    pub const FILE: u8 = 0x85;
    /// A file's stream extension: its clusters, size and name's length.
    pub const STREAM: u8 = 0xc0;
    /// A piece of a file's name.
    pub const NAME: u8 = 0xc1;
}

/// The bit of an entry's type that says it is in use.
pub const IN_USE: u8 = 0x80;
/// The bit that says it is secondary: part of the set of the primary
/// entry before it.
pub const SECONDARY: u8 = 0x40;

/// Attribute bits of a file entry.
pub mod attr {
    /// The file should not be written.
    pub const READ_ONLY: u16 = 0x01;
    /// The file is hidden from listings (Radix lists it anyway).
    pub const HIDDEN: u16 = 0x02;
    /// The file belongs to the system.
    pub const SYSTEM: u16 = 0x04;
    /// The entry is a directory.
    pub const DIRECTORY: u16 = 0x10;
    /// The file changed since it was backed up.
    pub const ARCHIVE: u16 = 0x20;
}

/// Bits of a stream extension's flags.
pub mod stream {
    /// Clusters may be allocated to it: always set.
    pub const ALLOCATION_POSSIBLE: u8 = 0x01;
    /// Its clusters lie in one run and the table has no entries for
    /// them.
    pub const NO_FAT_CHAIN: u8 = 0x02;
}

/// UTF-16 units in one file name entry.
pub const PER_NAME: usize = 15;
/// The most UTF-16 units of a name.
pub const NAME_MAX: usize = 255;
/// The most entries of a set Radix writes: the file entry, the stream
/// extension and the name's.
pub const MAX_SET: usize = 2 + NAME_MAX.div_ceil(PER_NAME);

/// A time as an entry holds it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Stamp {
    /// The date in the high 16 bits and the time in the low, as FAT has
    /// them.
    pub stamp: u32,
    /// Hundredths of a second added to it, 0 to 199.
    pub hundredths: u8,
    /// The offset from UTC: the top bit says it is given, the rest is a
    /// signed number of 15-minute steps.
    pub utc: u8,
}

impl Stamp {
    /// The stamp for `ns` nanoseconds since 1970, in UTC; `fine` keeps
    /// hundredths of a second (an access time has none).
    pub fn of(ns: u64, fine: bool) -> Stamp {
        let (date, time) = to_fat(ns);
        let base = from_fat(date, time);
        let hundredths = if fine && base != 0 {
            (ns.saturating_sub(base) / 10_000_000).min(199) as u8
        } else {
            0
        };
        Stamp { stamp: (u32::from(date) << 16) | u32::from(time), hundredths, utc: 0x80 }
    }

    /// Nanoseconds since 1970 of the stamp, in UTC; 0 if it was never
    /// set. A stamp without an offset is taken for UTC, as Linux does
    /// unless told otherwise.
    pub fn ns(&self) -> u64 {
        let base = from_fat((self.stamp >> 16) as u16, self.stamp as u16);
        if base == 0 {
            return 0;
        }
        let local = base + u64::from(self.hundredths.min(199)) * 10_000_000;
        if self.utc & 0x80 == 0 {
            return local;
        }
        // Seven bits, signed: -64 to 63 quarters of an hour.
        let quarters = i64::from(((self.utc << 1) as i8) >> 1);
        let shift = quarters * 15 * 60 * 1_000_000_000;
        (local as i64).saturating_sub(shift).max(0) as u64
    }
}

fn u16_at(b: &[u8], at: usize) -> u16 {
    u16::from_le_bytes([b[at], b[at + 1]])
}

fn u32_at(b: &[u8], at: usize) -> u32 {
    u32::from_le_bytes([b[at], b[at + 1], b[at + 2], b[at + 3]])
}

fn u64_at(b: &[u8], at: usize) -> u64 {
    let mut bytes = [0u8; 8];
    bytes.copy_from_slice(&b[at..at + 8]);
    u64::from_le_bytes(bytes)
}

/// A file entry: the primary entry of a file's set.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct File {
    /// How many secondary entries follow it.
    pub secondaries: u8,
    /// The checksum of the set.
    pub checksum: u16,
    /// Attribute bits.
    pub attributes: u16,
    /// When it was made.
    pub created: Stamp,
    /// When its contents last changed.
    pub modified: Stamp,
    /// When it was last read.
    pub accessed: Stamp,
}

impl File {
    /// The entry in `b`.
    pub fn decode(b: &[u8; ENTRY]) -> File {
        File {
            secondaries: b[1],
            checksum: u16_at(b, 2),
            attributes: u16_at(b, 4),
            created: Stamp { stamp: u32_at(b, 8), hundredths: b[20], utc: b[22] },
            modified: Stamp { stamp: u32_at(b, 12), hundredths: b[21], utc: b[23] },
            accessed: Stamp { stamp: u32_at(b, 16), hundredths: 0, utc: b[24] },
        }
    }

    /// The entry's bytes.
    pub fn encode(&self) -> [u8; ENTRY] {
        let mut b = [0u8; ENTRY];
        b[0] = kind::FILE;
        b[1] = self.secondaries;
        b[2..4].copy_from_slice(&self.checksum.to_le_bytes());
        b[4..6].copy_from_slice(&self.attributes.to_le_bytes());
        b[8..12].copy_from_slice(&self.created.stamp.to_le_bytes());
        b[12..16].copy_from_slice(&self.modified.stamp.to_le_bytes());
        b[16..20].copy_from_slice(&self.accessed.stamp.to_le_bytes());
        b[20] = self.created.hundredths;
        b[21] = self.modified.hundredths;
        b[22] = self.created.utc;
        b[23] = self.modified.utc;
        b[24] = self.accessed.utc;
        b
    }

    /// Whether it is a directory.
    pub fn is_dir(&self) -> bool {
        self.attributes & attr::DIRECTORY != 0
    }
}

/// A stream extension: where a file's data is.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Stream {
    /// Its flags ([`stream`]).
    pub flags: u8,
    /// The name's length in UTF-16 units.
    pub name_len: u8,
    /// The hash of the name in upper case ([`name_hash`]).
    pub hash: u16,
    /// How many of its bytes have been written; those after read as
    /// zeros, whatever the disk holds there.
    pub valid: u64,
    /// Its first cluster, 0 when it has none.
    pub first: u32,
    /// Its size in bytes; a directory's is that of its clusters.
    pub size: u64,
}

impl Stream {
    /// The entry in `b`.
    pub fn decode(b: &[u8; ENTRY]) -> Stream {
        Stream {
            flags: b[1],
            name_len: b[3],
            hash: u16_at(b, 4),
            valid: u64_at(b, 8),
            first: u32_at(b, 20),
            size: u64_at(b, 24),
        }
    }

    /// The entry's bytes.
    pub fn encode(&self) -> [u8; ENTRY] {
        let mut b = [0u8; ENTRY];
        b[0] = kind::STREAM;
        b[1] = self.flags;
        b[3] = self.name_len;
        b[4..6].copy_from_slice(&self.hash.to_le_bytes());
        b[8..16].copy_from_slice(&self.valid.to_le_bytes());
        b[20..24].copy_from_slice(&self.first.to_le_bytes());
        b[24..32].copy_from_slice(&self.size.to_le_bytes());
        b
    }

    /// Whether its clusters lie in one run, without table entries.
    pub fn contiguous(&self) -> bool {
        self.flags & stream::NO_FAT_CHAIN != 0
    }
}

/// The units of a file name entry.
pub fn name_units(b: &[u8; ENTRY]) -> [u16; PER_NAME] {
    core::array::from_fn(|i| u16_at(b, 2 + 2 * i))
}

/// A file name entry holding `units` (up to [`PER_NAME`], the rest 0).
pub fn name_entry(units: &[u16]) -> [u8; ENTRY] {
    let mut b = [0u8; ENTRY];
    b[0] = kind::NAME;
    for (i, &unit) in units.iter().take(PER_NAME).enumerate() {
        b[2 + 2 * i..4 + 2 * i].copy_from_slice(&unit.to_le_bytes());
    }
    b
}

/// The address of the 32-bit field at 20 of an entry: the bitmap's,
/// the up-case table's and a stream's first cluster.
pub fn first_cluster(b: &[u8; ENTRY]) -> u32 {
    u32_at(b, 20)
}

/// The 64-bit field at 24: their size.
pub fn data_length(b: &[u8; ENTRY]) -> u64 {
    u64_at(b, 24)
}

/// The up-case table's checksum, in its entry.
pub fn table_checksum(b: &[u8; ENTRY]) -> u32 {
    u32_at(b, 4)
}

/// Adds `entry`, the `n`th of a set (from 0), to the set's checksum
/// `sum`: the checksum's own two bytes in the file entry are left out.
pub fn add_to_checksum(sum: u16, n: usize, entry: &[u8; ENTRY]) -> u16 {
    entry.iter().enumerate().fold(sum, |sum, (i, &b)| {
        if n == 0 && (i == 2 || i == 3) {
            sum
        } else {
            sum.rotate_right(1).wrapping_add(u16::from(b))
        }
    })
}

/// The checksum of the set `entries`.
pub fn set_checksum(entries: &[[u8; ENTRY]]) -> u16 {
    entries.iter().enumerate().fold(0, |sum, (n, entry)| add_to_checksum(sum, n, entry))
}

/// Writes `entries[0]`'s checksum field with the set's checksum.
pub fn seal(entries: &mut [[u8; ENTRY]]) {
    let sum = set_checksum(entries);
    if let Some(first) = entries.first_mut() {
        first[2..4].copy_from_slice(&sum.to_le_bytes());
    }
}

/// The hash of a name in upper case that its stream extension carries:
/// each unit's low byte, then its high byte, added to the sum turned
/// right by a bit.
pub fn name_hash(upper: &[u16]) -> u16 {
    upper.iter().fold(0u16, |sum, &unit| add_to_hash(sum, unit))
}

/// Adds the unit `upper` of a name in upper case to its hash `sum`.
pub fn add_to_hash(sum: u16, upper: u16) -> u16 {
    let [low, high] = upper.to_le_bytes();
    let sum = sum.rotate_right(1).wrapping_add(u16::from(low));
    sum.rotate_right(1).wrapping_add(u16::from(high))
}

/// Why a name cannot be stored.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NameError {
    /// More than [`NAME_MAX`] UTF-16 units.
    TooLong,
    /// A character exFAT forbids, or a name ending in `.` or a space,
    /// which Windows cannot open.
    Invalid,
}

/// Checks a name and puts its UTF-16 units in `out`; returns how many
/// there are.
pub fn units(name: &str, out: &mut [u16; NAME_MAX]) -> Result<usize, NameError> {
    if name.is_empty() || name.ends_with('.') || name.ends_with(' ') {
        return Err(NameError::Invalid);
    }
    let mut len = 0;
    for c in name.chars() {
        if (c as u32) < 0x20 || "\"*/:<>?\\|".contains(c) {
            return Err(NameError::Invalid);
        }
        let mut buf = [0u16; 2];
        for &unit in c.encode_utf16(&mut buf).iter() {
            if len == NAME_MAX {
                return Err(NameError::TooLong);
            }
            out[len] = unit;
            len += 1;
        }
    }
    Ok(len)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stamps_round_trip_in_hundredths() {
        // 2026-10-22 00:42:23.37 UTC.
        let ns = 1_792_629_743_370_000_000;
        let stamp = Stamp::of(ns, true);
        assert_eq!(stamp.utc, 0x80);
        assert_eq!(stamp.hundredths, 137);
        assert_eq!(stamp.ns(), ns);
        // An access time keeps two-second steps.
        assert_eq!(Stamp::of(ns, false).ns(), 1_792_629_742_000_000_000);
        // A stamp written two hours east of UTC is two hours later there.
        let east = Stamp { utc: 0x80 | 8, ..Stamp::of(ns + 7_200_000_000_000, true) };
        assert_eq!(east.ns(), ns);
        // And one west of it, earlier.
        let west =
            Stamp { utc: 0x80 | (-4i8 as u8 & 0x7f), ..Stamp::of(ns - 3_600_000_000_000, true) };
        assert_eq!(west.ns(), ns);
        assert_eq!(Stamp::default().ns(), 0);
    }

    #[test]
    fn entries_round_trip() {
        let file = File {
            secondaries: 3,
            checksum: 0x1234,
            attributes: attr::ARCHIVE,
            created: Stamp::of(1_792_629_743_370_000_000, true),
            modified: Stamp::of(1_792_629_745_000_000_000, true),
            accessed: Stamp::of(1_792_629_746_000_000_000, false),
        };
        assert_eq!(File::decode(&file.encode()), file);
        let stream = Stream {
            flags: stream::ALLOCATION_POSSIBLE | stream::NO_FAT_CHAIN,
            name_len: 17,
            hash: 0xbeef,
            valid: 1000,
            first: 77,
            size: 5000,
        };
        assert_eq!(Stream::decode(&stream.encode()), stream);
        let units: [u16; 15] = core::array::from_fn(|i| 0x41 + i as u16);
        assert_eq!(name_units(&name_entry(&units)), units);
    }

    #[test]
    fn names_hash_as_the_specification_says() {
        // The hash, computed as the specification's NameHash does, byte
        // by byte.
        let upper: Vec<u16> = "HELLO.TXT".encode_utf16().collect();
        let mut hash = 0u16;
        for &unit in &upper {
            for byte in unit.to_le_bytes() {
                hash = (if hash & 1 != 0 { 0x8000 } else { 0 }) + (hash >> 1) + u16::from(byte);
            }
        }
        assert_eq!(name_hash(&upper), hash);
    }

    #[test]
    fn names_are_checked() {
        let mut out = [0u16; NAME_MAX];
        assert_eq!(units("hello.txt", &mut out), Ok(9));
        assert_eq!(units("a\u{1f600}", &mut out), Ok(3));
        assert_eq!(units("a:b", &mut out), Err(NameError::Invalid));
        assert_eq!(units("dot.", &mut out), Err(NameError::Invalid));
        assert_eq!(units("tab\t", &mut out), Err(NameError::Invalid));
        assert_eq!(units(&"x".repeat(256), &mut out), Err(NameError::TooLong));
        assert_eq!(units(&"x".repeat(255), &mut out), Ok(255));
        assert_eq!(units("del\u{7f}ok", &mut out), Ok(6));
    }
}
