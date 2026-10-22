// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Directory entries and names: 8.3 short names, VFAT long names, and the
//! 32-byte entries that hold them (Microsoft's FAT specification,
//! "Directory Structure" and "Long File Name Implementation").
//!
//! Every file has a short entry: an 11-byte name (8 for the base, 3 for
//! the extension, padded with spaces, in upper case), its attributes,
//! times, first cluster and size. A name that is not an upper-case 8.3
//! name also has a long name, in UTF-16, spread over long entries of 13
//! units each that come just before the short one, last part first, each
//! with the short name's checksum so that a long name left over from
//! another file is not mistaken for this one's.
//!
//! As Windows NT and Linux do, a name that is a valid 8.3 name in lower
//! case (`readme.txt`) needs no long entries: bits of the short entry say
//! its base or extension is lower case. Any other name gets a short form
//! made from it (upper case, invalid characters replaced, cut to 8.3),
//! with a numeric tail (`LONGFI~1.TXT`) when that loses anything. Names
//! compare without regard to case, as FAT requires.

/// The size of a directory entry.
pub const ENTRY: usize = 32;

/// Attribute bits.
pub mod attr {
    /// The file should not be written.
    pub const READ_ONLY: u8 = 0x01;
    /// The file is hidden from listings (Radix lists it anyway).
    pub const HIDDEN: u8 = 0x02;
    /// The file belongs to the system.
    pub const SYSTEM: u8 = 0x04;
    /// The entry is the volume's label.
    pub const VOLUME_ID: u8 = 0x08;
    /// The entry is a directory.
    pub const DIRECTORY: u8 = 0x10;
    /// The file changed since it was backed up.
    pub const ARCHIVE: u8 = 0x20;
    /// The entry is part of a long name.
    pub const LONG_NAME: u8 = 0x0f;
}

/// The first name byte of a deleted entry.
pub const DELETED: u8 = 0xe5;
/// Bits of the short entry's case byte: the base is lower case.
pub const LOWER_BASE: u8 = 0x08;
/// The extension is lower case.
pub const LOWER_EXT: u8 = 0x10;

/// The most UTF-16 units of a long name.
pub const LONG_MAX: usize = 255;
/// UTF-16 units in one long entry.
pub const PER_LONG: usize = 13;
/// The most long entries of one name.
pub const MAX_LONG: usize = LONG_MAX.div_ceil(PER_LONG);

/// A short directory entry.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Short {
    /// The 8.3 name, padded with spaces.
    pub name: [u8; 11],
    /// Attribute bits.
    pub attr: u8,
    /// Which parts of the name are lower case.
    pub case: u8,
    /// Hundredths of a second added to `created_time`'s two-second steps.
    pub created_tenths: u8,
    /// When the file was made.
    pub created_time: u16,
    /// Likewise.
    pub created_date: u16,
    /// When it was last read.
    pub accessed_date: u16,
    /// When it was last written.
    pub modified_time: u16,
    /// Likewise.
    pub modified_date: u16,
    /// Its first cluster, or 0 when it has none.
    pub cluster: u32,
    /// Its size in bytes (0 for directories).
    pub size: u32,
}

impl Short {
    /// The entry in `bytes`.
    pub fn decode(bytes: &[u8; ENTRY]) -> Short {
        let u16_at = |at: usize| u16::from_le_bytes([bytes[at], bytes[at + 1]]);
        let mut name = [0u8; 11];
        name.copy_from_slice(&bytes[0..11]);
        Short {
            name,
            attr: bytes[11],
            case: bytes[12],
            created_tenths: bytes[13],
            created_time: u16_at(14),
            created_date: u16_at(16),
            accessed_date: u16_at(18),
            cluster: (u32::from(u16_at(20)) << 16) | u32::from(u16_at(26)),
            modified_time: u16_at(22),
            modified_date: u16_at(24),
            size: u32::from_le_bytes([bytes[28], bytes[29], bytes[30], bytes[31]]),
        }
    }

