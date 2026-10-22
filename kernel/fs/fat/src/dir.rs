// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Directories: reading their entries, putting long names together, and
//! finding room for new entries.
//!
//! A directory is an array of 32-byte entries, in a cluster chain (or, for
//! the root of FAT12 and FAT16, a fixed region before the data). A file is
//! a run of long entries holding its name in UTF-16, last part first, then
//! its short entry; the long entries carry the short name's checksum, and
//! a run that does not add up is ignored, as every driver does. An entry
//! whose first byte is 0xe5 was removed and its slot is free; one whose
//! first byte is 0 was never used, and neither was anything after it.
//!
//! Every function here is called with the volume's `ns` held.

use radix_vfs::{Error, Result};

use crate::fs::{Inner, is_label};
use crate::name::{
    DELETED, ENTRY, Long, MAX_LONG, NameBuf, PER_LONG, Short, ShortBuf, attr, checksum, same,
};
use crate::node::{FatNode, Meta};
use crate::table::Width;

/// The most entries a directory may have, by the specification: 2 MiB of
/// them.
const MAX_ENTRIES: u32 = 65536;

/// An entry found in a directory.
pub(crate) struct Found {
    /// The index of its first entry: its first long entry, or the short
    /// entry when it has none.
    pub(crate) first: u32,
    /// The index of its short entry.
    pub(crate) index: u32,
    /// The short entry's offset on the device.
    pub(crate) offset: u64,
    /// The short entry.
    pub(crate) short: Short,
}

/// A file or directory a scan meets.
pub(crate) struct Visit<'a> {
    pub(crate) first: u32,
    pub(crate) index: u32,
    pub(crate) offset: u64,
    pub(crate) short: Short,
    /// Its long name, or its short name as shown when it has none.
    pub(crate) name: &'a str,
    /// Whether it has a long name.
    pub(crate) long: bool,
}

impl Visit<'_> {
    fn found(&self) -> Found {
        Found { first: self.first, index: self.index, offset: self.offset, short: self.short }
    }

    /// Whether it is called `name`, by its long or its short name.
    fn is(&self, name: &str, alias: &mut ShortBuf) -> bool {
        if same(self.name, name) {
            return true;
        }
        if !self.long {
            return false;
        }
        self.short.display(alias);
        same(alias.as_str(), name)
    }
}

/// What a scan puts names together in: kilobytes, so it lives in the
/// volume's `ns` lock, which every scan holds, rather than on the stack.
pub(crate) struct Names {
    long: LongName,
    name: NameBuf,
}

impl Default for Names {
    fn default() -> Self {
        Names::new()
    }
}

impl Names {
    pub(crate) const fn new() -> Names {
        Names { long: LongName::new(), name: NameBuf::new() }
    }
}

/// A long name being put together from its entries.
struct LongName {
    units: [u16; MAX_LONG * PER_LONG],
    /// How many entries it has.
    count: u8,
    /// The order of the entry that must come next; 0 once all have.
    expect: u8,
    sum: u8,
    /// The index of its first entry.
    first: u32,
    active: bool,
}

impl LongName {
    const fn new() -> LongName {
        LongName {
            units: [0; MAX_LONG * PER_LONG],
            count: 0,
            expect: 0,
            sum: 0,
            first: 0,
            active: false,
        }
    }

    fn add(&mut self, index: u32, entry: &Long) {
        if entry.last {
            self.active = true;
            self.count = entry.order;
            self.expect = entry.order;
            self.sum = entry.checksum;
            self.first = index;
        }
        if !self.active || entry.order != self.expect || entry.checksum != self.sum {
            self.active = false;
            return;
        }
        let Some(part) = part(entry.order) else {
            self.active = false;
            return;
        };
        self.units[part].copy_from_slice(&entry.units);
        // `order` is at least 1 (`part` says so) and equals `expect`.
        self.expect -= 1;
    }

    /// The name and its first entry's index, if every part came, in
    /// order, for the short name whose checksum is `sum`.
    fn complete(&self, sum: u8) -> Option<(u32, &[u16])> {
        let done = self.active && self.expect == 0 && self.sum == sum;
        done.then(|| (self.first, &self.units[..usize::from(self.count) * PER_LONG]))
    }
}

/// Where the part numbered `order` (from 1) of a long name goes among its
/// units; `None` for an order no name has.
fn part(order: u8) -> Option<core::ops::Range<usize>> {
    let at = usize::from(order.checked_sub(1)?) * PER_LONG;
    (at + PER_LONG <= MAX_LONG * PER_LONG).then_some(at..at + PER_LONG)
}

impl FatNode {
    /// Whether it is the fixed root directory of FAT12 or FAT16.
    fn fixed_root(&self) -> bool {
        self.root && self.fs.geo.width != Width::Fat32
    }

