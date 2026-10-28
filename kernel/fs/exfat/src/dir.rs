// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Directories: reading their sets of entries, finding names, and finding
//! room for new sets.
//!
//! A directory's entries lie in its clusters, in order; a set may cross
//! from one cluster to the next, which for a directory in pieces is
//! elsewhere on the disk, so where each entry is comes from the
//! directory's chain. A set counts only if it is whole: its entries all
//! in use, a stream extension second, as many name entries as its name
//! needs, and a checksum that adds up; anything else is skipped, as
//! Linux's driver skips it. Names are looked for by their hash first,
//! then compared unit by unit through the up-case table.
//!
//! Every function here is called with the volume's `ns` held.

use radix_block::cache::BlockCache;
use radix_fat::chain::Chain;
use radix_fat::name::NameBuf;
use radix_vfs::{Error, Result};

use crate::boot::Geometry;
use crate::entry::{
    ENTRY, File, IN_USE, NAME_MAX, PER_NAME, SECONDARY, Stream, add_to_checksum, add_to_hash, kind,
    name_units,
};
use crate::node::{ExfatNode, Meta};
use crate::upcase::Upcase;

/// The most bytes a directory may hold, by the specification.
const MAX_BYTES: u64 = 256 * 1024 * 1024;

/// The most pieces a set's entries are in: one per cluster it crosses.
/// The longest set, 256 entries, crosses at most 17 clusters of 512
/// bytes.
pub(crate) const MAX_RUNS: usize = 18;

/// Where a set's entries are: runs of entries next to each other on the
/// device, as (offset, entries).
pub(crate) type Runs = [(u64, u16)];

/// A set found in a directory.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Found {
    /// The index of its file entry.
    pub(crate) index: u32,
    /// How many entries it has.
    pub(crate) count: u32,
    pub(crate) file: File,
    pub(crate) stream: Stream,
}

/// A set a scan meets.
pub(crate) struct Visit<'a> {
    pub(crate) found: Found,
    /// Its name.
    pub(crate) units: &'a [u16],
}

/// What a scan puts sets together in: kilobytes, so it lives in the
/// volume's `ns` lock, which every scan holds, rather than on the stack.
pub(crate) struct Names {
    units: [u16; NAME_MAX],
    /// A name as UTF-8, for listings.
    text: NameBuf,
    /// Where the entries of the set last met are.
    runs: [(u64, u16); MAX_RUNS],
    runs_len: usize,
}

impl Default for Names {
    fn default() -> Self {
        Names::new()
    }
}

impl Names {
    pub(crate) const fn new() -> Names {
        Names { units: [0; NAME_MAX], text: NameBuf::new(), runs: [(0, 0); MAX_RUNS], runs_len: 0 }
    }

    /// Where the entries of the set a scan stopped at are.
    pub(crate) fn runs(&self) -> &Runs {
        &self.runs[..self.runs_len]
    }
}

/// Adds the entry at `offset` to `runs`; false if there is no room.
fn add_run(runs: &mut [(u64, u16); MAX_RUNS], len: &mut usize, offset: u64) -> bool {
    if let Some(last) = len.checked_sub(1).map(|i| &mut runs[i])
        && last.0 + u64::from(last.1) * ENTRY as u64 == offset
    {
        last.1 += 1;
        return true;
    }
    if *len == MAX_RUNS {
        return false;
    }
    runs[*len] = (offset, 1);
    *len += 1;
    true
}

/// Calls `f` with each entry of the directory whose clusters are `chain`
/// from index `from` on (its index, offset and bytes) until it returns
/// false, reading a sector at a time; returns how many entries the
/// directory has room for.
pub(crate) fn walk(
    cache: &BlockCache,
    geo: &Geometry,
    chain: &Chain,
    from: u32,
    mut f: impl FnMut(u32, u64, &[u8; ENTRY]) -> bool,
) -> Result<u32> {
    const PER_SECTOR: u32 = 512 / ENTRY as u32;
    let cs = u64::from(geo.cluster_size);
    let capacity = (u64::from(chain.len()) * cs).min(MAX_BYTES) / ENTRY as u64;
    let capacity = capacity as u32;
    let mut buf = [0u8; 512];
    let mut index = from;
    while index < capacity {
        // A sector's entries are next to each other on the disk: clusters
        // are whole sectors.
        let byte = u64::from(index) * ENTRY as u64;
        let cluster = chain.get((byte / cs) as u32).ok_or(Error::Corrupt)?;
        let offset = geo.cluster_offset(cluster) + byte % cs;
        let count = (PER_SECTOR - index % PER_SECTOR).min(capacity - index);
        let len = count as usize * ENTRY;
        cache.read(offset, &mut buf[..len])?;
        for (k, bytes) in buf[..len].as_chunks::<ENTRY>().0.iter().enumerate() {
            if !f(index + k as u32, offset + (k * ENTRY) as u64, bytes) {
                return Ok(capacity);
            }
        }
        index += count;
    }
    Ok(capacity)
}

