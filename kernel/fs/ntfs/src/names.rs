// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! The names Radix gives files, and the values that hold them:
//! `$FILE_NAME`, which a directory's index keeps a copy of, and
//! `$STANDARD_INFORMATION`.
//!
//! A name is kept as Windows keeps a long name (the Win32 namespace),
//! and as a short one too (Win32 and DOS) when it is a valid 8.3 name
//! already, as Windows does; Radix makes no other short names, as
//! Windows does not on a volume that is not its system's. Names Windows
//! could not open are refused, as ntfs-3g's `windows_names` refuses them:
//! with `\ / : * ? " < > |` or a control character in them, ending in a
//! space or a dot, or a device's (`CON`, `NUL`, `COM1`...).

use radix_vfs::mem::{self, KVec};
use radix_vfs::{Error, Result};

use crate::edit::{put_u32, put_u64};
use crate::index::{NAME_MAX, namespace};

/// The time NTFS counts from, 1601, as NTFS's 100 ns before 1970.
const NT_EPOCH: u64 = 116_444_736_000_000_000;

/// The NTFS time of `ns` nanoseconds since 1970.
pub fn to_nt(ns: u64) -> u64 {
    (ns / 100).saturating_add(NT_EPOCH)
}

/// `ns` as finely as NTFS keeps it, 100 ns.
pub fn coarse(ns: u64) -> u64 {
    ns - ns % 100
}

/// The UTF-16 units of `name` in `out`; how many. Fails with
/// [`Error::NameTooLong`] for more than NTFS holds.
pub fn units(name: &str, out: &mut [u16; NAME_MAX]) -> Result<usize> {
    let mut len = 0;
    for unit in name.encode_utf16() {
        *out.get_mut(len).ok_or(Error::NameTooLong)? = unit;
        len += 1;
    }
    Ok(len)
}

/// The device names Windows keeps for itself, with or without an
/// extension.
const DEVICES: [&str; 22] = [
    "CON", "PRN", "AUX", "NUL", "COM1", "COM2", "COM3", "COM4", "COM5", "COM6", "COM7", "COM8",
    "COM9", "LPT1", "LPT2", "LPT3", "LPT4", "LPT5", "LPT6", "LPT7", "LPT8", "LPT9",
];

/// Fails with [`Error::BadName`] for a name Windows could not open.
pub fn check_windows(name: &str) -> Result<()> {
    let bad = |c: char| (c as u32) < 0x20 || "\\/:*?\"<>|".contains(c);
    if name.chars().any(bad) || name.ends_with(' ') || name.ends_with('.') {
        return Err(Error::BadName);
    }
    let stem = name.split('.').next().unwrap_or(name).trim_end_matches(' ');
    if DEVICES.iter().any(|d| d.eq_ignore_ascii_case(stem)) {
        return Err(Error::BadName);
    }
    Ok(())
}

/// The namespace a new name goes in: Win32 and DOS for a valid 8.3 name
/// (upper case, at most eight characters and three after one dot, of
/// those DOS allows), Win32 for any other.
pub fn namespace_of(name: &str) -> u8 {
    let dos_char =
        |c: char| c.is_ascii_uppercase() || c.is_ascii_digit() || "!#$%&'()-@^_`{}~".contains(c);
    let (stem, ext) = match name.split_once('.') {
        Some((stem, ext)) => (stem, Some(ext)),
        None => (name, None),
    };
    let fits = !stem.is_empty()
        && stem.len() <= 8
        && stem.chars().all(dos_char)
        && ext.is_none_or(|e| !e.is_empty() && e.len() <= 3 && e.chars().all(dos_char));
    if fits { namespace::WIN32_AND_DOS } else { namespace::WIN32 }
}

/// What a `$FILE_NAME` says of its file besides the name, as a
/// directory's index shows it ("duplicated information").
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Dup {
    /// When it was made, as NTFS keeps times.
    pub created: u64,
    /// When its data last changed.
    pub modified: u64,
    /// When its record last changed.
    pub changed: u64,
    /// When it was last read.
    pub accessed: u64,
    /// The bytes its data's clusters hold.
    pub allocated: u64,
    /// Its data's size.
    pub size: u64,
    /// Its attributes ([`crate::index::attributes`]).
    pub flags: u32,
}

