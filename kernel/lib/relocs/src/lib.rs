//! The kernel relocation table, which lets Radix move itself to a random
//! address at boot (KASLR).
//!
//! The kernel is linked at one address, like any static executable. To run
//! it somewhere else, every absolute address embedded in the image must be
//! moved by the same amount, the *slide*. The linker keeps a record of
//! where those addresses are when asked to (`--emit-relocs`); after every
//! build, `ob` reads them from the ELF file, keeps the ones that point into
//! the kernel's higher-half image, and writes this table into the image's
//! `.krelocs` section, which the linker left empty. At boot, before the
//! kernel runs at its new address, the boot code adds the slide to each
//! location the table lists.
//!
//! Two kinds of location exist in an x86-64 kernel built with the `kernel`
//! code model: 64-bit absolute addresses (in data, and in `movabs`
//! instructions) and 32-bit sign-extended ones (in instructions that name
//! a kernel symbol). Relative addresses need no change, because the code
//! and its targets move together.
//!
//! The format, all integers little-endian:
//!
//! ```text
//! 0   8   magic "RADIXREL"
//! 8   4   format version (1)
//! 12  4   number of 64-bit locations, n64
//! 16  4   number of 32-bit locations, n32
//! 20  4   total length of the table in bytes
//! 24  8   reserved, zero
//! 32  4n64  physical addresses of the 64-bit locations, ascending
//! ... 4n32  physical addresses of the 32-bit locations, ascending
//! ```
//!
//! Locations are physical addresses because that is how the boot code
//! reaches them while it relocates: the image sits below 4 GiB, where the
//! boot page tables map memory one to one. The format is also described in
//! radix-debug(7).

#![cfg_attr(not(test), no_std)]
#![forbid(unsafe_code)]

use core::fmt;

/// First eight bytes of a table.
pub const MAGIC: [u8; 8] = *b"RADIXREL";
/// Format version.
pub const VERSION: u32 = 1;
/// Bytes before the first location.
pub const HEADER: usize = 32;

fn u32_at(bytes: &[u8], offset: usize) -> Option<u32> {
    Some(u32::from_le_bytes(bytes.get(offset..offset + 4)?.try_into().ok()?))
}

/// What a location holds.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    /// A 64-bit absolute address.
    Abs64,
    /// A 32-bit absolute address, sign-extended to 64 bits when used.
    Abs32,
}

/// Why a table could not be encoded.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EncodeError {
    /// The output buffer is too small.
    TooSmall {
        /// Bytes the table needs.
        needed: usize,
    },
    /// The locations are not in ascending order.
    Unsorted,
}

impl fmt::Display for EncodeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            EncodeError::TooSmall { needed } => write!(f, "the table needs {needed} bytes"),
            EncodeError::Unsorted => f.write_str("the locations are not sorted"),
        }
    }
}

/// Bytes a table of `abs64` and `abs32` locations takes.
pub fn encoded_len(abs64: usize, abs32: usize) -> usize {
    HEADER + 4 * (abs64 + abs32)
}

/// Writes a table into `out` and returns its length. The rest of `out` is
/// left as it is.
pub fn encode(abs64: &[u32], abs32: &[u32], out: &mut [u8]) -> Result<usize, EncodeError> {
    let needed = encoded_len(abs64.len(), abs32.len());
    if out.len() < needed {
        return Err(EncodeError::TooSmall { needed });
    }
    if !abs64.is_sorted() || !abs32.is_sorted() {
        return Err(EncodeError::Unsorted);
    }
    out[..8].copy_from_slice(&MAGIC);
    out[8..12].copy_from_slice(&VERSION.to_le_bytes());
    out[12..16].copy_from_slice(&(abs64.len() as u32).to_le_bytes());
    out[16..20].copy_from_slice(&(abs32.len() as u32).to_le_bytes());
    out[20..24].copy_from_slice(&(needed as u32).to_le_bytes());
    out[24..32].fill(0);
    let slots = out[HEADER..needed].as_chunks_mut::<4>().0;
    for (slot, location) in slots.iter_mut().zip(abs64.iter().chain(abs32)) {
        slot.copy_from_slice(&location.to_le_bytes());
    }
    Ok(needed)
}