    /// The entry's bytes.
    pub fn encode(&self) -> [u8; ENTRY] {
        let mut b = [0u8; ENTRY];
        b[0..11].copy_from_slice(&self.name);
        b[11] = self.attr;
        b[12] = self.case;
        b[13] = self.created_tenths;
        b[14..16].copy_from_slice(&self.created_time.to_le_bytes());
        b[16..18].copy_from_slice(&self.created_date.to_le_bytes());
        b[18..20].copy_from_slice(&self.accessed_date.to_le_bytes());
        b[20..22].copy_from_slice(&((self.cluster >> 16) as u16).to_le_bytes());
        b[22..24].copy_from_slice(&self.modified_time.to_le_bytes());
        b[24..26].copy_from_slice(&self.modified_date.to_le_bytes());
        b[26..28].copy_from_slice(&(self.cluster as u16).to_le_bytes());
        b[28..32].copy_from_slice(&self.size.to_le_bytes());
        b
    }

    /// Whether it is a directory.
    pub fn is_dir(&self) -> bool {
        self.attr & attr::DIRECTORY != 0
    }

    /// Whether it is `.` or `..`.
    pub fn is_dot(&self) -> bool {
        self.name[0] == b'.'
    }

    /// Writes its name as people see it, with the case bits applied.
    pub fn display<const N: usize>(&self, out: &mut NameBuf<N>) {
        out.clear();
        let mut first = self.name[0];
        if first == 0x05 {
            first = DELETED;
        }
        let base_len = self.name[..8].iter().rposition(|&b| b != b' ').map_or(0, |p| p + 1);
        let ext_len = self.name[8..].iter().rposition(|&b| b != b' ').map_or(0, |p| p + 1);
        for (i, &byte) in self.name[..base_len].iter().enumerate() {
            let byte = if i == 0 { first } else { byte };
            out.push_char(oem_char(byte, self.case & LOWER_BASE != 0));
        }
        if ext_len > 0 {
            out.push_char('.');
            for &byte in &self.name[8..8 + ext_len] {
                out.push_char(oem_char(byte, self.case & LOWER_EXT != 0));
            }
        }
    }
}

/// A short name's byte as a character: ASCII as it is (in lower case if
/// `lower`), other code page bytes as `_`, since Radix does not carry
/// code page 437.
fn oem_char(byte: u8, lower: bool) -> char {
    if byte.is_ascii() {
        let c = byte as char;
        if lower { c.to_ascii_lowercase() } else { c }
    } else {
        '_'
    }
}

/// The checksum of a short name that its long entries carry.
pub fn checksum(name: &[u8; 11]) -> u8 {
    name.iter().fold(0u8, |sum, &b| sum.rotate_right(1).wrapping_add(b))
}

/// A long entry.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Long {
    /// Its place in the name, from 1.
    pub order: u8,
    /// Whether it holds the end of the name (it comes first on disk).
    pub last: bool,
    /// Its 13 units.
    pub units: [u16; PER_LONG],
    /// The checksum of the short name it belongs to.
    pub checksum: u8,
}

/// Where a long entry's units are.
const UNIT_OFFSETS: [usize; PER_LONG] = [1, 3, 5, 7, 9, 14, 16, 18, 20, 22, 24, 28, 30];

impl Long {
    /// The long entry in `bytes`, if it is one.
    pub fn decode(bytes: &[u8; ENTRY]) -> Option<Long> {
        if bytes[11] & 0x3f != attr::LONG_NAME || bytes[0] == DELETED {
            return None;
        }
        let order = bytes[0] & 0x1f;
        if order == 0 || usize::from(order) > MAX_LONG {
            return None;
        }
        let mut units = [0u16; PER_LONG];
        for (unit, &at) in units.iter_mut().zip(&UNIT_OFFSETS) {
            *unit = u16::from_le_bytes([bytes[at], bytes[at + 1]]);
        }
        Some(Long { order, last: bytes[0] & 0x40 != 0, units, checksum: bytes[13] })
    }

