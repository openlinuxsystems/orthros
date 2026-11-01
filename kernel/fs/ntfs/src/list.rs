// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Attribute lists written (ADR 0239).
//!
//! A file whose attributes do not fit in its record has more records:
//! extension records, each of which names the first, its base record.
//! The base record then holds an `$ATTRIBUTE_LIST`, an entry for each of
//! the file's attributes but the list itself, wherever it is: its type,
//! name and first VCN, the record it is in and its number there, sorted
//! by type, then name, then VCN, as Windows sorts them.
//!
//! Radix spreads a file's data, its unnamed `$DATA`, over extension
//! records when the runs of a fragmented file outgrow its record, as
//! Windows and ntfs-3g do: a piece of the runs in each, the first carrying
//! the sizes. The list is rebuilt from the base record and the pieces
//! whenever the base record is written, so that it names every attribute
//! as it is; it stays in the base record (resident). A file whose other
//! attributes are in extension records too (as Windows moves names out
//! of a record of many hard links) is read, not written.

use core::cmp::Ordering;

use radix_vfs::mem::{self, KVec};
use radix_vfs::{Error, Result};

use crate::edit::{self, put_u16, put_u32, put_u64};
use crate::record::{kind, u16_at, u32_at, u64_at};
use crate::upcase::Upcase;

/// A piece of a file's data in an extension record.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Extent {
    /// The extension record's file reference.
    pub reference: u64,
    /// The piece's first VCN.
    pub lowest: u64,
    /// Its attribute's number in that record.
    pub id: u16,
}

/// The bytes of an entry before its name.
const ENTRY: usize = 0x1a;

/// An attribute an entry is for.
struct Entry<'a> {
    kind: u32,
    name: &'a [u8],
    lowest: u64,
    reference: u64,
    id: u16,
}

impl Entry<'_> {
    fn len(&self) -> usize {
        (ENTRY + self.name.len()).next_multiple_of(8)
    }

    fn order(&self, other: &Entry<'_>, upcase: &Upcase) -> Ordering {
        self.kind
            .cmp(&other.kind)
            .then_with(|| edit::collate(upcase, self.name, other.name))
            .then(self.lowest.cmp(&other.lowest))
    }
}

/// The value of the attribute list of the file whose base record is
/// `buf`, its reference `base`, with its data's pieces `extents` in
/// extension records: an entry for each attribute of `buf` but the list,
/// and for each piece, names ordered through the volume's up-case table
/// `upcase` ([`edit::collate`]).
pub fn build(buf: &[u8], base: u64, extents: &[Extent], upcase: &Upcase) -> Result<KVec<u8>> {
    let mut entries: KVec<Entry<'_>> = mem::vec();
    for at in edit::attrs(buf) {
        let kind = u32_at(buf, at);
        if kind == kind::ATTRIBUTE_LIST {
            continue;
        }
        let lowest = if buf[at + 8] != 0 { u64_at(buf, at + 0x10) } else { 0 };
        let entry = Entry {
            kind,
            name: edit::name_at(buf, at),
            lowest,
            reference: base,
            id: u16_at(buf, at + 0x0e),
        };
        mem::push(&mut entries, entry)?;
    }
    for e in extents {
        let entry = Entry {
            kind: kind::DATA,
            name: &[],
            lowest: e.lowest,
            reference: e.reference,
            id: e.id,
        };
        mem::push(&mut entries, entry)?;
    }
    // Sorted in place, those alike (a file's names) kept as their record
    // has them.
    for i in 1..entries.len() {
        let mut j = i;
        while j > 0 && entries[j - 1].order(&entries[j], upcase) == Ordering::Greater {
            entries.swap(j - 1, j);
            j -= 1;
        }
    }
    let mut value = crate::fs::zeroed(entries.iter().map(Entry::len).sum())?;
    let mut at = 0;
    for e in &entries {
        let len = e.len();
        let out = &mut value[at..at + len];
        put_u32(out, 0, e.kind);
        put_u16(out, 4, len as u16);
        out[6] = (e.name.len() / 2) as u8;
        out[7] = ENTRY as u8;
        put_u64(out, 8, e.lowest);
        put_u64(out, 0x10, e.reference);
        put_u16(out, 0x18, e.id);
        out[ENTRY..ENTRY + e.name.len()].copy_from_slice(e.name);
        at += len;
    }
    Ok(value)
}

/// Puts the attribute list `value` in the base record `buf`, in its list
/// or a new one. Fails with [`Error::NoSpace`] if the record has no room,
/// leaving it as it was.
pub fn put(buf: &mut [u8], value: &[u8]) -> Result<()> {
    match edit::find(buf, kind::ATTRIBUTE_LIST, &[]) {
        Some(at) => {
            if buf[at + 8] != 0 {
                return Err(Error::NotSupported);
            }
            edit::set_value(buf, at, value)
        }
        None => {
            let attr = edit::resident(kind::ATTRIBUTE_LIST, &[], u16_at(buf, 0x28), value, false)?;
            edit::insert(buf, &attr)?;
            edit::next_instance(buf);
            Ok(())
        }
    }
}

/// Takes the attribute list out of the record `buf`, if it has one.
pub fn remove(buf: &mut [u8]) {
    if let Some(at) = edit::find(buf, kind::ATTRIBUTE_LIST, &[]) {
        edit::remove(buf, at);
    }
}

/// Takes the pieces of the file's data, its unnamed `$DATA`, out of the
/// record `buf`.
pub fn remove_data(buf: &mut [u8]) {
    while let Some(at) = edit::find(buf, kind::DATA, &[]) {
        edit::remove(buf, at);
    }
}
