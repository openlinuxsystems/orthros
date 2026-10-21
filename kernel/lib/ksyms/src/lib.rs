// SPDX-FileCopyrightText: 2026 The OrthrOS contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! The kernel symbol table that symbolizes Radix backtraces.
//!
//! The kernel image reserves a fixed-size section, `.ksyms`, filled with
//! zeros at link time. After linking, `ob build` reads the function symbols
//! from the ELF symbol table, demangles them, encodes them with [`encode`]
//! and writes the result into the section in place, so no address in the
//! image moves. At run time [`Symbols::parse`] finds the table and
//! [`Symbols::lookup`] turns a code address into `function+offset`.
//!
//! The format, all integers little-endian:
//!
//! ```text
//! 0   8   magic "RADIXSYM"
//! 8   4   format version (1)
//! 12  4   number of symbols, n
//! 16  8   base: address that symbol offsets are relative to
//! 24  4   total length of the table in bytes
//! 28  4   reserved, zero
//! 32  16n symbols sorted by address: offset from base (u32), size (u32),
//!         name offset from the start of the table (u32), name length (u32)
//! ... names, UTF-8, not terminated
//! ```
//!
//! The format is also described in radix-debug(7).

#![cfg_attr(not(test), no_std)]
#![forbid(unsafe_code)]

use core::fmt;

/// First eight bytes of a table.
pub const MAGIC: [u8; 8] = *b"RADIXSYM";
/// Format version.
pub const VERSION: u32 = 1;
/// Bytes before the first symbol entry.
pub const HEADER: usize = 32;
/// Bytes per symbol entry.
pub const ENTRY: usize = 16;

fn u32_at(bytes: &[u8], offset: usize) -> Option<u32> {
    Some(u32::from_le_bytes(bytes.get(offset..offset + 4)?.try_into().ok()?))
}

fn u64_at(bytes: &[u8], offset: usize) -> Option<u64> {
    Some(u64::from_le_bytes(bytes.get(offset..offset + 8)?.try_into().ok()?))
}

/// One symbol for [`encode`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Symbol<'a> {
    /// Start address.
    pub address: u64,
    /// Size in bytes, or 0 if unknown.
    pub size: u64,
    /// Demangled name.
    pub name: &'a str,
}

/// Why a table could not be encoded.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EncodeError {
    /// The output buffer is too small; the table needs this many bytes.
    TooLarge(usize),
    /// The symbols are not sorted by address.
    Unsorted,
    /// A symbol lies more than 4 GiB above the first one.
    OutOfRange,
}

impl fmt::Display for EncodeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            EncodeError::TooLarge(needed) => {
                write!(f, "the symbol table needs {needed} bytes, more than the space reserved")
            }
            EncodeError::Unsorted => f.write_str("symbols are not sorted by address"),
            EncodeError::OutOfRange => f.write_str("symbols span more than 4 GiB"),
        }
    }
}

/// Bytes the table for `symbols` takes.
pub fn encoded_len(symbols: &[Symbol<'_>]) -> usize {
    HEADER + symbols.len() * ENTRY + symbols.iter().map(|s| s.name.len()).sum::<usize>()
}

/// Writes the table for `symbols`, which must be sorted by address, to the
/// start of `out` and zeroes the rest. Returns the bytes used.
pub fn encode(symbols: &[Symbol<'_>], out: &mut [u8]) -> Result<usize, EncodeError> {
    let total = encoded_len(symbols);
    if total > out.len() || u32::try_from(total).is_err() {
        return Err(EncodeError::TooLarge(total));
    }
    if symbols.windows(2).any(|pair| pair[0].address > pair[1].address) {
        return Err(EncodeError::Unsorted);
    }
    let base = symbols.first().map_or(0, |s| s.address);
    out.fill(0);
    out[..8].copy_from_slice(&MAGIC);
    out[8..12].copy_from_slice(&VERSION.to_le_bytes());
    out[12..16].copy_from_slice(&(symbols.len() as u32).to_le_bytes());
    out[16..24].copy_from_slice(&base.to_le_bytes());
    out[24..28].copy_from_slice(&(total as u32).to_le_bytes());
    let mut name_at = HEADER + symbols.len() * ENTRY;
    for (i, symbol) in symbols.iter().enumerate() {
        let offset = u32::try_from(symbol.address - base).map_err(|_| EncodeError::OutOfRange)?;
        let size = u32::try_from(symbol.size).unwrap_or(u32::MAX);
        let entry = &mut out[HEADER + i * ENTRY..HEADER + (i + 1) * ENTRY];
        entry[0..4].copy_from_slice(&offset.to_le_bytes());
        entry[4..8].copy_from_slice(&size.to_le_bytes());
        entry[8..12].copy_from_slice(&(name_at as u32).to_le_bytes());
        entry[12..16].copy_from_slice(&(symbol.name.len() as u32).to_le_bytes());
        out[name_at..name_at + symbol.name.len()].copy_from_slice(symbol.name.as_bytes());
        name_at += symbol.name.len();
    }
    Ok(total)
}

/// A symbol table found in the kernel image.
#[derive(Clone, Copy, Debug)]
pub struct Symbols<'a> {
    bytes: &'a [u8],
    count: usize,
    base: u64,
}

/// The function an address belongs to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Location<'a> {
    /// The function's demangled name.
    pub name: &'a str,
    /// Bytes from the start of the function.
    pub offset: u64,
}