    /// How many entries the directory has room for.
    fn capacity(&self, meta: &mut Meta) -> Result<u32> {
        if self.fixed_root() {
            return Ok(self.fs.geo.root_entries);
        }
        let per_cluster = self.fs.geo.cluster_size / ENTRY as u32;
        let chain = self.chain(meta)?;
        Ok(chain.len().saturating_mul(per_cluster).min(MAX_ENTRIES))
    }

    /// Where entry `index` is on the device.
    fn entry_offset(&self, meta: &mut Meta, index: u32) -> Result<u64> {
        let geo = &self.fs.geo;
        let at = u64::from(index) * ENTRY as u64;
        if self.fixed_root() {
            return Ok(geo.root_start + at);
        }
        let size = u64::from(geo.cluster_size);
        let cluster = self.chain(meta)?.get((at / size) as u32).ok_or(Error::Corrupt)?;
        Ok(geo.cluster_offset(cluster) + at % size)
    }

    /// Calls `f` with each entry from index `from` on (its index, offset
    /// and bytes) until it returns false, reading a sector at a time;
    /// returns how many entries the directory has room for.
    fn raw(&self, from: u32, mut f: impl FnMut(u32, u64, &[u8; ENTRY]) -> bool) -> Result<u32> {
        const PER_SECTOR: u32 = 512 / ENTRY as u32;
        let mut meta = self.meta.lock();
        let capacity = self.capacity(&mut meta)?;
        let mut buf = [0u8; 512];
        let mut index = from;
        while index < capacity {
            // A sector's entries are next to each other on the disk:
            // clusters and the fixed root are whole sectors.
            let offset = self.entry_offset(&mut meta, index)?;
            let count = (PER_SECTOR - index % PER_SECTOR).min(capacity - index);
            let len = count as usize * ENTRY;
            self.fs.cache.read(offset, &mut buf[..len])?;
            for (k, bytes) in buf[..len].as_chunks::<ENTRY>().0.iter().enumerate() {
                if !f(index + k as u32, offset + (k * ENTRY) as u64, bytes) {
                    return Ok(capacity);
                }
            }
            index += count;
        }
        Ok(capacity)
    }

