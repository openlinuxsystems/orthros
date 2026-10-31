// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Finding a UDF volume, and where its partitions' blocks are.
//!
//! A UDF disc says what it is in its volume recognition sequence at 32
//! KiB (`BEA01`, `NSR02` or `NSR03`, `TEA01`). The anchor volume
//! descriptor pointer, at block 256 (or 256 blocks from the end, or the
//! last block), gives the volume descriptor sequence, which holds the
//! partitions and the logical volume. The logical volume's partition
//! maps say how each of its partitions' blocks are found:
//!
//! - physical: they lie in a row on the device from the partition's
//!   start;
//! - sparable (rewritable discs): as physical, but for packets moved
//!   elsewhere when they wore out, which its sparing table lists;
//! - virtual (CD-R and other write-once discs): each block is where the
//!   virtual allocation table, the disc's last file, says;
//! - metadata (UDF 2.50 and later): its blocks are those of the
//!   metadata file, a file of the physical partition.
//!
//! The file set descriptor, in the logical volume, names the root
//! directory.

use radix_block::cache::BlockCache;
use radix_vfs::mem::{self, KBox, KVec};
use radix_vfs::{Error, Result};

use crate::format::{self, Extent, LongAd, NameBuf, ident, le16, le32, tag};
use crate::node::{Entry, Run};

/// Where the volume recognition sequence starts.
pub const RECOGNITION: u64 = 32 * 1024;
/// The most descriptors of a volume descriptor sequence read.
const MAX_DESCRIPTORS: u64 = 64;
/// The most volume descriptor pointers followed.
const MAX_POINTERS: u32 = 8;
/// The most partitions and partition maps.
const MAX_PARTITIONS: usize = 8;
/// The largest sparing table read, in bytes.
const MAX_SPARING: u32 = 64 * 1024;
/// The largest virtual allocation table read, in bytes: enough for a
/// disc of 8 GiB of 2048-byte blocks.
const MAX_VAT: u64 = 16 << 20;
/// File types of the files a partition map needs (UDF 2.60 2.3.5.2).
const METADATA_FILE: u8 = 250;
const VAT_FILE: u8 = 248;

/// How the blocks of one of the logical volume's partitions are found.
pub(crate) enum Map {
    /// In a row from the device's byte `start`.
    Physical { start: u64, blocks: u32 },
    /// In a row, but for packets of `packet` blocks the sparing table
    /// moved: (the packet's first block, where it is now on the device,
    /// in blocks), sorted.
    Sparable { start: u64, blocks: u32, packet: u32, spared: KVec<(u32, u32)> },
    /// Each block where the virtual allocation table says, in the
    /// physical partition from byte `start`.
    Virtual { start: u64, table: KVec<u32> },
    /// The metadata file's blocks: its runs on the device.
    Metadata { runs: KVec<Run> },
    /// Not found yet: a virtual or metadata partition, read once the
    /// others are known. `physical` is the map of the partition it is in;
    /// `file` the metadata file's block in it (and its mirror's), or 0
    /// for a virtual one.
    Pending { physical: u16, file: u32, mirror: u32 },
}

/// A UDF volume: its logical blocks, its partitions and its root.
pub struct Volume {
    /// The size of its logical blocks, in bytes.
    pub block: u64,
    pub(crate) maps: KVec<Map>,
    /// The root directory's ICB.
    pub root: LongAd,
    /// The logical volume's identifier, which is its label.
    pub label: NameBuf,
    /// How many blocks its partitions have.
    pub blocks: u64,
}

/// A partition descriptor: its number, its first block and its length.
#[derive(Clone, Copy, Default)]
struct Partition {
    number: u16,
    start: u32,
    blocks: u32,
}

/// What the volume descriptor sequence holds.
struct Sequence {
    partitions: [Partition; MAX_PARTITIONS],
    count: usize,
    logical: Option<KVec<u8>>,
}

/// `len` zero bytes on the heap.
pub(crate) fn zeroed(len: usize) -> Result<KVec<u8>> {
    let mut bytes = mem::vec_with_capacity(len)?;
    while bytes.len() < len {
        // Room was made above.
        let _ = bytes.push_within_capacity(0);
    }
    Ok(bytes)
}

