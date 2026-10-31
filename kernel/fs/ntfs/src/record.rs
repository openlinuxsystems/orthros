// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! File records and their attributes.
//!
//! Everything on an NTFS volume is a file, and every file has a record in
//! the master file table: a header, then attributes, each a type, an
//! optional name and a value. A small value is in the record itself
//! (resident); a large one is in clusters elsewhere (non-resident), and
//! the record holds where, as a run list ([`crate::runs`]). A file with
//! too many attributes for its record has more records, which its
//! `$ATTRIBUTE_LIST` names.
//!
//! Records and index blocks are guarded against writes cut short: the
//! last two bytes of each 512 of them are replaced on the disk by an
//! update sequence number, whose array in the header keeps what they
//! were. A record whose ends do not all match its number was half
//! written, and is refused ([`fix_up`]).

use radix_vfs::{Error, Result};

/// The stride of the update sequence: 512 bytes, whatever the sector.
pub const STRIDE: usize = 512;

/// The attribute types Radix reads.
pub mod kind {
    /// Times and DOS attributes.
    pub const STANDARD_INFORMATION: u32 = 0x10;
    /// Where the attributes of a file of more than one record are.
    pub const ATTRIBUTE_LIST: u32 = 0x20;
    /// A name of the file, and the directory it is in.
    pub const FILE_NAME: u32 = 0x30;
    /// The volume's label (`$Volume`).
    pub const VOLUME_NAME: u32 = 0x60;
    /// The volume's version and flags (`$Volume`).
    pub const VOLUME_INFORMATION: u32 = 0x70;
    /// The file's data; named, an alternate data stream.
    pub const DATA: u32 = 0x80;
    /// A directory index's root.
    pub const INDEX_ROOT: u32 = 0x90;
    /// A directory index's blocks.
    pub const INDEX_ALLOCATION: u32 = 0xa0;
    /// Which index blocks are in use.
    pub const BITMAP: u32 = 0xb0;
    /// What a reparse point (a link, a mount point) points to.
    pub const REPARSE_POINT: u32 = 0xc0;
    /// The end of a record's attributes.
    pub const END: u32 = 0xffff_ffff;
}

/// Bits of a record's flags.
pub mod flags {
    /// The record is a file's.
    pub const IN_USE: u16 = 1 << 0;
    /// The file is a directory.
    pub const DIRECTORY: u16 = 1 << 1;
}

/// Bits of an attribute's flags.
pub mod attr_flags {
    /// Its data is compressed (LZNT1).
    pub const COMPRESSED: u16 = 0x0001;
    /// Its data is encrypted (EFS).
    pub const ENCRYPTED: u16 = 0x4000;
    /// Its data has holes.
    pub const SPARSE: u16 = 0x8000;
}

pub(crate) fn u16_at(b: &[u8], at: usize) -> u16 {
    u16::from_le_bytes([b[at], b[at + 1]])
}

pub(crate) fn u32_at(b: &[u8], at: usize) -> u32 {
    u32::from_le_bytes([b[at], b[at + 1], b[at + 2], b[at + 3]])
}

pub(crate) fn u64_at(b: &[u8], at: usize) -> u64 {
    let mut bytes = [0u8; 8];
    bytes.copy_from_slice(&b[at..at + 8]);
    u64::from_le_bytes(bytes)
}

/// The record number a file reference names: its low 48 bits; the top
/// 16 are the record's sequence number when the reference was made.
pub fn record_of(reference: u64) -> u64 {
    reference & 0xffff_ffff_ffff
}

/// The sequence number a file reference names.
pub fn sequence_of(reference: u64) -> u16 {
    (reference >> 48) as u16
}

/// Puts back the bytes the update sequence of the record or index block
/// `buf`, whose header must start with `magic`, stands in for. Fails with
/// [`Error::Corrupt`] for another magic, or a record whose strides do not
/// all end in its number.
pub fn fix_up(buf: &mut [u8], magic: &[u8; 4]) -> Result<()> {
    if buf.len() < STRIDE || !buf.len().is_multiple_of(STRIDE) || &buf[..4] != magic {
        return Err(Error::Corrupt);
    }
    let at = usize::from(u16_at(buf, 4));
    let count = usize::from(u16_at(buf, 6));
    let strides = buf.len() / STRIDE;
    // The number, then a unit for each stride.
    if count != strides + 1 || at % 2 != 0 || at + 2 * count > STRIDE - 2 {
        return Err(Error::Corrupt);
    }
    let number = [buf[at], buf[at + 1]];
    for i in 0..strides {
        let end = (i + 1) * STRIDE - 2;
        if buf[end..end + 2] != number {
            return Err(Error::Corrupt);
        }
        let saved = at + 2 * (i + 1);
        buf[end] = buf[saved];
        buf[end + 1] = buf[saved + 1];
    }
    Ok(())
}

#[cfg(kani)]
mod proofs {
    use super::*;

