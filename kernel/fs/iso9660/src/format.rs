// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! ISO 9660's structures on the disk (ECMA-119, 4th edition): the volume
//! descriptors from sector 16, the directory records, and the dates.
//!
//! Every number is kept twice, little-endian then big-endian ("both-byte
//! order"); the little-endian half is read. A directory is a run of
//! records, none of which crosses a logical block: a length of zero means
//! the rest of the block is unused.

/// The size of the sectors the volume descriptors are counted in, and of
/// every logical block of the volumes this crate reads.
pub const SECTOR: u64 = 2048;
/// The sector of the first volume descriptor: the 32 KiB before it are
/// the system area, left to whatever boots from the disc.
pub const FIRST_DESCRIPTOR: u64 = 16;
/// How many descriptors are looked at before giving up on a terminator.
pub const MAX_DESCRIPTORS: u64 = 64;
/// The bytes of a directory record before its name.
pub const RECORD_HEADER: usize = 33;
/// The most bytes of a directory record.
pub const RECORD_MAX: usize = 255;

/// The bits of a directory record's flags.
pub mod flag {
    /// It is hidden from listings, as a DOS file can be (shown anyway).
    pub const HIDDEN: u8 = 1 << 0;
    /// It is a directory.
    pub const DIRECTORY: u8 = 1 << 1;
    /// It is an associated file (a Macintosh resource fork): skipped.
    pub const ASSOCIATED: u8 = 1 << 2;
    /// More records of the same file follow: its next extents.
    pub const MULTI_EXTENT: u8 = 1 << 7;
}

/// Reads a little-endian 16-bit number.
pub fn le16(bytes: &[u8], at: usize) -> u16 {
    u16::from_le_bytes([bytes[at], bytes[at + 1]])
}

/// Reads a little-endian 32-bit number, the first half of a both-byte
/// order one.
pub fn le32(bytes: &[u8], at: usize) -> u32 {
    u32::from_le_bytes([bytes[at], bytes[at + 1], bytes[at + 2], bytes[at + 3]])
}

/// What a volume descriptor says.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Descriptor {
    /// The primary volume descriptor.
    Primary(Volume),
    /// A supplementary one with Joliet's escape sequence: its names are
    /// UCS-2, at this level (1 to 3).
    Joliet(Volume, u8),
    /// The end of the descriptors.
    Terminator,
    /// One this crate does not read: a boot record, a partition, or a
    /// supplementary descriptor of another character set.
    Other,
}

/// What a primary or Joliet descriptor says of its volume and tree.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Volume {
    /// Its logical block size, in bytes.
    pub block_size: u32,
    /// How many logical blocks it has.
    pub blocks: u32,
    /// Where its root directory is: its first block.
    pub root_extent: u32,
    /// Its root directory's size, in bytes.
    pub root_size: u32,
    /// When its root directory was recorded.
    pub root_recorded: u64,
    /// Its volume identifier: 32 bytes of d-characters padded with
    /// spaces, or for Joliet 16 UCS-2 characters, big-endian.
    pub label: [u8; 32],
    /// When it was made, in nanoseconds since 1970, or 0.
    pub created: u64,
}

/// Reads the volume descriptor in `sector`, or `None` if it is not one
/// (the standard identifier `CD001` is missing).
pub fn descriptor(sector: &[u8]) -> Option<Descriptor> {
    if sector.len() < SECTOR as usize || &sector[1..6] != b"CD001" {
        return None;
    }
    let volume = || {
        let root = record(&sector[156..190])?;
        let mut label = [0; 32];
        label.copy_from_slice(&sector[40..72]);
        let created = long_date(sector[813..830].try_into().ok()?);
        Some(Volume {
            block_size: u32::from(le16(sector, 128)),
            blocks: le32(sector, 80),
            root_extent: root.extent,
            root_size: root.size,
            root_recorded: root.recorded,
            label,
            created,
        })
    };
    Some(match sector[0] {
        1 => volume().map_or(Descriptor::Other, Descriptor::Primary),
        2 => {
            let escape = &sector[88..91];
            let level = match escape {
                b"%/@" => 1,
                b"%/C" => 2,
                b"%/E" => 3,
                _ => 0,
            };
            match volume() {
                Some(volume) if level > 0 => Descriptor::Joliet(volume, level),
                _ => Descriptor::Other,
            }
        }
        255 => Descriptor::Terminator,
        _ => Descriptor::Other,
    })
}

