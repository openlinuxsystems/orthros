// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Changing file records: a fresh record's header, attributes made, put
//! in, resized and taken out, and the update sequence put on before a
//! record or index block goes to the disk.
//!
//! Records are changed in memory with their update sequence put back
//! ([`crate::record::fix_up`]), as they are read, and [`fix_down`] puts it
//! on again for the write: the sequence number goes up by one, each
//! stride's last two bytes go into the array, and the number takes their
//! place. A write cut short then leaves strides that end in different
//! numbers, which the next read refuses.
//!
//! A record's attributes are in order of their type, and of their name
//! among those of one type, unnamed first, as Windows keeps them; each has
//! a number of its own in the record (its instance), from the record's
//! next number, which the attribute list names it by. The header counts
//! the record's `$FILE_NAME`s (its links, a short name's among them, as
//! Windows and ntfs-3g count them), and [`insert`] and [`remove`] keep
//! the count.

use radix_vfs::mem::{self, KVec};
use radix_vfs::{Error, Result};

use crate::record::{STRIDE, attr_flags, kind, u16_at, u32_at};

pub(crate) fn put_u16(b: &mut [u8], at: usize, v: u16) {
    b[at..at + 2].copy_from_slice(&v.to_le_bytes());
}

pub(crate) fn put_u32(b: &mut [u8], at: usize, v: u32) {
    b[at..at + 4].copy_from_slice(&v.to_le_bytes());
}

pub(crate) fn put_u64(b: &mut [u8], at: usize, v: u64) {
    b[at..at + 8].copy_from_slice(&v.to_le_bytes());
}

/// Where a record's update sequence array is.
pub const RECORD_USA: usize = 0x30;

/// Where an index block's update sequence array is.
pub const BLOCK_USA: usize = 0x28;

/// Where the attributes of a record of `size` bytes start: after its
/// header and update sequence array, on 8 bytes.
pub fn first_attr(size: usize) -> usize {
    (RECORD_USA + 2 * (1 + size / STRIDE)).next_multiple_of(8)
}

/// Puts the update sequence on `buf`, a record or index block whose
/// sequence was put back ([`crate::record::fix_up`]) and whose header
/// says where its array is: the number goes up by one (0 and 0xffff are
/// skipped, as Windows skips them), each stride's last two bytes are kept
/// in the array, and the number takes their place. [`crate::record::fix_up`]
/// undoes it.
pub fn fix_down(buf: &mut [u8]) -> Result<()> {
    if buf.len() < STRIDE || !buf.len().is_multiple_of(STRIDE) {
        return Err(Error::Corrupt);
    }
    let at = usize::from(u16_at(buf, 4));
    let count = usize::from(u16_at(buf, 6));
    let strides = buf.len() / STRIDE;
    if count != strides + 1 || at % 2 != 0 || at + 2 * count > STRIDE - 2 {
        return Err(Error::Corrupt);
    }
    let mut number = u16_at(buf, at).wrapping_add(1);
    if number == 0 || number == 0xffff {
        number = 1;
    }
    put_u16(buf, at, number);
    for i in 0..strides {
        let end = (i + 1) * STRIDE - 2;
        let saved = at + 2 * (i + 1);
        buf[saved] = buf[end];
        buf[saved + 1] = buf[end + 1];
        put_u16(buf, end, number);
    }
    Ok(())
}

/// Makes `buf` a fresh record, number `number`, its sequence number
/// `sequence` and its flags `flags` ([`crate::record::flags`]), with no
/// attributes. Records of NTFS 3.1 keep their own number in the header.
pub fn format_record(buf: &mut [u8], number: u64, sequence: u16, flags: u16) {
    let size = buf.len();
    buf.fill(0);
    buf[..4].copy_from_slice(b"FILE");
    put_u16(buf, 4, RECORD_USA as u16);
    put_u16(buf, 6, (1 + size / STRIDE) as u16);
    put_u16(buf, 0x10, sequence);
    let first = first_attr(size);
    put_u16(buf, 0x14, first as u16);
    put_u16(buf, 0x16, flags);
    put_u32(buf, 0x18, (first + 8) as u32);
    put_u32(buf, 0x1c, size as u32);
    // The update sequence number starts at 1.
    put_u16(buf, RECORD_USA, 1);
    put_u32(buf, 0x2c, number as u32);
    put_u32(buf, first, kind::END);
}

/// The bytes of the record `buf` in use, up to and with its end marker.
pub fn used(buf: &[u8]) -> usize {
    u32_at(buf, 0x18) as usize
}

/// How many more bytes the record `buf` has room for.
pub fn room(buf: &[u8]) -> usize {
    buf.len().saturating_sub(used(buf))
}

/// The record's next attribute number, which it takes for itself.
pub fn next_instance(buf: &mut [u8]) -> u16 {
    let n = u16_at(buf, 0x28);
    put_u16(buf, 0x28, n.wrapping_add(1));
    n
}