impl Volume {
    /// Finds the volume on the device `cache` reads. Fails with
    /// [`Error::UnknownFilesystem`] if the device holds none, and with
    /// [`Error::NotSupported`] for one Radix cannot read.
    pub fn find(cache: &BlockCache) -> Result<KBox<Volume>> {
        let info = radix_block::BlockDevice::info(&**cache.device());
        let size = info.sectors * radix_block::SECTOR;
        if !recognized(cache, size)? {
            return Err(Error::UnknownFilesystem);
        }
        let (block, main, reserve) =
            anchor(cache, size, u64::from(info.logical_block))?.ok_or(Error::UnknownFilesystem)?;
        let mut sequence = Sequence { partitions: Default::default(), count: 0, logical: None };
        read_sequence(cache, block, main, &mut sequence)?;
        if sequence.logical.is_none() || sequence.count == 0 {
            read_sequence(cache, block, reserve, &mut sequence)?;
        }
        let logical = sequence.logical.take().ok_or(Error::Corrupt)?;
        if u64::from(le32(&logical, 212)) != block {
            return Err(Error::Corrupt);
        }
        let mut volume = mem::boxed(Volume {
            block,
            maps: mem::vec(),
            root: LongAd::default(),
            label: NameBuf::default(),
            blocks: 0,
        })?;
        format::dstring(&logical[84..212], &mut volume.label);
        volume.label.trim_end();
        volume.maps(cache, &logical, &sequence, size)?;
        volume.root = file_set(cache, &volume, format::long_ad(&logical, 248))?;
        Ok(volume)
    }

    /// Reads the logical volume descriptor's partition maps.
    #[inline(never)]
    fn maps(
        &mut self,
        cache: &BlockCache,
        logical: &[u8],
        sequence: &Sequence,
        size: u64,
    ) -> Result<()> {
        let count = le32(logical, 268) as usize;
        let end = 440usize.saturating_add(le32(logical, 264) as usize).min(logical.len());
        if count == 0 || count > MAX_PARTITIONS {
            return Err(Error::Corrupt);
        }
        let partition = |number: u16| {
            sequence.partitions[..sequence.count].iter().find(|p| p.number == number).copied()
        };
        let mut at = 440;
        for _ in 0..count {
            let (kind, len) = (logical.get(at).copied(), logical.get(at + 1).copied());
            let (Some(kind), Some(len)) = (kind, len.map(usize::from)) else {
                return Err(Error::Corrupt);
            };
            if len < 6 || at + len > end {
                return Err(Error::Corrupt);
            }
            let map = &logical[at..at + len];
            let map = match kind {
                1 => {
                    let p = partition(le16(map, 4)).ok_or(Error::Corrupt)?;
                    self.blocks += u64::from(p.blocks);
                    Map::Physical { start: u64::from(p.start) * self.block, blocks: p.blocks }
                }
                2 if len >= 64 => {
                    let id = &map[5..28];
                    let p = partition(le16(map, 38)).ok_or(Error::Corrupt)?;
                    let start = u64::from(p.start) * self.block;
                    if id.starts_with(b"*UDF Sparable Partition") {
                        self.blocks += u64::from(p.blocks);
                        let packet = u32::from(le16(map, 40)).max(1);
                        let tables = usize::from(map[42]).min(4);
                        let table = le32(map, 44).min(MAX_SPARING);
                        let mut spared = None;
                        for i in 0..tables {
                            spared = sparing(cache, self.block, le32(map, 48 + 4 * i), table)?;
                            if spared.is_some() {
                                break;
                            }
                        }
                        let spared = spared.ok_or(Error::Corrupt)?;
                        Map::Sparable { start, blocks: p.blocks, packet, spared }
                    } else if id.starts_with(b"*UDF Virtual Partition") {
                        // The physical partition's index.
                        let physical = Self::index_of(logical, count, p.number)?;
                        Map::Pending { physical, file: 0, mirror: 0 }
                    } else if id.starts_with(b"*UDF Metadata Partition") {
                        let physical = Self::index_of(logical, count, p.number)?;
                        Map::Pending { physical, file: le32(map, 40), mirror: le32(map, 44) }
                    } else {
                        return Err(Error::NotSupported);
                    }
                }
                _ => return Err(Error::Corrupt),
            };
            mem::push(&mut self.maps, map)?;
            at += len;
        }
        for i in 0..self.maps.len() {
            if let Map::Pending { physical, file, mirror } = self.maps[i] {
                let map = if file == 0 {
                    self.virtual_table(cache, physical, size)?
                } else {
                    match self.metadata(cache, physical, file) {
                        Ok(map) => map,
                        Err(_) => self.metadata(cache, physical, mirror)?,
                    }
                };
                self.maps[i] = map;
            }
        }
        Ok(())
    }

    /// The index of the type 1 map of partition `number`.
    fn index_of(logical: &[u8], count: usize, number: u16) -> Result<u16> {
        let mut at = 440;
        for i in 0..count {
            let (kind, len) = (logical[at], usize::from(logical[at + 1]));
            if len < 6 || at + len > logical.len() {
                break;
            }
            if kind == 1 && le16(logical, at + 4) == number {
                return u16::try_from(i).map_err(|_| Error::Corrupt);
            }
            at += len;
        }
        Err(Error::Corrupt)
    }

