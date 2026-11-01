// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! UDF's structures on the disk (ECMA-167 parts 3 and 4, UDF 2.60): the
//! tag every descriptor starts with, allocation descriptors, timestamps,
//! and names in OSTA compressed Unicode.

/// What a descriptor's tag says it is (ECMA-167 3/7.2.1 and 4/7.2.1).
pub mod ident {
    /// A primary volume descriptor.
    pub const PRIMARY: u16 = 1;
    /// The anchor volume descriptor pointer, at block 256.
    pub const ANCHOR: u16 = 2;
    /// A pointer to more of the volume descriptor sequence.
    pub const POINTER: u16 = 3;
    /// A partition descriptor.
    pub const PARTITION: u16 = 5;
    /// The logical volume descriptor.
    pub const LOGICAL_VOLUME: u16 = 6;
    /// The end of a sequence of descriptors.
    pub const TERMINATING: u16 = 8;
    /// The file set descriptor, which names the root directory.
    pub const FILE_SET: u16 = 256;
    /// A file identifier descriptor: one entry of a directory.
    pub const FILE_ID: u16 = 257;
    /// More of a file's allocation descriptors.
    pub const ALLOCATION_EXTENT: u16 = 258;
    /// An indirect entry, which points to a later file entry.
    pub const INDIRECT: u16 = 259;
    /// A file entry.
    pub const FILE_ENTRY: u16 = 261;
    /// An extended file entry (UDF 2.00 and later).
    pub const EXTENDED_FILE_ENTRY: u16 = 266;
}

/// The size of a tag.
pub const TAG: usize = 16;

/// Reads a little-endian `u16` at `at`.
pub fn le16(bytes: &[u8], at: usize) -> u16 {
    u16::from_le_bytes([bytes[at], bytes[at + 1]])
}

/// Reads a little-endian `u32` at `at`.
pub fn le32(bytes: &[u8], at: usize) -> u32 {
    u32::from_le_bytes([bytes[at], bytes[at + 1], bytes[at + 2], bytes[at + 3]])
}

/// Reads a little-endian `u64` at `at`.
pub fn le64(bytes: &[u8], at: usize) -> u64 {
    u64::from(le32(bytes, at)) | (u64::from(le32(bytes, at + 4)) << 32)
}

/// CRC-16 of ITU-T V.41 (polynomial 0x1021, from 0), as tags have it.
pub fn crc16(bytes: &[u8]) -> u16 {
    let mut crc = 0u16;
    for &byte in bytes {
        crc ^= u16::from(byte) << 8;
        for _ in 0..8 {
            crc = if crc & 0x8000 != 0 { (crc << 1) ^ 0x1021 } else { crc << 1 };
        }
    }
    crc
}

/// The identifier of the descriptor in `bytes` if its tag is sound: its
/// checksum right, its CRC right over the bytes there are, and the block
/// it says it is in `location`, if given.
pub fn tag(bytes: &[u8], location: Option<u32>) -> Option<u16> {
    let tag = bytes.get(..TAG)?;
    let sum =
        tag.iter().enumerate().filter(|&(i, _)| i != 4).fold(0u8, |s, (_, &b)| s.wrapping_add(b));
    if sum != tag[4] {
        return None;
    }
    if location.is_some_and(|at| at != le32(tag, 12)) {
        return None;
    }
    let len = usize::from(le16(tag, 10));
    if let Some(body) = bytes.get(TAG..TAG + len)
        && crc16(body) != le16(tag, 8)
    {
        return None;
    }
    Some(le16(tag, 0))
}

/// An extent: its length in bytes and where it starts (`extent_ad`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Extent {
    /// Its length in bytes.
    pub len: u32,
    /// Its first block.
    pub block: u32,
}

/// Reads an `extent_ad` at `at`.
pub fn extent(bytes: &[u8], at: usize) -> Extent {
    Extent { len: le32(bytes, at), block: le32(bytes, at + 4) }
}

/// A logical block of a partition (`lb_addr`), and an extent of them.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct LongAd {
    /// Its length in bytes, the top two bits its [`kind`].
    pub len: u32,
    /// Its first block in the partition.
    pub block: u32,
    /// The partition: its number in the logical volume's maps.
    pub partition: u16,
}

/// Reads a `long_ad` at `at`.
pub fn long_ad(bytes: &[u8], at: usize) -> LongAd {
    LongAd { len: le32(bytes, at), block: le32(bytes, at + 4), partition: le16(bytes, at + 8) }
}