/// A parsed table.
#[derive(Clone, Copy, Debug)]
pub struct Table<'a> {
    abs64: &'a [u8],
    abs32: &'a [u8],
}

impl<'a> Table<'a> {
    /// Checks the header and finds the two lists. `None` if `bytes` holds
    /// no valid table (for example, the zeros of a kernel that `ob` did not
    /// post-process).
    pub fn parse(bytes: &'a [u8]) -> Option<Table<'a>> {
        if bytes.get(..8)? != MAGIC || u32_at(bytes, 8)? != VERSION {
            return None;
        }
        let abs64 = u32_at(bytes, 12)? as usize;
        let abs32 = u32_at(bytes, 16)? as usize;
        let len = u32_at(bytes, 20)? as usize;
        if len != encoded_len(abs64, abs32) || len > bytes.len() {
            return None;
        }
        let split = HEADER + 4 * abs64;
        Some(Table { abs64: &bytes[HEADER..split], abs32: &bytes[split..len] })
    }

    /// Number of locations.
    pub fn len(&self) -> usize {
        (self.abs64.len() + self.abs32.len()) / 4
    }

    /// Whether the table lists no locations.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Every location, 64-bit ones first.
    pub fn locations(&self) -> impl Iterator<Item = (Kind, u32)> + 'a {
        let words = |bytes: &'a [u8], kind| {
            bytes.as_chunks::<4>().0.iter().map(move |&w| (kind, u32::from_le_bytes(w)))
        };
        words(self.abs64, Kind::Abs64).chain(words(self.abs32, Kind::Abs32))
    }
}

/// A 64-bit address moved by `slide`.
pub fn slide64(value: u64, slide: u64) -> u64 {
    value.wrapping_add(slide)
}

/// A 32-bit sign-extended address moved by `slide`, or `None` if the
/// result no longer fits (the slide would leave the top 2 GiB).
pub fn slide32(value: u32, slide: u64) -> Option<u32> {
    let moved = i64::from(value as i32).checked_add(i64::try_from(slide).ok()?)?;
    i32::try_from(moved).ok().map(|v| v as u32)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips() {
        let mut buf = vec![0xffu8; 64];
        let len = encode(&[0x10_0008, 0x20_0000], &[0x10_0100], &mut buf).unwrap();
        assert_eq!(len, 44);
        let table = Table::parse(&buf).unwrap();
        assert_eq!(table.len(), 3);
        let all: Vec<_> = table.locations().collect();
        assert_eq!(
            all,
            [(Kind::Abs64, 0x10_0008), (Kind::Abs64, 0x20_0000), (Kind::Abs32, 0x10_0100)]
        );
        assert_eq!(encode(&[2, 1], &[], &mut buf), Err(EncodeError::Unsorted));
        assert_eq!(encode(&[1; 9], &[], &mut buf[..40]), Err(EncodeError::TooSmall { needed: 68 }));
        assert!(Table::parse(&[0; 64]).is_none());
        buf[20] = 99;
        assert!(Table::parse(&buf).is_none());
    }

    #[test]
    fn slides_addresses() {
        assert_eq!(slide64(0xffff_ffff_8010_0000, 0x200_0000), 0xffff_ffff_8210_0000);
        // 0xffffffff80100000 as a 32-bit sign-extended value.
        assert_eq!(slide32(0x8010_0000, 0x200_0000), Some(0x8210_0000));
        assert_eq!(slide32(0x7fff_f000, 0x1000), None);
        assert_eq!(slide32(0xffff_f000, 0x1000), Some(0));
    }
}