    /// The entry's bytes.
    pub fn encode(&self) -> [u8; ENTRY] {
        let mut b = [0u8; ENTRY];
        b[0] = self.order | if self.last { 0x40 } else { 0 };
        b[11] = attr::LONG_NAME;
        b[13] = self.checksum;
        for (&unit, &at) in self.units.iter().zip(&UNIT_OFFSETS) {
            b[at..at + 2].copy_from_slice(&unit.to_le_bytes());
        }
        b
    }
}

/// The long entries for `units`, in the order they go on disk (the end of
/// the name first).
pub fn long_entries(units: &[u16], sum: u8, mut f: impl FnMut([u8; ENTRY])) {
    let count = units.len().div_ceil(PER_LONG);
    for order in (1..=count).rev() {
        let start = (order - 1) * PER_LONG;
        let mut chunk = [0xffffu16; PER_LONG];
        for (i, unit) in chunk.iter_mut().enumerate() {
            match units.get(start + i) {
                Some(&u) => *unit = u,
                // The name ends with one NUL, then padding.
                None if start + i == units.len() => *unit = 0,
                None => {}
            }
        }
        let entry = Long { order: order as u8, last: order == count, units: chunk, checksum: sum };
        f(entry.encode());
    }
}

/// The bytes of the longest name as UTF-8: [`LONG_MAX`] units of up to
/// three bytes each.
pub const NAME_BYTES: usize = LONG_MAX * 3;

/// A name being assembled or shown, as UTF-8: by default up to
/// [`LONG_MAX`] units, any long name.
pub struct NameBuf<const N: usize = NAME_BYTES> {
    bytes: [u8; N],
    len: usize,
}

/// Room for a short name as shown: eight characters, a dot and three
/// more, all ASCII ([`Short::display`]).
pub type ShortBuf = NameBuf<12>;

impl<const N: usize> Default for NameBuf<N> {
    fn default() -> Self {
        NameBuf::new()
    }
}

impl<const N: usize> NameBuf<N> {
    /// An empty name.
    pub const fn new() -> NameBuf<N> {
        NameBuf { bytes: [0; N], len: 0 }
    }

    /// Empties it.
    pub fn clear(&mut self) {
        self.len = 0;
    }

    /// Adds `c`, if there is room.
    pub fn push_char(&mut self, c: char) {
        let mut tmp = [0u8; 4];
        let s = c.encode_utf8(&mut tmp);
        if self.len + s.len() <= self.bytes.len() {
            self.bytes[self.len..self.len + s.len()].copy_from_slice(s.as_bytes());
            self.len += s.len();
        }
    }

    /// Sets it from UTF-16 units, up to the first NUL. Units that are not
    /// valid UTF-16 become U+FFFD.
    pub fn set_units(&mut self, units: &[u16]) {
        self.clear();
        let end = units.iter().position(|&u| u == 0).unwrap_or(units.len());
        for c in char::decode_utf16(units[..end].iter().copied()) {
            self.push_char(c.unwrap_or(char::REPLACEMENT_CHARACTER));
        }
    }

    /// The name.
    pub fn as_str(&self) -> &str {
        core::str::from_utf8(&self.bytes[..self.len]).unwrap_or("")
    }
}

/// Whether two names are the same to FAT: equal but for case.
pub fn same(a: &str, b: &str) -> bool {
    if a.len() == b.len() && a.eq_ignore_ascii_case(b) {
        return true;
    }
    a.chars().flat_map(char::to_uppercase).eq(b.chars().flat_map(char::to_uppercase))
}

/// Why a name cannot be stored.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NameError {
    /// Too many UTF-16 units.
    TooLong,
    /// A character FAT forbids, or a name ending in `.` or a space.
    Invalid,
}