/// The offsets of the attributes of the record `buf`, which must have been
/// checked ([`crate::record::Record::new`]), up to the end marker.
fn offsets(buf: &[u8]) -> impl Iterator<Item = usize> + '_ {
    let mut at = usize::from(u16_at(buf, 0x14));
    let end = used(buf);
    core::iter::from_fn(move || {
        if at + 8 > end || u32_at(buf, at) == kind::END {
            return None;
        }
        let len = u32_at(buf, at + 4) as usize;
        if len < 0x18 || at + len > end {
            return None;
        }
        let this = at;
        at += len;
        Some(this)
    })
}

/// The name of the attribute at `at` of `buf`, as bytes.
fn name_at(buf: &[u8], at: usize) -> &[u8] {
    let off = at + usize::from(u16_at(buf, at + 0x0a));
    &buf[off..off + 2 * usize::from(buf[at + 9])]
}

/// Where the attribute of type `kind` named `name` (UTF-16 bytes, empty
/// for none) is in `buf`, if it is there.
pub fn find(buf: &[u8], kind: u32, name: &[u8]) -> Option<usize> {
    offsets(buf).find(|&at| u32_at(buf, at) == kind && name_at(buf, at) == name)
}

/// Where the attributes of type `kind` are in `buf`.
pub fn find_all(buf: &[u8], kind: u32) -> impl Iterator<Item = usize> + '_ {
    offsets(buf).filter(move |&at| u32_at(buf, at) == kind)
}

/// Where an attribute of type `kind` named `name` goes among those of
/// `buf`: after every one of a lower type, or of its type and a name that
/// is not greater.
fn place(buf: &[u8], kind: u32, name: &[u8]) -> usize {
    for at in offsets(buf) {
        let k = u32_at(buf, at);
        if k > kind || (k == kind && name_greater(name_at(buf, at), name)) {
            return at;
        }
    }
    // Before the end marker.
    used(buf) - 8
}

/// Whether the UTF-16 name `a` (bytes) sorts after `b`, unit by unit, an
/// unnamed attribute first.
fn name_greater(a: &[u8], b: &[u8]) -> bool {
    let a_units = a.as_chunks::<2>().0.iter().map(|u| u16::from_le_bytes(*u));
    let b_units = b.as_chunks::<2>().0.iter().map(|u| u16::from_le_bytes(*u));
    a_units.cmp(b_units) == core::cmp::Ordering::Greater
}

/// Puts the attribute `attr` (whole, its instance already set) into the
/// record `buf` where its type and name go; returns where. Fails with
/// [`Error::NoSpace`] if the record has no room for it.
pub fn insert(buf: &mut [u8], attr: &[u8]) -> Result<usize> {
    if attr.len() > room(buf) {
        return Err(Error::NoSpace);
    }
    let kind = u32_at(attr, 0);
    let name_off = usize::from(u16_at(attr, 0x0a));
    let name = &attr[name_off..name_off + 2 * usize::from(attr[9])];
    let at = place(buf, kind, name);
    let end = used(buf);
    buf.copy_within(at..end, at + attr.len());
    buf[at..at + attr.len()].copy_from_slice(attr);
    put_u32(buf, 0x18, (end + attr.len()) as u32);
    if kind == kind::FILE_NAME {
        put_u16(buf, 0x12, u16_at(buf, 0x12).saturating_add(1));
    }
    Ok(at)
}

/// Takes the attribute at `at` out of the record `buf`.
pub fn remove(buf: &mut [u8], at: usize) {
    if u32_at(buf, at) == kind::FILE_NAME {
        put_u16(buf, 0x12, u16_at(buf, 0x12).saturating_sub(1));
    }
    let len = u32_at(buf, at + 4) as usize;
    let end = used(buf);
    buf.copy_within(at + len..end, at);
    buf[end - len..end].fill(0);
    put_u32(buf, 0x18, (end - len) as u32);
}

/// Makes the attribute at `at` of `buf` `len` bytes long (a multiple of
/// 8), moving those after it: its bytes past the old length are zeros,
/// and those past the new one are lost. Fails with [`Error::NoSpace`] if
/// the record has no room.
pub fn resize(buf: &mut [u8], at: usize, len: usize) -> Result<()> {
    let old = u32_at(buf, at + 4) as usize;
    let end = used(buf);
    if len > old {
        let grow = len - old;
        if grow > room(buf) {
            return Err(Error::NoSpace);
        }
        buf.copy_within(at + old..end, at + len);
        buf[at + old..at + len].fill(0);
        put_u32(buf, 0x18, (end + grow) as u32);
    } else {
        let shrink = old - len;
        buf.copy_within(at + old..end, at + len);
        buf[end - shrink..end].fill(0);
        put_u32(buf, 0x18, (end - shrink) as u32);
    }
    put_u32(buf, at + 4, len as u32);
    Ok(())
}