/// A set being read.
struct Partial {
    found: Found,
    /// Entries read, the file entry's among them.
    seen: u32,
    sum: u16,
    /// Units of the name read.
    named: usize,
    /// Whether it holds an entry Radix must understand and does not.
    unknown: bool,
}

/// The length and hash of `name` in upper case, as its stream extension
/// would have them; `None` if it is longer than any exFAT name.
pub(crate) fn hash_of(upcase: &Upcase, name: &str) -> Option<(usize, u16)> {
    let mut len = 0;
    let mut hash = 0u16;
    for unit in name.encode_utf16() {
        len += 1;
        if len > NAME_MAX {
            return None;
        }
        hash = add_to_hash(hash, upcase.upper(unit));
    }
    Some((len, hash))
}

impl ExfatNode {
    /// How many entries the directory has room for.
    fn capacity(&self, meta: &mut Meta) -> Result<u32> {
        let cs = u64::from(self.fs.geo.cluster_size);
        let chain = self.chain(meta)?;
        Ok(((u64::from(chain.len()) * cs).min(MAX_BYTES) / ENTRY as u64) as u32)
    }

    /// Where entry `index` is on the device.
    pub(crate) fn entry_offset(&self, meta: &mut Meta, index: u32) -> Result<u64> {
        let geo = &self.fs.geo;
        let at = u64::from(index) * ENTRY as u64;
        let cs = u64::from(geo.cluster_size);
        let cluster = self.chain(meta)?.get((at / cs) as u32).ok_or(Error::Corrupt)?;
        Ok(geo.cluster_offset(cluster) + at % cs)
    }

    /// Calls `f` with each entry from index `from` on ([`walk`]).
    fn raw(&self, from: u32, f: impl FnMut(u32, u64, &[u8; ENTRY]) -> bool) -> Result<u32> {
        let mut meta = self.meta.lock();
        let chain = self.chain(&mut meta)?;
        walk(&self.fs.cache, &self.fs.geo, chain, from, f)
    }

