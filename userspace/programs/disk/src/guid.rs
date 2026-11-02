// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! GUIDs as GPT keeps them (UEFI 2.10, appendix A): 16 bytes whose first
//! three fields are little-endian on the disk and big-endian in the text
//! form, `C12A7328-F81F-11D2-BA4B-00A0C93EC93B`.

use std::fmt;

use crate::rng::Rng;

/// A GUID, in its on-disk byte order.
#[derive(Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Guid(pub [u8; 16]);

/// Where each byte of the text form is on the disk.
const ORDER: [usize; 16] = [3, 2, 1, 0, 5, 4, 7, 6, 8, 9, 10, 11, 12, 13, 14, 15];

/// The value of the hexadecimal digit `c`, for [`Guid::from_text`].
const fn digit(c: u8) -> u8 {
    match c {
        b'0'..=b'9' => c - b'0',
        b'a'..=b'f' => c - b'a' + 10,
        b'A'..=b'F' => c - b'A' + 10,
        _ => 0xff,
    }
}

impl Guid {
    /// The GUID of no type: an unused GPT entry.
    pub const ZERO: Guid = Guid([0; 16]);

    /// The GUID written `text` in 8-4-4-4-12 hexadecimal digits, either
    /// case, perhaps in braces; `None` if it is not one.
    pub const fn from_text(text: &str) -> Option<Guid> {
        let mut bytes = text.as_bytes();
        if let [b'{', inner @ .., b'}'] = bytes {
            bytes = inner;
        }
        if bytes.len() != 36 {
            return None;
        }
        let mut text_order = [0u8; 16];
        let (mut at, mut n) = (0, 0);
        while at < 36 {
            if at == 8 || at == 13 || at == 18 || at == 23 {
                if bytes[at] != b'-' {
                    return None;
                }
                at += 1;
                continue;
            }
            let (high, low) = (digit(bytes[at]), digit(bytes[at + 1]));
            if high > 15 || low > 15 {
                return None;
            }
            text_order[n] = high << 4 | low;
            n += 1;
            at += 2;
        }
        let mut disk = [0u8; 16];
        let mut i = 0;
        while i < 16 {
            disk[ORDER[i]] = text_order[i];
            i += 1;
        }
        Some(Guid(disk))
    }

    /// [`Guid::from_text`] for a GUID the code names, which must be one.
    ///
    /// # Panics
    ///
    /// If `text` is not a GUID: at compile time, in a constant.
    pub const fn known(text: &str) -> Guid {
        match Guid::from_text(text) {
            Some(guid) => guid,
            None => panic!("not a GUID"),
        }
    }

    /// A random GUID, of version 4 (RFC 9562).
    pub fn random(rng: &mut Rng) -> Guid {
        let mut text_order = [0u8; 16];
        rng.fill(&mut text_order);
        text_order[6] = (text_order[6] & 0x0f) | 0x40;
        text_order[8] = (text_order[8] & 0x3f) | 0x80;
        let mut disk = [0u8; 16];
        for (i, &at) in ORDER.iter().enumerate() {
            disk[at] = text_order[i];
        }
        Guid(disk)
    }

    /// Whether this is [`Guid::ZERO`].
    pub fn is_zero(&self) -> bool {
        self.0 == [0; 16]
    }

    /// The GUID in lowercase, as blkid, lsblk and the links in
    /// `/devices/disk/by-partuuid` give it.
    pub fn lower(&self) -> String {
        self.to_string().to_ascii_lowercase()
    }
}

impl fmt::Display for Guid {
    /// In uppercase, as fdisk and sgdisk print it.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for (i, &at) in ORDER.iter().enumerate() {
            if matches!(i, 4 | 6 | 8 | 10) {
                f.write_str("-")?;
            }
            write!(f, "{:02X}", self.0[at])?;
        }
        Ok(())
    }
}

impl fmt::Debug for Guid {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Guid({self})")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_round_trips_in_disk_order() {
        let efi = Guid::known("C12A7328-F81F-11D2-BA4B-00A0C93EC93B");
        assert_eq!(efi.0[..4], [0x28, 0x73, 0x2a, 0xc1]);
        assert_eq!(efi.0[8..], [0xba, 0x4b, 0x00, 0xa0, 0xc9, 0x3e, 0xc9, 0x3b]);
        assert_eq!(efi.to_string(), "C12A7328-F81F-11D2-BA4B-00A0C93EC93B");
        assert_eq!(efi.lower(), "c12a7328-f81f-11d2-ba4b-00a0c93ec93b");
        assert_eq!(Guid::from_text("{c12a7328-f81f-11d2-ba4b-00a0c93ec93b}"), Some(efi));
        for bad in ["", "C12A7328F81F11D2BA4B00A0C93EC93B", "C12A7328-F81F-11D2-BA4B-00A0C93EC93G"]
        {
            assert_eq!(Guid::from_text(bad), None, "{bad}");
        }
    }

    #[test]
    fn random_ones_are_version_4() {
        let mut rng = Rng::seeded(7);
        let a = Guid::random(&mut rng);
        let b = Guid::random(&mut rng);
        assert_ne!(a, b);
        let text = a.to_string();
        assert_eq!(&text[14..15], "4");
        assert!(matches!(&text[19..20], "8" | "9" | "A" | "B"));
    }
}