/// The bytes of a resident attribute of type `kind`, named `name` (UTF-16
/// bytes), instance `instance`, holding `value`; `indexed` for one a
/// directory's index holds the value of (a `$FILE_NAME`).
pub fn resident(
    kind: u32,
    name: &[u8],
    instance: u16,
    value: &[u8],
    indexed: bool,
) -> Result<KVec<u8>> {
    let value_at = (0x18 + name.len()).next_multiple_of(8);
    let len = (value_at + value.len()).next_multiple_of(8);
    let mut a = crate::fs::zeroed(len)?;
    put_u32(&mut a, 0, kind);
    put_u32(&mut a, 4, len as u32);
    a[9] = (name.len() / 2) as u8;
    put_u16(&mut a, 0x0a, 0x18);
    put_u16(&mut a, 0x0e, instance);
    put_u32(&mut a, 0x10, value.len() as u32);
    put_u16(&mut a, 0x14, value_at as u16);
    a[0x16] = u8::from(indexed);
    a[0x18..0x18 + name.len()].copy_from_slice(name);
    a[value_at..value_at + value.len()].copy_from_slice(value);
    Ok(a)
}

/// What a non-resident attribute's header holds.
#[derive(Clone, Copy, Debug, Default)]
pub struct Sizes {
    /// The bytes its clusters hold.
    pub allocated: u64,
    /// Its value's size.
    pub size: u64,
    /// How much of it was written.
    pub initialized: u64,
    /// For compressed or sparse data: the bytes of its clusters on the
    /// disk, holes left out.
    pub compressed: u64,
    /// For compressed or sparse data: its compression unit, the log2 of
    /// its clusters.
    pub unit: u8,
    /// Whether it is compressed or sparse
    /// ([`crate::record::attr_flags`]), which data that was resident did
    /// not say.
    pub packed: u16,
}

/// The bytes of a non-resident attribute of type `kind`, named `name`
/// (UTF-16 bytes), instance `instance` and flags `flags`, from VCN
/// `lowest` to `highest`, whose packed run list is `runs`
/// ([`crate::runs::Runs::pack`]); its sizes are `sizes` in a first piece
/// (`lowest` 0) and 0 in the others, as Windows writes them. Compressed
/// or sparse data's header is 8 bytes longer, for the bytes on the disk.
#[allow(clippy::too_many_arguments)]
pub fn non_resident(
    kind: u32,
    name: &[u8],
    instance: u16,
    flags: u16,
    lowest: u64,
    highest: u64,
    runs: &[u8],
    sizes: Sizes,
) -> Result<KVec<u8>> {
    let packed = flags & (attr_flags::COMPRESSED | attr_flags::SPARSE) != 0;
    let header = if packed { 0x48 } else { 0x40 };
    let runs_at = (header + name.len()).next_multiple_of(8);
    let len = (runs_at + runs.len()).next_multiple_of(8);
    let mut a = crate::fs::zeroed(len)?;
    put_u32(&mut a, 0, kind);
    put_u32(&mut a, 4, len as u32);
    a[8] = 1;
    a[9] = (name.len() / 2) as u8;
    put_u16(&mut a, 0x0a, header as u16);
    put_u16(&mut a, 0x0c, flags);
    put_u16(&mut a, 0x0e, instance);
    put_u64(&mut a, 0x10, lowest);
    put_u64(&mut a, 0x18, highest);
    put_u16(&mut a, 0x20, runs_at as u16);
    if packed {
        a[0x22] = sizes.unit;
    }
    if lowest == 0 {
        put_u64(&mut a, 0x28, sizes.allocated);
        put_u64(&mut a, 0x30, sizes.size);
        put_u64(&mut a, 0x38, sizes.initialized);
        if packed {
            put_u64(&mut a, 0x40, sizes.compressed);
        }
    }
    a[header..header + name.len()].copy_from_slice(name);
    a[runs_at..runs_at + runs.len()].copy_from_slice(runs);
    Ok(a)
}

/// Sets the value of the resident attribute at `at` of `buf` to `value`,
/// resizing the attribute; fails with [`Error::NoSpace`] if the record has
/// no room.
pub fn set_value(buf: &mut [u8], at: usize, value: &[u8]) -> Result<()> {
    let value_at = usize::from(u16_at(buf, at + 0x14));
    let len = (value_at + value.len()).next_multiple_of(8);
    resize(buf, at, len)?;
    put_u32(buf, at + 0x10, value.len() as u32);
    buf[at + value_at..at + value_at + value.len()].copy_from_slice(value);
    buf[at + value_at + value.len()..at + len].fill(0);
    Ok(())
}

/// The value of the resident attribute at `at` of `buf`, to change in
/// place.
pub fn value_mut(buf: &mut [u8], at: usize) -> &mut [u8] {
    let len = u32_at(buf, at + 0x10) as usize;
    let off = at + usize::from(u16_at(buf, at + 0x14));
    &mut buf[off..off + len]
}

/// `$I30`, the name of a directory's index attributes, as UTF-16 bytes.
pub const I30: [u8; 8] = [b'$', 0, b'I', 0, b'3', 0, b'0', 0];

/// A copy of `bytes`.
pub fn copy(bytes: &[u8]) -> Result<KVec<u8>> {
    Ok(mem::copy(bytes)?)
}