    /// Whatever a record of two strides holds, putting back its update
    /// sequence reads and writes only inside it, and leaves each stride
    /// ending in what the sequence kept for it.
    #[kani::proof]
    #[kani::unwind(5)]
    fn fixups_stay_in_the_record() {
        let mut buf: [u8; 1024] = kani::any();
        let before = buf;
        if fix_up(&mut buf, b"FILE").is_ok() {
            let at = usize::from(u16_at(&before, 4));
            for i in 0..2 {
                let end = (i + 1) * STRIDE - 2;
                let saved = at + 2 * (i + 1);
                assert!(buf[end] == before[saved] && buf[end + 1] == before[saved + 1]);
            }
        }
    }
}

/// A file record, its update sequence put back ([`fix_up`]).
pub struct Record<'a> {
    bytes: &'a [u8],
}

impl<'a> Record<'a> {
    /// The record in `bytes`, its update sequence already put back.
    /// Fails with [`Error::Corrupt`] for a header that does not add up.
    pub fn new(bytes: &'a [u8]) -> Result<Record<'a>> {
        if bytes.len() < 0x30 || &bytes[..4] != b"FILE" {
            return Err(Error::Corrupt);
        }
        let first = usize::from(u16_at(bytes, 0x14));
        let used = u32_at(bytes, 0x18) as usize;
        if first % 8 != 0 || first < 0x2a || used > bytes.len() || first + 4 > used {
            return Err(Error::Corrupt);
        }
        Ok(Record { bytes })
    }

    /// Its flags ([`flags`]).
    pub fn flags(&self) -> u16 {
        u16_at(self.bytes, 0x16)
    }

    /// Whether it is a file's.
    pub fn in_use(&self) -> bool {
        self.flags() & flags::IN_USE != 0
    }

    /// Its sequence number: how many times the record has been reused.
    pub fn sequence(&self) -> u16 {
        u16_at(self.bytes, 0x10)
    }

    /// The file reference of the record a file's other records belong to;
    /// 0 for a base record.
    pub fn base(&self) -> u64 {
        u64_at(self.bytes, 0x20)
    }

    /// Its attributes, in order. A malformed attribute ends them with an
    /// error.
    pub fn attrs(&self) -> Attrs<'a> {
        let first = usize::from(u16_at(self.bytes, 0x14));
        let used = u32_at(self.bytes, 0x18) as usize;
        Attrs { bytes: &self.bytes[..used], at: first, done: false }
    }
}

/// A record's attributes ([`Record::attrs`]).
pub struct Attrs<'a> {
    bytes: &'a [u8],
    at: usize,
    done: bool,
}

impl<'a> Iterator for Attrs<'a> {
    type Item = Result<Attr<'a>>;

    fn next(&mut self) -> Option<Result<Attr<'a>>> {
        if self.done {
            return None;
        }
        let rest = &self.bytes[self.at..];
        if rest.len() < 4 || u32_at(rest, 0) == kind::END {
            self.done = true;
            return None;
        }
        match Attr::new(rest) {
            Ok(attr) => {
                self.at += attr.bytes.len();
                Some(Ok(attr))
            }
            Err(err) => {
                self.done = true;
                Some(Err(err))
            }
        }
    }
}

/// An attribute in a record.
#[derive(Clone, Copy)]
pub struct Attr<'a> {
    bytes: &'a [u8],
}

impl<'a> Attr<'a> {
    /// The attribute at the start of `rest`, checked: its length, name and
    /// value or run list lie within it.
    fn new(rest: &'a [u8]) -> Result<Attr<'a>> {
        if rest.len() < 0x18 {
            return Err(Error::Corrupt);
        }
        let len = u32_at(rest, 4) as usize;
        let non_resident = rest[8] != 0;
        let min = if non_resident { 0x40 } else { 0x18 };
        if len < min || !len.is_multiple_of(8) || len > rest.len() {
            return Err(Error::Corrupt);
        }
        let bytes = &rest[..len];
        let name_end = usize::from(u16_at(bytes, 0x0a)) + 2 * usize::from(bytes[9]);
        if name_end > len {
            return Err(Error::Corrupt);
        }
        if non_resident {
            let runs = usize::from(u16_at(bytes, 0x20));
            let lowest = u64_at(bytes, 0x10);
            let highest = u64_at(bytes, 0x18);
            // The sizes are signed on the disk; none may be negative.
            let sizes = [0x28, 0x30, 0x38].map(|at| u64_at(bytes, at) as i64);
            if runs >= len || (lowest as i64) < 0 || highest.wrapping_add(1) < lowest {
                return Err(Error::Corrupt);
            }
            if sizes.iter().any(|&s| s < 0) {
                return Err(Error::Corrupt);
            }
        } else {
            let value_len = u32_at(bytes, 0x10) as usize;
            let value_at = usize::from(u16_at(bytes, 0x14));
            if value_at.checked_add(value_len).is_none_or(|end| end > len) {
                return Err(Error::Corrupt);
            }
        }
        Ok(Attr { bytes })
    }

    /// Its type ([`kind`]).
    pub fn kind(&self) -> u32 {
        u32_at(self.bytes, 0)
    }

    /// Whether its value lies outside the record.
    pub fn non_resident(&self) -> bool {
        self.bytes[8] != 0
    }

    /// Its name's UTF-16 units, as bytes, little-endian.
    pub fn name(&self) -> &'a [u8] {
        let at = usize::from(u16_at(self.bytes, 0x0a));
        &self.bytes[at..at + 2 * usize::from(self.bytes[9])]
    }