/// A directory record.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Record<'a> {
    /// Its length in bytes.
    pub len: usize,
    /// The logical blocks of its extended attribute record, which come
    /// before its data.
    pub ext_attr: u8,
    /// Its extent's first logical block.
    pub extent: u32,
    /// Its data's length in bytes.
    pub size: u32,
    /// When it was recorded, in nanoseconds since 1970.
    pub recorded: u64,
    /// Its [`flag`]s.
    pub flags: u8,
    /// Its file unit size: 0 unless the file is interleaved.
    pub unit: u8,
    /// Its interleave gap, likewise.
    pub gap: u8,
    /// Its identifier: a name and `;1`, or 0 for `.` and 1 for `..`.
    pub name: &'a [u8],
    /// Its System Use area, which holds Rock Ridge's entries.
    pub system_use: &'a [u8],
}

impl Record<'_> {
    /// Whether it is a directory's own record (`.`) or its parent's
    /// (`..`).
    pub fn is_dot(&self) -> bool {
        self.name == [0] || self.name == [1]
    }

    /// Whether it says it is a directory.
    pub fn is_dir(&self) -> bool {
        self.flags & flag::DIRECTORY != 0
    }

    /// Where its data starts, in logical blocks.
    pub fn data_block(&self) -> u64 {
        u64::from(self.extent) + u64::from(self.ext_attr)
    }
}

/// Reads the directory record at the start of `bytes`, which hold it
/// all; `None` if it does not fit or is too short for its name.
pub fn record(bytes: &[u8]) -> Option<Record<'_>> {
    let len = usize::from(*bytes.first()?);
    if len < RECORD_HEADER + 1 || len > bytes.len() {
        return None;
    }
    let name_len = usize::from(bytes[32]);
    let name_end = RECORD_HEADER + name_len;
    if name_len == 0 || name_end > len {
        return None;
    }
    // A pad byte follows a name of even length, so that the System Use
    // area starts on an even byte.
    let system = (name_end + (1 - name_len % 2)).min(len);
    Some(Record {
        len,
        ext_attr: bytes[1],
        extent: le32(bytes, 2),
        size: le32(bytes, 10),
        recorded: short_date(bytes[18..25].try_into().ok()?),
        flags: bytes[25],
        unit: bytes[26],
        gap: bytes[27],
        name: &bytes[RECORD_HEADER..name_end],
        system_use: &bytes[system..len],
    })
}

const NS: i64 = 1_000_000_000;