impl Dup {
    /// Puts it in the `$FILE_NAME` value `value`.
    pub fn put(&self, value: &mut [u8]) {
        put_u64(value, 0x08, self.created);
        put_u64(value, 0x10, self.modified);
        put_u64(value, 0x18, self.changed);
        put_u64(value, 0x20, self.accessed);
        put_u64(value, 0x28, self.allocated);
        put_u64(value, 0x30, self.size);
        put_u32(value, 0x38, self.flags);
    }
}

/// A `$FILE_NAME` value: the name `units` in namespace `ns`, in the
/// directory `parent` (a file reference), saying `dup`.
pub fn file_name(parent: u64, dup: &Dup, units: &[u16], ns: u8) -> Result<KVec<u8>> {
    let mut v = crate::fs::zeroed(0x42 + 2 * units.len())?;
    put_u64(&mut v, 0, parent);
    dup.put(&mut v);
    v[0x40] = units.len() as u8;
    v[0x41] = ns;
    for (i, u) in units.iter().enumerate() {
        v[0x42 + 2 * i..0x44 + 2 * i].copy_from_slice(&u.to_le_bytes());
    }
    Ok(v)
}

/// The size of a `$STANDARD_INFORMATION` of NTFS 3.x.
pub const SI_SIZE: usize = 0x48;

/// A `$STANDARD_INFORMATION` value of NTFS 3.x: the times of `dup`, the
/// attributes `attributes`, and the security descriptor `security` (its
/// number in `$Secure`).
pub fn standard_information(dup: &Dup, attributes: u32, security: u32) -> Result<KVec<u8>> {
    let mut v = crate::fs::zeroed(SI_SIZE)?;
    put_u64(&mut v, 0x00, dup.created);
    put_u64(&mut v, 0x08, dup.modified);
    put_u64(&mut v, 0x10, dup.changed);
    put_u64(&mut v, 0x18, dup.accessed);
    put_u32(&mut v, 0x20, attributes);
    put_u32(&mut v, 0x34, security);
    Ok(v)
}

/// Puts the times `dup` has and `attributes` in the
/// `$STANDARD_INFORMATION` value `value`.
pub fn put_standard(value: &mut [u8], dup: &Dup, attributes: u32) {
    if value.len() < 0x24 {
        return;
    }
    put_u64(value, 0x00, dup.created);
    put_u64(value, 0x08, dup.modified);
    put_u64(value, 0x10, dup.changed);
    put_u64(value, 0x18, dup.accessed);
    put_u32(value, 0x20, attributes);
}

/// The UTF-16 bytes of `units`.
pub fn bytes_of(units: &[u16]) -> Result<KVec<u8>> {
    let mut v = mem::vec_with_capacity(2 * units.len())?;
    for u in units {
        let _ = v.push_within_capacity(u.to_le_bytes()[0]);
        let _ = v.push_within_capacity(u.to_le_bytes()[1]);
    }
    Ok(v)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn windows_names() {
        for good in ["a", "notes.txt", "Ünïcödé ✓", ".profile", "a b", "CONFIG", "COM10"] {
            assert_eq!(check_windows(good), Ok(()), "{good}");
        }
        let bad = ["a:b", "a*", "q?", "x\"y", "<", ">", "|", "back\\slash", "end.", "end "];
        for bad in bad.into_iter().chain(["NUL", "con.d", "com1.txt", "\u{1}"]) {
            assert_eq!(check_windows(bad), Err(Error::BadName), "{bad}");
        }
    }

    #[test]
    fn short_names_are_dos_names() {
        assert_eq!(namespace_of("README.TXT"), namespace::WIN32_AND_DOS);
        assert_eq!(namespace_of("A"), namespace::WIN32_AND_DOS);
        assert_eq!(namespace_of("FILE~1"), namespace::WIN32_AND_DOS);
        for long in ["readme.txt", "README.TEXT", "NINECHARS", "A.B.C", "A B", ".PROFILE", "X."] {
            assert_eq!(namespace_of(long), namespace::WIN32, "{long}");
        }
    }

    #[test]
    fn times_round_trip() {
        let ns = 1_700_000_000_123_456_789;
        assert_eq!(crate::fs::nt_time(to_nt(ns)), coarse(ns));
    }
}
