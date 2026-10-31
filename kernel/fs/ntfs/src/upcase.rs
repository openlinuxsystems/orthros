// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! The up-case table (`$UpCase`), which orders and matches names.
//!
//! A directory's index is sorted by its names' upper cases, unit by unit
//! of UTF-16, as the volume's own table gives them: 65536 units, the
//! upper case of each, which `mkntfs` and Windows write when they format
//! it. Most units map to themselves, so Radix keeps only those that do
//! not, sorted, with ASCII's in a table of their own, since most names
//! are ASCII.

use core::cmp::Ordering;

use radix_vfs::mem::{self, KVec};
use radix_vfs::{Error, Result};

/// The table's size: a unit for each of the 65536.
pub const BYTES: u64 = 65536 * 2;

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

    /// The table whose bytes `read` gives a piece at a time (`read(at,
    /// buf)` fills `buf` with the bytes from `at`), `len` of them: units
    /// past a shorter table map to themselves.
    pub fn load(len: u64, mut read: impl FnMut(u64, &mut [u8]) -> Result<()>) -> Result<Upcase> {
        if !(256..=BYTES).contains(&len) || !len.is_multiple_of(2) {
            return Err(Error::Corrupt);
        }
        let mut table = Upcase::identity();
        let mut buf = [0u8; 512];
        let mut at = 0;
        while at < len {
            let n = (len - at).min(buf.len() as u64) as usize;
            read(at, &mut buf[..n])?;
            for (i, pair) in buf[..n].as_chunks::<2>().0.iter().enumerate() {
                let unit = (at / 2) as usize + i;
                let upper = u16::from_le_bytes(*pair);
                if unit < 128 {
                    table.ascii[unit] = upper;
                } else if upper != unit as u16 {
                    mem::push(&mut table.others, (unit as u16, upper))?;
                }
            }
            at += n as u64;
        }
        Ok(table)
    }

    /// The upper case of `unit`.
    pub fn upper(&self, unit: u16) -> u16 {
        if unit < 128 {
            return self.ascii[usize::from(unit)];
        }
        match self.others.binary_search_by_key(&unit, |&(u, _)| u) {
            Ok(i) => self.others[i].1,
            Err(_) => unit,
        }
    }

    /// How the names `a` and `b` order in an index: by their upper cases,
    /// unit by unit, then by length.
    pub fn compare(&self, a: impl Iterator<Item = u16>, b: impl Iterator<Item = u16>) -> Ordering {
        a.map(|u| self.upper(u)).cmp(b.map(|u| self.upper(u)))
    }
}
