// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! The up-case table (the specification's "Up-case Table").
//!
//! exFAT names compare without regard to case, by the volume's own table
//! of each UTF-16 unit's upper case, which `mkfs` writes into the cluster
//! heap. Most units map to themselves, so the table is usually kept
//! compressed: 0xffff, then a count, stands for that many units in a row
//! that map to themselves. Radix keeps only the units that do not, sorted,
//! with those of ASCII in a table of their own, since most names are.

use radix_vfs::mem::{self, KVec};
use radix_vfs::{Error, Result};

/// The largest table: one unit for each of the 65536.
pub const MAX_BYTES: u64 = 65536 * 2;

/// A volume's up-case table.
pub struct Upcase {
    /// ASCII's upper cases.
    ascii: [u16; 128],
    /// Every other unit that does not map to itself, and what it maps to,
    /// by unit.
    others: KVec<(u16, u16)>,
}

impl Upcase {
    /// The table that maps every unit to itself.
    pub fn identity() -> Upcase {
        Upcase { ascii: core::array::from_fn(|i| i as u16), others: mem::vec() }
    }

    /// The table whose bytes `read` gives, in order, a piece at a time
    /// (`read(at, buf)` fills `buf` with the bytes from `at`), `len`
    /// bytes in all; `checksum` is the one its entry gives.
    pub fn load(
        len: u64,
        checksum: u32,
        mut read: impl FnMut(u64, &mut [u8]) -> Result<()>,
    ) -> Result<Upcase> {
        if len == 0 || len > MAX_BYTES || !len.is_multiple_of(2) {
            return Err(Error::Corrupt);
        }
        let mut table = Upcase::identity();
        let mut sum = 0u32;
        // The unit the next value maps, and whether the value before was
        // the mark of a run.
        let mut unit = 0u32;
        let mut run = false;
        let mut buf = [0u8; 512];
        let mut at = 0;
        while at < len {
            let n = (len - at).min(buf.len() as u64) as usize;
            read(at, &mut buf[..n])?;
            for &byte in &buf[..n] {
                sum = sum.rotate_right(1).wrapping_add(u32::from(byte));
            }
            for pair in buf[..n].as_chunks::<2>().0 {
                let value = u16::from_le_bytes(*pair);
                if unit > 0xffff {
                    continue;
                }
                if run {
                    unit += u32::from(value);
                    run = false;
                } else if u32::from(value) == unit {
                    unit += 1;
                } else if value == 0xffff {
                    run = true;
                } else {
                    table.set(unit as u16, value)?;
                    unit += 1;
                }
            }
            at += n as u64;
        }
        if sum != checksum {
            return Err(Error::Corrupt);
        }
        Ok(table)
    }

    /// Notes that `unit`'s upper case is `upper`. Units come in order.
    fn set(&mut self, unit: u16, upper: u16) -> Result<()> {
        match self.ascii.get_mut(usize::from(unit)) {
            Some(slot) => *slot = upper,
            None => mem::push(&mut self.others, (unit, upper))?,
        }
        Ok(())
    }

    /// The upper case of `unit`.
    pub fn upper(&self, unit: u16) -> u16 {
        match self.ascii.get(usize::from(unit)) {
            Some(&upper) => upper,
            None => match self.others.binary_search_by_key(&unit, |&(u, _)| u) {
                Ok(i) => self.others[i].1,
                Err(_) => unit,
            },
        }
    }

    /// Whether two names, as UTF-16 units, are the same to exFAT.
    pub fn same(&self, a: &[u16], b: &[u16]) -> bool {
        a.len() == b.len() && a.iter().zip(b).all(|(&x, &y)| x == y || self.upper(x) == self.upper(y))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A table as `mkfs` writes one, compressed: a-z map to A-Z, U+00E0
    /// to U+00C0, and runs of units map to themselves.
    fn compressed() -> Vec<u8> {
        let mut units: Vec<u16> = vec![0xffff, 0x61];
        units.extend(0x41..=0x5a);
        units.extend([0xffff, 0x00e0 - 0x7b, 0x00c0, 0xffff, 0x2000]);
        units.iter().flat_map(|u| u.to_le_bytes()).collect()
    }

    fn sum(bytes: &[u8]) -> u32 {
        bytes.iter().fold(0u32, |s, &b| s.rotate_right(1).wrapping_add(u32::from(b)))
    }

    #[test]
    fn loads_a_compressed_table() {
        let bytes = compressed();
        let read = |at: u64, buf: &mut [u8]| {
            buf.copy_from_slice(&bytes[at as usize..at as usize + buf.len()]);
            Ok(())
        };
        let table = Upcase::load(bytes.len() as u64, sum(&bytes), read).unwrap();
        assert_eq!(table.upper(u16::from(b'a')), u16::from(b'A'));
        assert_eq!(table.upper(u16::from(b'Z')), u16::from(b'Z'));
        assert_eq!(table.upper(u16::from(b'{')), u16::from(b'{'));
        assert_eq!(table.upper(0x00e0), 0x00c0);
        assert_eq!(table.upper(0x00e1), 0x00e1);
        assert!(table.same(&[0x61, 0xe0], &[0x41, 0xc0]));
        assert!(!table.same(&[0x61], &[0x41, 0x41]));
        let wrong = Upcase::load(bytes.len() as u64, sum(&bytes) ^ 1, read);
        assert_eq!(wrong.err(), Some(Error::Corrupt));
    }
}