    /// Calls `f` with each file and directory from entry `from` on, with
    /// its name, until it returns false. `.`, `..` and the volume label
    /// are not shown.
    pub(crate) fn scan(
        &self,
        names: &mut Names,
        from: u32,
        mut f: impl FnMut(&Visit<'_>) -> bool,
    ) -> Result<()> {
        let Names { long, name } = names;
        long.active = false;
        self.raw(from, |index, offset, bytes| {
            match bytes[0] {
                0 => return false,
                DELETED => {
                    long.active = false;
                    return true;
                }
                _ => {}
            }
            if bytes[11] & 0x3f == attr::LONG_NAME {
                match Long::decode(bytes) {
                    Some(entry) => long.add(index, &entry),
                    None => long.active = false,
                }
                return true;
            }
            let short = Short::decode(bytes);
            if is_label(&short) || short.is_dot() {
                long.active = false;
                return true;
            }
            let (first, has_long) = match long.complete(checksum(&short.name)) {
                Some((first, units)) => {
                    name.set_units(units);
                    (first, true)
                }
                None => {
                    short.display(name);
                    (index, false)
                }
            };
            long.active = false;
            f(&Visit { first, index, offset, short, name: name.as_str(), long: has_long })
        })?;
        Ok(())
    }

    /// The entry called `name`, but for case, by its long or its short
    /// name, other than those at the indices in `skip`.
    pub(crate) fn find(
        &self,
        names: &mut Names,
        name: &str,
        skip: &[u32],
    ) -> Result<Option<Found>> {
        let mut found = None;
        let mut alias = ShortBuf::new();
        self.scan(names, 0, |visit| {
            if !skip.contains(&visit.index) && visit.is(name, &mut alias) {
                found = Some(visit.found());
            }
            found.is_none()
        })?;
        Ok(found)
    }

    /// Whether an entry is called `name` (but for those in `skip`), and
    /// which of the short names in `candidates` entries have, as bits.
    pub(crate) fn survey(
        &self,
        names: &mut Names,
        name: &str,
        candidates: &[[u8; 11]],
        skip: &[u32],
    ) -> Result<(bool, u64)> {
        let mut exists = false;
        let mut used = 0u64;
        let mut alias = ShortBuf::new();
        self.scan(names, 0, |visit| {
            if skip.contains(&visit.index) {
                return true;
            }
            for (i, candidate) in candidates.iter().enumerate() {
                if visit.short.name == *candidate {
                    used |= 1 << i;
                }
            }
            exists = visit.is(name, &mut alias);
            !exists
        })?;
        Ok((exists, used))
    }

    /// Whether the directory holds nothing but `.` and `..`.
    pub(crate) fn is_empty_dir(&self, names: &mut Names) -> Result<bool> {
        let mut empty = true;
        self.scan(names, 0, |_| {
            empty = false;
            false
        })?;
        Ok(empty)
    }

    /// The volume label entry's name, in the root directory.
    pub(crate) fn volume_label(&self) -> Result<Option<[u8; 11]>> {
        let mut label = None;
        self.raw(0, |_, _, bytes| match bytes[0] {
            0 => false,
            DELETED => true,
            _ => {
                let short = Short::decode(bytes);
                if is_label(&short) {
                    label = Some(short.name);
                }
                label.is_none()
            }
        })?;
        Ok(label)
    }

    /// Room for `count` entries in a row: the index of the first, and of
    /// the end mark, if the room reaches it.
    fn room(&self, count: u32) -> Result<(u32, Option<u32>)> {
        let (mut start, mut run, mut end, mut found) = (0, 0, None, None);
        let capacity = self.raw(0, |index, _, bytes| match bytes[0] {
            0 => {
                end = Some(index);
                false
            }
            DELETED => {
                if run == 0 {
                    start = index;
                }
                run += 1;
                if run == count {
                    found = Some(start);
                }
                found.is_none()
            }
            _ => {
                run = 0;
                true
            }
        })?;
        if let Some(start) = found {
            return Ok((start, None));
        }
        // Everything from the end mark on is free, and so are removed
        // entries right before it.
        let tail = end.unwrap_or(capacity);
        Ok((if run > 0 { start } else { tail }, end))
    }

    /// Makes the directory big enough for `needed` entries.
    fn grow(&self, meta: &mut Meta, needed: u32) -> Result<()> {
        if needed <= self.capacity(meta)? {
            return Ok(());
        }
        if self.fixed_root() || needed > MAX_ENTRIES {
            return Err(Error::NoSpace);
        }
        let clusters = needed.div_ceil(self.fs.geo.cluster_size / ENTRY as u32);
        self.chain(meta)?;
        let Meta { chain, first, .. } = meta;
        let Some(chain) = chain.as_mut() else { return Err(Error::Corrupt) };
        let had = chain.len();
        while chain.len() < clusters {
            let grown = match chain.reserve() {
                Ok(()) => self.fs.extend(chain.last()),
                Err(err) => Err(err.into()),
            };
            let zeroed = grown.and_then(|cluster| {
                // Room was made for it.
                let _ = chain.push(cluster);
                self.fs.zero_cluster(cluster)
            });
            if let Err(err) = zeroed {
                let _ = self.fs.cut(chain, first, had, true);
                return Err(err);
            }
        }
        Ok(())
    }

    /// Writes `entries` in a row into the directory, growing it if need
    /// be; returns the index and offset of the last.
    pub(crate) fn put(&self, entries: &[[u8; ENTRY]]) -> Result<(u32, u64)> {
        let count = entries.len() as u32;
        let (start, end) = self.room(count)?;
        let mut meta = self.meta.lock();
        self.grow(&mut meta, start + count)?;
        let mut offset = 0;
        for (k, entry) in entries.iter().enumerate() {
            offset = self.entry_offset(&mut meta, start + k as u32)?;
            self.fs.cache.write(offset, entry)?;
        }
        // What lies past the end mark may hold anything; the entry after
        // the new ones is the mark now.
        let after = start + count;
        if end.is_some_and(|end| after > end) && after < self.capacity(&mut meta)? {
            let mark = self.entry_offset(&mut meta, after)?;
            self.fs.cache.write(mark, &[0])?;
        }
        Ok((after - 1, offset))
    }

    /// Marks the entries of `found` removed.
    pub(crate) fn delete(&self, found: &Found) -> Result<()> {
        let mut meta = self.meta.lock();
        for index in found.first..=found.index {
            let offset = self.entry_offset(&mut meta, index)?;
            self.fs.cache.write(offset, &[DELETED])?;
        }
        Ok(())
    }
}

impl Inner {
    /// The first cluster of the parent of the directory that starts at
    /// `cluster`, from its `..` entry: 0 for the root.
    pub(crate) fn parent_of(&self, cluster: u32) -> Result<u32> {
        if !self.geo.valid(cluster) {
            return Err(Error::Corrupt);
        }
        let mut bytes = [0u8; ENTRY];
        self.cache.read(self.geo.cluster_offset(cluster) + ENTRY as u64, &mut bytes)?;
        let short = Short::decode(&bytes);
        if &short.name != b"..         " {
            return Err(Error::Corrupt);
        }
        Ok(short.cluster)
    }
}

#[cfg(kani)]
mod proofs {
    use super::*;

    /// Every order a long entry can hold places its part of the name
    /// inside the name's units, or is refused: assembling a name never
    /// indexes out of bounds, and the order it expects next never wraps.
    #[kani::proof]
    fn long_name_parts_fit() {
        let order: u8 = kani::any();
        match part(order) {
            Some(range) => {
                assert!((1..=MAX_LONG as u8).contains(&order));
                assert_eq!(range.len(), PER_LONG);
                assert!(range.end <= MAX_LONG * PER_LONG);
            }
            None => assert!(order == 0 || usize::from(order) > MAX_LONG),
        }
    }
}