    /// Whether it has no name.
    pub fn unnamed(&self) -> bool {
        self.bytes[9] == 0
    }

    /// Its flags ([`attr_flags`]).
    pub fn flags(&self) -> u16 {
        u16_at(self.bytes, 0x0c)
    }

    /// Its number in its record, which the attribute list names it by.
    pub fn id(&self) -> u16 {
        u16_at(self.bytes, 0x0e)
    }

    /// A resident attribute's value; empty for a non-resident one.
    pub fn value(&self) -> &'a [u8] {
        if self.non_resident() {
            return &[];
        }
        let len = u32_at(self.bytes, 0x10) as usize;
        let at = usize::from(u16_at(self.bytes, 0x14));
        &self.bytes[at..at + len]
    }

    /// A non-resident attribute's first virtual cluster: 0, or where this
    /// piece of it starts when it spans records.
    pub fn lowest_vcn(&self) -> u64 {
        u64_at(self.bytes, 0x10)
    }

    /// Its last virtual cluster.
    pub fn highest_vcn(&self) -> u64 {
        u64_at(self.bytes, 0x18)
    }

    /// Its run list's bytes.
    pub fn runs(&self) -> &'a [u8] {
        &self.bytes[usize::from(u16_at(self.bytes, 0x20))..]
    }

    /// The log2 of the clusters in each compression unit; 0 when it is
    /// not compressed.
    pub fn compression_unit(&self) -> u8 {
        self.bytes[0x22]
    }

    /// The bytes its clusters hold (non-resident).
    pub fn allocated_size(&self) -> u64 {
        u64_at(self.bytes, 0x28)
    }

    /// Its value's size: a resident value's length, or a non-resident
    /// one's as its first piece gives.
    pub fn size(&self) -> u64 {
        if self.non_resident() {
            u64_at(self.bytes, 0x30)
        } else {
            u64::from(u32_at(self.bytes, 0x10))
        }
    }

    /// How much of a non-resident value was written: what lies past it
    /// reads as zeros.
    pub fn initialized_size(&self) -> u64 {
        if self.non_resident() { u64_at(self.bytes, 0x38) } else { self.size() }
    }
}

/// An entry of an `$ATTRIBUTE_LIST`: where one of a file's attributes is.
#[derive(Clone, Copy)]
pub struct ListEntry<'a> {
    bytes: &'a [u8],
}

impl<'a> ListEntry<'a> {
    /// Its attribute's type.
    pub fn kind(&self) -> u32 {
        u32_at(self.bytes, 0)
    }

    /// Its attribute's name, as [`Attr::name`].
    pub fn name(&self) -> &'a [u8] {
        let at = usize::from(self.bytes[7]);
        &self.bytes[at..at + 2 * usize::from(self.bytes[6])]
    }

    /// The first virtual cluster of the piece it names.
    pub fn lowest_vcn(&self) -> u64 {
        u64_at(self.bytes, 8)
    }

    /// The file reference of the record the attribute is in.
    pub fn reference(&self) -> u64 {
        u64_at(self.bytes, 0x10)
    }

    /// The attribute's number in that record ([`Attr::id`]).
    pub fn id(&self) -> u16 {
        u16_at(self.bytes, 0x18)
    }
}

/// The entries of the attribute list `value`, in order. A malformed entry
/// ends them with an error.
pub fn list_entries(value: &[u8]) -> impl Iterator<Item = Result<ListEntry<'_>>> {
    let mut at = 0;
    let mut done = false;
    core::iter::from_fn(move || {
        if done || at >= value.len() {
            return None;
        }
        let rest = &value[at..];
        let len = if rest.len() >= 0x1a { usize::from(u16_at(rest, 4)) } else { 0 };
        let name_end =
            if len >= 0x1a { usize::from(rest[7]) + 2 * usize::from(rest[6]) } else { 0 };
        if len < 0x1a || len % 8 != 0 || len > rest.len() || name_end > len {
            done = true;
            return Some(Err(Error::Corrupt));
        }
        at += len;
        Some(Ok(ListEntry { bytes: &rest[..len] }))
    })
}

/// Whether the UTF-16 name `name`, as bytes, is `want`, ASCII.
pub fn name_is(name: &[u8], want: &str) -> bool {
    name.len() == 2 * want.len()
        && name.as_chunks::<2>().0.iter().zip(want.bytes()).all(|(unit, b)| *unit == [b, 0])
}