/// Days from 1970-01-01 to the date: Howard Hinnant's `days_from_civil`
/// (public domain, <https://howardhinnant.github.io/date_algorithms.html>).
fn days_from_civil(year: i64, month: u32, day: u32) -> i64 {
    let y = if month <= 2 { year - 1 } else { year };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let m = i64::from(month);
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + i64::from(day) - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// The time of a date and time in a zone `offset` quarter hours east of
/// UTC, in nanoseconds since 1970: 0 for a date that is not one, or
/// before 1970.
fn time_of(date: (i64, u32, u32), (hour, minute, second): (i64, i64, i64), offset: i8) -> u64 {
    let (year, month, day) = date;
    if !(1..=12).contains(&month) || !(1..=31).contains(&day) || hour > 23 || minute > 59 {
        return 0;
    }
    let days = days_from_civil(year, month, day);
    let seconds =
        days * 86_400 + hour * 3600 + minute * 60 + second.min(60) - i64::from(offset) * 15 * 60;
    u64::try_from(seconds.saturating_mul(NS)).unwrap_or(0)
}

/// A directory record's date: years since 1900, month, day, hour, minute
/// and second, and the zone in quarter hours from UTC.
pub fn short_date(bytes: &[u8; 7]) -> u64 {
    let [year, month, day, hour, minute, second, offset] = *bytes;
    if month == 0 {
        return 0;
    }
    let date = (1900 + i64::from(year), u32::from(month), u32::from(day));
    let clock = (i64::from(hour), i64::from(minute), i64::from(second));
    time_of(date, clock, offset as i8)
}

/// A descriptor's or Rock Ridge's long date: 16 ASCII digits (year,
/// month, day, hour, minute, second, hundredths) and the zone; 0 if it
/// says none (all zeros) or is not digits.
pub fn long_date(bytes: &[u8; 17]) -> u64 {
    let mut digits = [0i64; 16];
    for (digit, &byte) in digits.iter_mut().zip(&bytes[..16]) {
        if !byte.is_ascii_digit() {
            return 0;
        }
        *digit = i64::from(byte - b'0');
    }
    let number = |from: usize, to: usize| digits[from..to].iter().fold(0, |n, d| n * 10 + d);
    let date = (number(0, 4), number(4, 6) as u32, number(6, 8) as u32);
    if date.0 == 0 {
        return 0;
    }
    let clock = (number(8, 10), number(10, 12), number(12, 14));
    let base = time_of(date, clock, bytes[16] as i8);
    if base == 0 {
        return 0;
    }
    base + number(14, 16) as u64 * 10_000_000
}

/// A name as Linux shows a plain ISO 9660 one (`map=normal`): its
/// version (`;1`) and a trailing dot cut, and in lower case.
pub fn plain_name(raw: &[u8], out: &mut NameBuf) {
    out.clear();
    // The directory's own record and its parent's have no name.
    if matches!(raw, [0 | 1]) {
        return;
    }
    let end = raw.iter().position(|&b| b == b';').unwrap_or(raw.len());
    let mut name = &raw[..end];
    if let [rest @ .., b'.'] = name {
        name = rest;
    }
    for &byte in name {
        let byte = byte.to_ascii_lowercase();
        // ISO 9660 allows only d-characters; anything else is kept as a
        // byte only if it is ASCII, so that the name is UTF-8.
        out.push_char(if byte.is_ascii() && byte != b'/' && byte != 0 {
            char::from(byte)
        } else {
            '_'
        });
    }
}

/// A Joliet name, UCS-2 (or UTF-16) big-endian, as UTF-8, its version
/// (`;1`) cut.
pub fn joliet_name(raw: &[u8], out: &mut NameBuf) {
    out.clear();
    let units = raw.as_chunks::<2>().0.iter().map(|&pair| u16::from_be_bytes(pair));
    for c in char::decode_utf16(units) {
        let c = c.unwrap_or(char::REPLACEMENT_CHARACTER);
        out.push_char(if c == '/' || c == '\0' { '_' } else { c });
    }
    out.cut_version();
}

/// A file name being built, at most [`radix_vfs::NAME_MAX`] bytes of
/// UTF-8: past that it is marked too long.
pub struct NameBuf {
    bytes: [u8; radix_vfs::NAME_MAX],
    len: usize,
    too_long: bool,
}

impl Default for NameBuf {
    fn default() -> Self {
        NameBuf { bytes: [0; radix_vfs::NAME_MAX], len: 0, too_long: false }
    }
}

impl NameBuf {
    /// Empties it.
    pub fn clear(&mut self) {
        self.len = 0;
        self.too_long = false;
    }

    /// Adds `c`.
    pub fn push_char(&mut self, c: char) {
        let mut utf8 = [0; 4];
        self.push_bytes(c.encode_utf8(&mut utf8).as_bytes());
    }

    /// Adds `bytes`, which are UTF-8 or are made so.
    pub fn push_bytes(&mut self, bytes: &[u8]) {
        match core::str::from_utf8(bytes) {
            Ok(text) => self.push_str(text),
            Err(_) => {
                for chunk in bytes.utf8_chunks() {
                    self.push_str(chunk.valid());
                    if !chunk.invalid().is_empty() {
                        self.push_str("\u{fffd}");
                    }
                }
            }
        }
    }

    fn push_str(&mut self, text: &str) {
        let end = self.len + text.len();
        if end > self.bytes.len() {
            self.too_long = true;
            return;
        }
        self.bytes[self.len..end].copy_from_slice(text.as_bytes());
        self.len = end;
    }

    /// Cuts the spaces off its end, which pad a volume's label.
    pub fn trim_end(&mut self) {
        while let [.., b' '] = self.bytes[..self.len] {
            self.len -= 1;
        }
    }

    /// Cuts a version (`;` and digits) off its end.
    fn cut_version(&mut self) {
        let text = &self.bytes[..self.len];
        if let Some(at) = text.iter().rposition(|&b| b == b';')
            && text[at + 1..].iter().all(u8::is_ascii_digit)
        {
            self.len = at;
        }
    }

    /// The name, or `None` if it was too long or is not one a directory
    /// can hold (empty, `.`, `..`).
    pub fn name(&self) -> Option<&str> {
        let text = core::str::from_utf8(&self.bytes[..self.len]).ok()?;
        (!self.too_long && !text.is_empty() && text != "." && text != "..").then_some(text)
    }
}