    /// Calls `f` with each whole set from entry `from` on, until it
    /// returns false; `f` also gets room for the name as text. When `f`
    /// stops the scan, [`Names::runs`] says where the set's entries are.
    pub(crate) fn scan(
        &self,
        names: &mut Names,
        from: u32,
        mut f: impl FnMut(&Visit<'_>, &mut NameBuf) -> bool,
    ) -> Result<()> {
        let Names { units, text, runs, runs_len } = names;
        let mut set: Option<Partial> = None;
        let mut skip = 0u32;
        self.raw(from, |index, offset, bytes| {
            let t = bytes[0];
            if t == kind::END {
                return false;
            }
            if t & IN_USE == 0 {
                set = None;
                skip = 0;
                return true;
            }
            if t & SECONDARY == 0 {
                set = None;
                skip = 0;
                if t == kind::FILE {
                    let file = File::decode(bytes);
                    if file.secondaries >= 2 {
                        *runs_len = 0;
                        add_run(runs, runs_len, offset);
                        set = Some(Partial {
                            found: Found {
                                index,
                                count: 1 + u32::from(file.secondaries),
                                file,
                                stream: Stream::default(),
                            },
                            seen: 1,
                            sum: add_to_checksum(0, 0, bytes),
                            named: 0,
                            unknown: false,
                        });
                    } else {
                        skip = u32::from(file.secondaries);
                    }
                } else if !matches!(t, kind::BITMAP | kind::UPCASE | kind::LABEL) {
                    // Another primary entry: its secondaries are its own.
                    skip = u32::from(bytes[1]);
                }
                return true;
            }
            if skip > 0 {
                skip -= 1;
                return true;
            }
            let Some(partial) = set.as_mut() else { return true };
            if !add_run(runs, runs_len, offset) {
                set = None;
                return true;
            }
            let n = partial.seen;
            partial.sum = add_to_checksum(partial.sum, n as usize, bytes);
            partial.seen += 1;
            let name_len = usize::from(partial.found.stream.name_len);
            if n == 1 {
                let stream = Stream::decode(bytes);
                let needed = usize::from(stream.name_len).div_ceil(PER_NAME) as u32;
                if t != kind::STREAM || stream.name_len == 0 || needed + 2 > partial.found.count {
                    set = None;
                    return true;
                }
                partial.found.stream = stream;
            } else if t == kind::NAME && partial.named < name_len {
                let take = (name_len - partial.named).min(PER_NAME);
                let piece = name_units(bytes);
                units[partial.named..partial.named + take].copy_from_slice(&piece[..take]);
                partial.named += take;
            } else if t & 0x20 == 0 {
                // A critical secondary entry: one Radix does not know, or a
                // name entry too many.
                partial.unknown = true;
            }
            if partial.seen < partial.found.count {
                return true;
            }
            let Some(done) = set.take() else { return true };
            if done.sum != done.found.file.checksum || done.named != name_len || done.unknown {
                return true;
            }
            let visit = Visit { found: done.found, units: &units[..name_len] };
            f(&visit, text)
        })?;
        Ok(())
    }

    /// The set called `name`, but for case, other than those at the
    /// indices in `skip`. [`Names::runs`] then says where its entries
    /// are.
    pub(crate) fn find(&self, names: &mut Names, name: &str, skip: &[u32]) -> Result<Option<Found>> {
        let upcase = &self.fs.upcase;
        let Some((len, hash)) = hash_of(upcase, name) else { return Ok(None) };
        let mut found = None;
        self.scan(names, 0, |visit, _| {
            let stream = &visit.found.stream;
            if usize::from(stream.name_len) == len
                && stream.hash == hash
                && !skip.contains(&visit.found.index)
                && same(upcase, visit.units, name)
            {
                found = Some(visit.found);
            }
            found.is_none()
        })?;
        Ok(found)
    }

    /// Whether the directory holds no entry in use.
    pub(crate) fn is_empty_dir(&self) -> Result<bool> {
        let mut empty = true;
        self.raw(0, |_, _, bytes| match bytes[0] {
            kind::END => false,
            t if t & IN_USE != 0 => {
                empty = false;
                false
            }
            _ => true,
        })?;
        Ok(empty)
    }

    /// Room for `count` entries in a row: the index of the first, and of
    /// the end mark, if the room reaches it.
    fn room(&self, count: u32) -> Result<(u32, Option<u32>)> {
        let (mut start, mut run, mut end, mut found) = (0, 0, None, None);
        let capacity = self.raw(0, |index, _, bytes| {
            if bytes[0] == kind::END {
                end = Some(index);
                if run == 0 {
                    start = index;
                }
                return false;
            }
            if bytes[0] & IN_USE != 0 {
                run = 0;
                return true;
            }
            if run == 0 {
                start = index;
            }
            run += 1;
            if run == count {
                found = Some(start);
            }
            found.is_none()
        })?;
        if let Some(start) = found {
            return Ok((start, None));
        }
        // Everything from the end mark on is free, and so are the unused
        // entries right before it.
        let tail = end.unwrap_or(capacity);
        Ok((if run > 0 || end.is_some() { start } else { tail }, end))
    }

    /// Makes the directory big enough for `needed` entries.
    fn grow(&self, meta: &mut Meta, needed: u32) -> Result<()> {
        if needed <= self.capacity(meta)? {
            return Ok(());
        }
        let cs = self.fs.geo.cluster_size;
        let bytes = u64::from(needed) * ENTRY as u64;
        if bytes > MAX_BYTES {
            return Err(Error::NoSpace);
        }
        let clusters = bytes.div_ceil(u64::from(cs)) as u32;
        self.chain(meta)?;
        let Meta { chain, first, contiguous, .. } = meta;
        let Some(chain) = chain.as_mut() else { return Err(Error::Corrupt) };
        self.fs.grow(chain, first, contiguous, clusters, true)?;
        meta.size = u64::from(clusters) * u64::from(cs);
        meta.dirty = true;
        Ok(())
    }

    /// Writes a set of `count` entries, entry `k` being `entry(k)`, into
    /// the directory, growing it if need be; returns the index of its
    /// first entry, and leaves where its entries went in
    /// [`Names::runs`].
    pub(crate) fn put(
        &self,
        names: &mut Names,
        count: u32,
        entry: impl Fn(usize) -> [u8; ENTRY],
    ) -> Result<u32> {
        let (start, end) = self.room(count)?;
        self.fs.changing()?;
        let mut meta = self.meta.lock();
        self.grow(&mut meta, start + count)?;
        names.runs_len = 0;
        for k in 0..count {
            let offset = self.entry_offset(&mut meta, start + k)?;
            self.fs.cache.write(offset, &entry(k as usize))?;
            if !add_run(&mut names.runs, &mut names.runs_len, offset) {
                return Err(Error::Corrupt);
            }
        }
        // Past the end mark nothing should be in use, but a careless
        // writer may have left bytes there: the entry after the set is the
        // mark now.
        let after = start + count;
        if end.is_some_and(|end| after > end) && after < self.capacity(&mut meta)? {
            let mark = self.entry_offset(&mut meta, after)?;
            self.fs.cache.write(mark, &[kind::END])?;
        }
        Ok(start)
    }

    /// Marks the entries of `found` unused.
    pub(crate) fn delete(&self, found: &Found) -> Result<()> {
        self.fs.changing()?;
        let mut meta = self.meta.lock();
        for index in found.index..found.index + found.count {
            let offset = self.entry_offset(&mut meta, index)?;
            let mut t = [0u8];
            self.fs.cache.read(offset, &mut t)?;
            self.fs.cache.write(offset, &[t[0] & !IN_USE])?;
        }
        Ok(())
    }
}

/// Whether the name `units` is `name`, but for case.
pub(crate) fn same(upcase: &Upcase, units: &[u16], name: &str) -> bool {
    let mut theirs = units.iter();
    for unit in name.encode_utf16() {
        match theirs.next() {
            Some(&other) if other == unit || upcase.upper(other) == upcase.upper(unit) => {}
            _ => return false,
        }
    }
    theirs.next().is_none()
}