/// Checks a long name and puts its UTF-16 units in `out`; returns how
/// many there are.
pub fn units(name: &str, out: &mut [u16; LONG_MAX]) -> Result<usize, NameError> {
    if name.ends_with('.') || name.ends_with(' ') {
        return Err(NameError::Invalid);
    }
    let mut len = 0;
    for c in name.chars() {
        if (c as u32) < 0x20 || "\"*/:<>?\\|\u{7f}".contains(c) {
            return Err(NameError::Invalid);
        }
        let mut buf = [0u16; 2];
        for &unit in c.encode_utf16(&mut buf).iter() {
            if len == LONG_MAX {
                return Err(NameError::TooLong);
            }
            out[len] = unit;
            len += 1;
        }
    }
    Ok(len)
}

/// Characters a short name may hold besides letters and digits.
const SHORT_EXTRA: &[u8] = b"$%'-_@~`!(){}^#&";

fn short_ok(byte: u8) -> bool {
    byte.is_ascii_uppercase() || byte.is_ascii_digit() || SHORT_EXTRA.contains(&byte)
}

/// The short form of a name.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ShortForm {
    /// The name is an 8.3 name in one case per part: this short entry,
    /// with these case bits, is all it needs.
    Exact([u8; 11], u8),
    /// The name needs long entries, but its short form lost only case:
    /// it is used as it is unless another file has it.
    Cased([u8; 11]),
    /// The short form lost characters: it gets a numeric tail.
    Lossy([u8; 11]),
}

/// The short form of `name` (checked by [`units`]).
pub fn short_form(name: &str) -> ShortForm {
    let bytes = name.as_bytes();
    let dot = bytes.iter().rposition(|&b| b == b'.').filter(|&p| p > 0);
    let (base, ext) = match dot {
        Some(p) => (&bytes[..p], &bytes[p + 1..]),
        None => (bytes, &bytes[..0]),
    };
    let mut short = [b' '; 11];
    let mut lossy = false;
    let mut fill = |part: &[u8], room: usize, at: usize, lossy: &mut bool| {
        let mut n = 0;
        for &byte in part {
            let up = byte.to_ascii_uppercase();
            if byte == b' ' || byte == b'.' {
                // Spaces and inner dots are dropped.
                *lossy = true;
                continue;
            }
            if n == room {
                *lossy = true;
                break;
            }
            short[at + n] = if short_ok(up) {
                up
            } else {
                *lossy = true;
                b'_'
            };
            n += 1;
        }
    };
    fill(base, 8, 0, &mut lossy);
    fill(ext, 3, 8, &mut lossy);
    if !name.is_ascii() || name.starts_with('.') {
        lossy = true;
    }
    if short[0] == b' ' {
        // Nothing usable in the base.
        short[0] = b'_';
        lossy = true;
    }
    if short[0] == DELETED {
        short[0] = 0x05;
    }
    if lossy {
        return ShortForm::Lossy(short);
    }
    let case = |part: &[u8]| -> Option<bool> {
        let lower = part.iter().any(u8::is_ascii_lowercase);
        let upper = part.iter().any(u8::is_ascii_uppercase);
        if lower && upper { None } else { Some(lower) }
    };
    match (case(base), case(ext)) {
        (Some(base_lower), Some(ext_lower)) => {
            let bits =
                if base_lower { LOWER_BASE } else { 0 } | if ext_lower { LOWER_EXT } else { 0 };
            ShortForm::Exact(short, bits)
        }
        _ => ShortForm::Cased(short),
    }
}