/// The kinds of extent an allocation descriptor's length says (ECMA-167
/// 4/14.14.1.1).
pub mod kind {
    /// Recorded: it holds the data.
    pub const RECORDED: u32 = 0;
    /// Allocated but not recorded: it reads as zeros.
    pub const ALLOCATED: u32 = 1;
    /// Neither: a hole, which reads as zeros.
    pub const HOLE: u32 = 2;
    /// The next extent of allocation descriptors.
    pub const NEXT: u32 = 3;
}

/// An allocation descriptor's kind.
pub fn ad_kind(len: u32) -> u32 {
    len >> 30
}

/// An allocation descriptor's length in bytes.
pub fn ad_len(len: u32) -> u32 {
    len & 0x3fff_ffff
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

/// A timestamp (ECMA-167 1/7.3), 12 bytes, in nanoseconds since 1970: 0
/// if it is not a time, or before 1970.
pub fn timestamp(b: &[u8]) -> u64 {
    let Some(b) = b.get(..12) else { return 0 };
    let zone = le16(b, 0);
    // The zone, in minutes east of UTC, twelve bits with a sign; -2047
    // when not said.
    let offset = i64::from(((zone & 0x0fff) as i16) << 4 >> 4);
    let offset = if offset == -2047 || zone >> 12 != 1 { 0 } else { offset };
    let year = i64::from(le16(b, 2) as i16);
    let (month, day) = (u32::from(b[4]), u32::from(b[5]));
    let (hour, minute, second) = (i64::from(b[6]), i64::from(b[7]), i64::from(b[8]));
    if !(1..=12).contains(&month) || !(1..=31).contains(&day) || hour > 23 || minute > 59 {
        return 0;
    }
    let seconds =
        days_from_civil(year, month, day) * 86_400 + hour * 3600 + minute * 60 + second.min(60)
            - offset * 60;
    let fraction = i64::from(b[9].min(99)) * 10_000_000
        + i64::from(b[10].min(99)) * 100_000
        + i64::from(b[11].min(99)) * 1000;
    u64::try_from(seconds.saturating_mul(NS).saturating_add(fraction)).unwrap_or(0)
}

/// A name in OSTA compressed Unicode (UDF 2.1.1): its first byte says
/// whether each character is a byte (8, or 254) or two, big-endian (16,
/// or 255). `/` and NUL, which a name here cannot hold, become `_`.
pub fn cs0(bytes: &[u8], out: &mut NameBuf) {
    out.clear();
    let Some((&id, rest)) = bytes.split_first() else { return };
    let mut push = |c: char| out.push_char(if c == '/' || c == '\0' { '_' } else { c });
    match id {
        8 | 254 => rest.iter().for_each(|&b| push(char::from(b))),
        16 | 255 => {
            let units = rest.as_chunks::<2>().0.iter().map(|&pair| u16::from_be_bytes(pair));
            for c in char::decode_utf16(units) {
                push(c.unwrap_or(char::REPLACEMENT_CHARACTER));
            }
        }
        _ => out.too_long = true,
    }
}

/// A `dstring`: compressed Unicode in a field whose last byte says how
/// many of its bytes are used.
pub fn dstring(field: &[u8], out: &mut NameBuf) {
    out.clear();
    let Some((&len, text)) = field.split_last() else { return };
    let len = usize::from(len).min(text.len());
    if len > 0 {
        cs0(&text[..len], out);
    }
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
        let text = c.encode_utf8(&mut utf8);
        let end = self.len + text.len();
        if end > self.bytes.len() {
            self.too_long = true;
            return;
        }
        self.bytes[self.len..end].copy_from_slice(text.as_bytes());
        self.len = end;
    }

    /// Cuts the spaces off its end.
    pub fn trim_end(&mut self) {
        while let [.., b' '] = self.bytes[..self.len] {
            self.len -= 1;
        }
    }

    /// What it holds, as it is.
    pub fn text(&self) -> &str {
        core::str::from_utf8(&self.bytes[..self.len]).unwrap_or("")
    }

    /// The name, or `None` if it was too long or is not one a directory
    /// can hold (empty, `.`, `..`).
    pub fn name(&self) -> Option<&str> {
        let text = core::str::from_utf8(&self.bytes[..self.len]).ok()?;
        (!self.too_long && !text.is_empty() && text != "." && text != "..").then_some(text)
    }
}