impl fmt::Display for Location<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}+{:#x}", self.name, self.offset)
    }
}

impl<'a> Symbols<'a> {
    /// Checks the table at the start of `bytes`. Returns `None` if there is
    /// none (the section was never filled in) or it is malformed.
    pub fn parse(bytes: &'a [u8]) -> Option<Symbols<'a>> {
        if bytes.get(..8)? != MAGIC || u32_at(bytes, 8)? != VERSION {
            return None;
        }
        let count = u32_at(bytes, 12)? as usize;
        let total = u32_at(bytes, 24)? as usize;
        let bytes = bytes.get(..total)?;
        if HEADER.checked_add(count.checked_mul(ENTRY)?)? > total {
            return None;
        }
        Some(Symbols { bytes, count, base: u64_at(bytes, 16)? })
    }

    /// Number of symbols.
    pub fn len(&self) -> usize {
        self.count
    }

    /// Whether the table is empty.
    pub fn is_empty(&self) -> bool {
        self.count == 0
    }

    fn entry(&self, index: usize) -> Option<(u64, u64, &'a str)> {
        let at = HEADER + index * ENTRY;
        let address = self.base + u64::from(u32_at(self.bytes, at)?);
        let size = u64::from(u32_at(self.bytes, at + 4)?);
        let name_at = u32_at(self.bytes, at + 8)? as usize;
        let name_len = u32_at(self.bytes, at + 12)? as usize;
        let name = self.bytes.get(name_at..name_at.checked_add(name_len)?)?;
        Some((address, size, core::str::from_utf8(name).ok()?))
    }

    /// The function containing `address`. A symbol of unknown size covers
    /// everything up to the next symbol.
    pub fn lookup(&self, address: u64) -> Option<Location<'a>> {
        // Index of the first symbol above `address`.
        let (mut low, mut high) = (0, self.count);
        while low < high {
            let mid = low + (high - low) / 2;
            if self.entry(mid)?.0 <= address {
                low = mid + 1;
            } else {
                high = mid;
            }
        }
        let (start, size, name) = self.entry(low.checked_sub(1)?)?;
        let offset = address - start;
        (size == 0 || offset < size).then_some(Location { name, offset })
    }

    /// The address of the first symbol named exactly `name`.
    pub fn address_of(&self, name: &str) -> Option<u64> {
        (0..self.count).filter_map(|i| self.entry(i)).find(|e| e.2 == name).map(|e| e.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn table(symbols: &[Symbol<'_>]) -> Vec<u8> {
        let mut out = vec![0xaa; encoded_len(symbols) + 64];
        let used = encode(symbols, &mut out).unwrap();
        assert_eq!(used, encoded_len(symbols));
        assert!(out[used..].iter().all(|&b| b == 0));
        out
    }

    #[test]
    fn looks_up_functions() {
        let symbols = [
            Symbol { address: 0xffff_ffff_8010_0000, size: 0x40, name: "radix::kernel_main" },
            Symbol { address: 0xffff_ffff_8010_0040, size: 0, name: "core::panicking::panic" },
            Symbol { address: 0xffff_ffff_8010_0100, size: 0x10, name: "radix::selftest::fault" },
        ];
        let bytes = table(&symbols);
        let table = Symbols::parse(&bytes).unwrap();
        assert_eq!(table.len(), 3);
        let at = |address| table.lookup(address).map(|l| l.to_string());
        assert_eq!(at(0xffff_ffff_8010_0000).as_deref(), Some("radix::kernel_main+0x0"));
        assert_eq!(at(0xffff_ffff_8010_003f).as_deref(), Some("radix::kernel_main+0x3f"));
        assert_eq!(at(0xffff_ffff_8010_00ff).as_deref(), Some("core::panicking::panic+0xbf"));
        assert_eq!(at(0xffff_ffff_8010_0105).as_deref(), Some("radix::selftest::fault+0x5"));
        assert_eq!(at(0xffff_ffff_8010_0110), None, "past the last function");
        assert_eq!(at(0xffff_ffff_800f_ffff), None, "before the first function");
        assert_eq!(table.address_of("radix::selftest::fault"), Some(0xffff_ffff_8010_0100));
    }

    #[test]
    fn empty_and_unfilled_tables() {
        assert!(Symbols::parse(&[0; 4096]).is_none());
        let bytes = table(&[]);
        let table = Symbols::parse(&bytes).unwrap();
        assert!(table.is_empty());
        assert_eq!(table.lookup(0x1000), None);
    }

    #[test]
    fn rejects_bad_input() {
        let a = Symbol { address: 0x2000, size: 1, name: "a" };
        let b = Symbol { address: 0x1000, size: 1, name: "b" };
        assert_eq!(encode(&[a, b], &mut [0; 256]), Err(EncodeError::Unsorted));
        assert_eq!(encode(&[b, a], &mut [0; 40]), Err(EncodeError::TooLarge(66)));
        let far = Symbol { address: 0x1_0000_2000, size: 1, name: "far" };
        assert_eq!(encode(&[b, far], &mut [0; 256]), Err(EncodeError::OutOfRange));
        let mut bytes = table(&[a]);
        bytes[24] = 0xff;
        assert!(Symbols::parse(&bytes[..60]).is_none(), "length beyond the slice");
    }
}