/// `basis` with the numeric tail `~n`, its base cut to make room.
pub fn with_tail(basis: &[u8; 11], n: u32) -> [u8; 11] {
    let mut digits = [0u8; 10];
    let mut len = 0;
    let mut value = n;
    loop {
        digits[len] = b'0' + (value % 10) as u8;
        len += 1;
        value /= 10;
        if value == 0 {
            break;
        }
    }
    let tail = len + 1;
    let base_len = basis[..8].iter().rposition(|&b| b != b' ').map_or(0, |p| p + 1);
    let keep = base_len.min(8 - tail);
    let mut out = *basis;
    for byte in out[keep..8].iter_mut() {
        *byte = b' ';
    }
    out[keep] = b'~';
    for i in 0..len {
        out[keep + 1 + i] = digits[len - 1 - i];
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn short(s: &str) -> [u8; 11] {
        let mut out = [b' '; 11];
        let (base, ext) = s.split_once('.').unwrap_or((s, ""));
        out[..base.len()].copy_from_slice(base.as_bytes());
        out[8..8 + ext.len()].copy_from_slice(ext.as_bytes());
        out
    }

    #[test]
    fn short_forms() {
        assert_eq!(short_form("README.TXT"), ShortForm::Exact(short("README.TXT"), 0));
        assert_eq!(short_form("readme.txt"), ShortForm::Exact(short("README.TXT"), 0x18));
        assert_eq!(short_form("readme.TXT"), ShortForm::Exact(short("README.TXT"), 0x08));
        assert_eq!(short_form("ReadMe.txt"), ShortForm::Cased(short("README.TXT")));
        assert_eq!(short_form("a long name.text"), ShortForm::Lossy(short("ALONGNAM.TEX")));
        assert_eq!(short_form("x.tar.gz"), ShortForm::Lossy(short("XTAR.GZ")));
        assert_eq!(short_form(".profile"), ShortForm::Lossy(short("PROFILE")));
        assert_eq!(short_form("héllo"), ShortForm::Lossy(short("H__LLO")));
        assert_eq!(short_form("a+b"), ShortForm::Lossy(short("A_B")));
        assert_eq!(with_tail(&short("ALONGNAM.TEX"), 1), short("ALONGN~1.TEX"));
        assert_eq!(with_tail(&short("AB"), 12), short("AB~12"));
        assert_eq!(with_tail(&short("ABCDEFGH"), 123456), short("A~123456"));
    }

    #[test]
    fn names_and_entries() {
        let mut out = [0u16; LONG_MAX];
        assert_eq!(units("ok name", &mut out), Ok(7));
        assert_eq!(units("bad?", &mut out), Err(NameError::Invalid));
        assert_eq!(units("trailing.", &mut out), Err(NameError::Invalid));
        assert_eq!(units(&"x".repeat(256), &mut out), Err(NameError::TooLong));
        assert!(same("Hello.TXT", "hello.txt"));
        assert!(same("Ärger", "ärger"));
        assert!(!same("a", "b"));
        let name = short("ALONGN~1.TEX");
        let sum = checksum(&name);
        let n = units("a long name.text", &mut out).unwrap();
        let mut entries = std::vec::Vec::new();
        long_entries(&out[..n], sum, |e| entries.push(e));
        assert_eq!(entries.len(), 2);
        let first = Long::decode(&entries[0]).unwrap();
        assert!(first.last && first.order == 2 && first.checksum == sum);
        // "ext" then NUL then padding.
        assert_eq!(&first.units[..5], &[b'e' as u16, b'x' as u16, b't' as u16, 0, 0xffff]);
        let mut buf: NameBuf = NameBuf::new();
        let second = Long::decode(&entries[1]).unwrap();
        let mut all = [0u16; 26];
        all[..13].copy_from_slice(&second.units);
        all[13..].copy_from_slice(&first.units);
        buf.set_units(&all);
        assert_eq!(buf.as_str(), "a long name.text");
        let entry = Short {
            name: short("README.TXT"),
            attr: attr::ARCHIVE,
            case: 0x18,
            created_tenths: 0,
            created_time: 1,
            created_date: 2,
            accessed_date: 3,
            modified_time: 4,
            modified_date: 5,
            cluster: 0x0012_3456,
            size: 99,
        };
        assert_eq!(Short::decode(&entry.encode()), entry);
        entry.display(&mut buf);
        assert_eq!(buf.as_str(), "readme.txt");
    }
}