    /// The metadata partition, from its file at block `file` of the
    /// partition of map `physical`.
    fn metadata(&self, cache: &BlockCache, physical: u16, file: u32) -> Result<Map> {
        let icb = LongAd { len: self.block as u32, block: file, partition: physical };
        let entry = Entry::read(self, cache, icb)?;
        if entry.file_type != METADATA_FILE || entry.embedded.is_some() {
            return Err(Error::Corrupt);
        }
        Ok(Map::Metadata { runs: entry.runs })
    }

    /// The virtual partition: its table from the virtual allocation
    /// table, the last file on the disc, in the partition of map
    /// `physical`.
    fn virtual_table(&self, cache: &BlockCache, physical: u16, size: u64) -> Result<Map> {
        let Map::Physical { start, .. } = self.maps[usize::from(physical)] else {
            return Err(Error::Corrupt);
        };
        let last = (size / self.block).checked_sub(1).ok_or(Error::Corrupt)?;
        let first = start / self.block;
        // The last blocks may be the lead-out a drive could not read.
        for back in 0..16 {
            let Some(block) = last.checked_sub(back).and_then(|b| b.checked_sub(first)) else {
                break;
            };
            let icb = LongAd { len: self.block as u32, block: block as u32, partition: physical };
            let Ok(entry) = Entry::read(self, cache, icb) else { continue };
            if entry.size > MAX_VAT || entry.size < 4 {
                continue;
            }
            let mut data = zeroed(entry.size as usize)?;
            entry.read_at(cache, 0, &mut data)?;
            let entries = if entry.file_type == VAT_FILE {
                // UDF 2.00: a header, then the table.
                usize::from(le16(&data, 0)).min(data.len())..data.len()
            } else if data.len() >= 36
                && data[data.len() - 35..].starts_with(b"*UDF Virtual Alloc Tbl")
            {
                // UDF 1.50: the table, then an identifier.
                0..data.len() - 36
            } else {
                continue;
            };
            let bytes = &data[entries];
            let mut table = mem::vec_with_capacity(bytes.len() / 4)?;
            for &entry in bytes.as_chunks::<4>().0 {
                let _ = table.push_within_capacity(u32::from_le_bytes(entry));
            }
            return Ok(Map::Virtual { start, table });
        }
        Err(Error::Corrupt)
    }

    /// Where block `block` of partition `partition` is on the device, in
    /// bytes, and how many blocks from it lie in a row there.
    pub(crate) fn locate(&self, partition: u16, block: u32) -> Result<(u64, u32)> {
        let map = self.maps.get(usize::from(partition)).ok_or(Error::Corrupt)?;
        let size = self.block;
        match map {
            Map::Physical { start, blocks } => {
                let left = blocks.checked_sub(block).filter(|&n| n > 0).ok_or(Error::Corrupt)?;
                Ok((start + u64::from(block) * size, left))
            }
            Map::Sparable { start, blocks, packet, spared } => {
                let left = blocks.checked_sub(block).filter(|&n| n > 0).ok_or(Error::Corrupt)?;
                let first = block - block % packet;
                let row = (packet - block % packet).min(left);
                match spared.binary_search_by_key(&first, |&(original, _)| original) {
                    Ok(i) => Ok((u64::from(spared[i].1 + block % packet) * size, row)),
                    Err(_) => Ok((start + u64::from(block) * size, row)),
                }
            }
            Map::Virtual { start, table } => {
                let at = |b: u32| table.get(b as usize).copied().filter(|&p| p != u32::MAX);
                let physical = at(block).ok_or(Error::Corrupt)?;
                let mut row = 1;
                while row < 64 && at(block + row) == Some(physical + row) {
                    row += 1;
                }
                Ok((start + u64::from(physical) * size, row))
            }
            Map::Metadata { runs } => {
                let pos = u64::from(block) * size;
                let i = runs.partition_point(|run| run.pos + run.len <= pos);
                let run = runs.get(i).filter(|run| run.pos <= pos).ok_or(Error::Corrupt)?;
                let at = run.at.ok_or(Error::Corrupt)?;
                let left = ((run.len - (pos - run.pos)) / size).max(1);
                Ok((at + (pos - run.pos), u32::try_from(left).unwrap_or(u32::MAX)))
            }
            Map::Pending { .. } => Err(Error::Corrupt),
        }
    }
}

/// Whether the volume recognition sequence at 32 KiB has an NSR
/// descriptor: whether the disc is UDF.
#[inline(never)]
fn recognized(cache: &BlockCache, size: u64) -> Result<bool> {
    let mut id = [0u8; 7];
    let mut nsr = false;
    // One descriptor each 2048 bytes, or each block where blocks are
    // larger: what lies between is zeros.
    for i in 0..64 {
        let at = RECOGNITION + i * 2048;
        if at + 2048 > size {
            break;
        }
        cache.read(at, &mut id)?;
        match &id[1..6] {
            b"NSR02" | b"NSR03" => nsr = true,
            b"BEA01" | b"BOOT2" | b"CD001" | b"CDW02" => {}
            b"TEA01" => break,
            [0, 0, 0, 0, 0] if id[0] == 0 => {}
            _ => break,
        }
    }
    Ok(nsr)
}

/// The anchor volume descriptor pointer: the block size it is found
/// with, and the main and reserve volume descriptor sequences.
#[inline(never)]
fn anchor(cache: &BlockCache, size: u64, smallest: u64) -> Result<Option<(u64, Extent, Extent)>> {
    let mut avdp = [0u8; 512];
    for block in [512u64, 1024, 2048, 4096] {
        if block < smallest {
            continue;
        }
        let blocks = size / block;
        for at in [256, blocks.saturating_sub(256), blocks.saturating_sub(1)] {
            if at < 256 || (at + 1) * block > size {
                continue;
            }
            cache.read(at * block, &mut avdp)?;
            if tag(&avdp, Some(at as u32)) == Some(ident::ANCHOR) {
                return Ok(Some((block, format::extent(&avdp, 16), format::extent(&avdp, 24))));
            }
        }
    }
    Ok(None)
}

/// Reads the volume descriptor sequence in `extent` into `sequence`.
#[inline(never)]
fn read_sequence(
    cache: &BlockCache,
    block: u64,
    mut extent: Extent,
    sequence: &mut Sequence,
) -> Result<()> {
    let mut buf = zeroed(block as usize)?;
    let mut pointers = 0;
    'sequence: loop {
        let count = (u64::from(extent.len) / block).min(MAX_DESCRIPTORS);
        for i in 0..count as u32 {
            let at = extent.block.checked_add(i).ok_or(Error::Corrupt)?;
            if cache.read(u64::from(at) * block, &mut buf).is_err() {
                return Ok(());
            }
            match tag(&buf, Some(at)) {
                Some(ident::PARTITION) => {
                    let p = Partition {
                        number: le16(&buf, 22),
                        start: le32(&buf, 188),
                        blocks: le32(&buf, 192),
                    };
                    let known =
                        sequence.partitions[..sequence.count].iter().any(|q| q.number == p.number);
                    if !known && sequence.count < MAX_PARTITIONS {
                        sequence.partitions[sequence.count] = p;
                        sequence.count += 1;
                    }
                }
                Some(ident::LOGICAL_VOLUME) if sequence.logical.is_none() => {
                    sequence.logical = Some(mem::copy(&buf)?);
                }
                Some(ident::POINTER) => {
                    pointers += 1;
                    if pointers > MAX_POINTERS {
                        return Err(Error::Corrupt);
                    }
                    extent = format::extent(&buf, 20);
                    continue 'sequence;
                }
                Some(ident::TERMINATING) | None => return Ok(()),
                Some(_) => {}
            }
        }
        return Ok(());
    }
}

/// A sparable partition's sparing table at block `at`, `len` bytes long,
/// if it is sound.
fn sparing(cache: &BlockCache, block: u64, at: u32, len: u32) -> Result<Option<KVec<(u32, u32)>>> {
    let len = (len as usize).max(56);
    let mut table = zeroed(len)?;
    if cache.read(u64::from(at) * block, &mut table).is_err()
        || tag(&table, Some(at)) != Some(0)
        || !table[17..].starts_with(b"*UDF Sparing Table")
    {
        return Ok(None);
    }
    let count = usize::from(le16(&table, 48)).min((len - 56) / 8);
    let mut spared = mem::vec_with_capacity(count)?;
    for i in 0..count {
        let (original, mapped) = (le32(&table, 56 + 8 * i), le32(&table, 60 + 8 * i));
        // Free or defective entries.
        if original < 0xffff_fff0 {
            let _ = spared.push_within_capacity((original, mapped));
        }
    }
    spared.sort_unstable();
    Ok(Some(spared))
}

/// The root directory's ICB, from the file set descriptor at `at`.
#[inline(never)]
fn file_set(cache: &BlockCache, volume: &Volume, at: LongAd) -> Result<LongAd> {
    let (byte, _) = volume.locate(at.partition, at.block)?;
    let mut buf = zeroed(volume.block as usize)?;
    cache.read(byte, &mut buf)?;
    if tag(&buf, Some(at.block)) != Some(ident::FILE_SET) {
        return Err(Error::Corrupt);
    }
    Ok(format::long_ad(&buf, 400))
}
