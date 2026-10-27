// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! New chunks: more of the device for data, metadata or the chunk tree,
//! when every block group of that kind is full (ADR 0131).
//!
//! A chunk is made in memory first: the free ranges of the device it lies
//! on, its logical addresses after every other chunk, and its block
//! group's free space, so that its space may be taken at once, even by
//! the changes that record it. The commit records it, as Linux does at
//! the end of the transaction that made it: its chunk item, and for a
//! chunk of the chunk tree its copy in the superblock's system chunk
//! array; a device extent for each stripe; the device's bytes used; its
//! block group item; and its free-space info.
//!
//! Its size is Linux's: 1 GiB for data, 256 MiB for metadata (1 GiB on a
//! device over 50 GiB) and 32 MiB for the chunk tree, but at most a tenth
//! of the device, rounded up to 16 MiB; and less when no free range of
//! the device is that large.

use radix_vfs::mem::{self, KVec};
use radix_vfs::{Error, Result};

use crate::chunk::{Chunk, Stripe, kind};
use crate::cow::Writer;
use crate::format::{Key, item, tree, u64_at};
use crate::space::{self, Ranges};
use crate::tree::{Cursor, MAX_LEVEL};
use crate::volume::Volume;

/// The object ID of the device items in the chunk tree.
const DEV_ITEMS: u64 = 1;

/// The start of every device, never allocated, as on Linux: room for a
/// boot loader and the first superblock.
const RESERVED: u64 = 1 << 20;

/// The stripe length every chunk item gives, and the unit of a chunk's
/// length.
const STRIPE_LEN: u64 = 64 << 10;

/// The tenth of the device a chunk may take is rounded up to this.
const ROUND: u64 = 16 << 20;

/// The smallest stripe made when the device has no room for a larger one.
const MIN_STRIPE: u64 = 1 << 20;

/// Where the superblock keeps its system chunk array, its size, and the
/// most it holds.
const SYS_ARRAY_SIZE_AT: usize = 0xa0;
const SYS_ARRAY_AT: usize = 0x32b;
const SYS_ARRAY_MAX: usize = 2048;

/// The superblock's copy of its device item: its total and used bytes.
const DEV_TOTAL_AT: usize = 0xc9 + 8;
const DEV_USED_AT: usize = 0xc9 + 16;

/// A device extent item: the chunk tree, the chunk's object ID, its
/// logical start, its length, and the chunk tree's UUID.
const DEV_EXTENT_SIZE: usize = 48;

/// A block group item: bytes used, the chunk's object ID, flags.
const GROUP_ITEM_SIZE: usize = 24;

/// A free-space info item: how many extents or bitmaps, flags.
const FREE_SPACE_INFO_SIZE: usize = 8;

/// The trees a commit changes, each a path of blocks it may copy: the
/// extent, free-space, block group, root, chunk, device and checksum
/// trees, and a subvolume's.
const COMMIT_PATHS: u64 = 8;

/// The paths of blocks one change may copy, beyond the commit's.
const CHANGE_PATHS: u64 = 8;

/// The most chunks made to find room for one change.
const MAX_NEW: usize = 4;

fn put_u64(bytes: &mut [u8], at: usize, value: u64) {
    bytes[at..at + 8].copy_from_slice(&value.to_le_bytes());
}

/// The device as chunks use it.
pub(crate) struct Device {
    /// Its size, as its device item says.
    total: u64,
    /// The ranges its device extents take, and those of chunks made.
    used: Ranges,
    /// The chunk tree's UUID, which every device extent names.
    chunk_tree_uuid: [u8; 16],
}

/// The chunks a transaction made, and what they change that the commit
/// writes to the superblock.
pub(crate) struct Grown {
    /// Chunks made that the trees do not record yet.
    pub made: KVec<Chunk>,
    /// The device, read at the first chunk made.
    pub device: Option<Device>,
    /// Bytes of the device the recorded chunks take, for the superblock's
    /// device item.
    pub dev_bytes: u64,
    /// Entries for the system chunk array: a key and a chunk item each.
    pub sys_chunks: KVec<u8>,
}

impl Grown {
    /// No chunks made.
    pub(crate) fn new() -> Grown {
        Grown { made: mem::vec(), device: None, dev_bytes: 0, sys_chunks: mem::vec() }
    }
}

impl Writer {
    /// Makes a chunk that holds `want` ([`kind`]), with room for `size`
    /// bytes, from the device's free ranges.
    ///
    /// Fails with [`Error::NoSpace`], changing nothing, when no free range
    /// of the device is large enough, or there is no group of `want` to
    /// take a profile from, or its profile needs more than one device.
    pub(crate) fn new_chunk(&mut self, volume: &Volume, want: u64, size: u64) -> Result<()> {
        let flags = self.space.flags_for(want).ok_or(Error::NoSpace)?;
        let copies: u64 = match flags & kind::PROFILE {
            0 => 1,
            kind::DUP => 2,
            _ => return Err(Error::NoSpace),
        };
        if self.grown.device.is_none() {
            self.grown.device = Some(read_device(volume)?);
        }
        let device = self.grown.device.as_ref().ok_or(Error::Corrupt)?;
        if flags & kind::SYSTEM != 0 {
            let written = crate::format::u32_at(&volume.superblock_bytes()?, SYS_ARRAY_SIZE_AT);
            let array = written as usize + self.grown.sys_chunks.len();
            let entry = Key::SIZE + Chunk::item_size(copies as usize);
            let pending = self.grown.made.iter().filter(|c| c.kind & kind::SYSTEM != 0).count();
            if array + entry * (pending + 1) > SYS_ARRAY_MAX {
                return Err(Error::NoSpace);
            }
        }
        let usual: u64 = if flags & kind::DATA != 0 {
            1 << 30
        } else if flags & kind::METADATA != 0 {
            if device.total > 50 << 30 { 1 << 30 } else { 256 << 20 }
        } else {
            32 << 20
        };
        let least = size.next_multiple_of(STRIPE_LEN).max(MIN_STRIPE);
        let stripe = usual.min((device.total / 10).next_multiple_of(ROUND)).max(least);
        let (at, stripe) = find_room(&device.used, device.total, stripe, least, copies)?;
        let start = volume.with_chunks(crate::chunk::ChunkMap::end);
        let devid = volume.superblock().devid;
        // From here on only memory runs short, and a half made chunk
        // leaves the writer refusing changes.
        let made = self.add_chunk(volume, start, stripe, flags, devid, at, copies);
        if made.is_err() {
            self.break_off();
        }
        made
    }

    #[allow(clippy::too_many_arguments)]
    fn add_chunk(
        &mut self,
        volume: &Volume,
        start: u64,
        length: u64,
        flags: u64,
        devid: u64,
        at: u64,
        copies: u64,
    ) -> Result<()> {
        let mut stripes = mem::vec_with_capacity(copies as usize)?;
        for i in 0..copies {
            mem::push(&mut stripes, Stripe { devid, offset: at + i * length })?;
        }
        let chunk =
            Chunk { start, length, stripe_len: STRIPE_LEN, kind: flags, sub_stripes: 1, stripes };
        let device = self.grown.device.as_mut().ok_or(Error::Corrupt)?;
        space::add(&mut device.used, at, at + copies * length)?;
        mem::push(&mut self.grown.made, chunk.copy()?)?;
        volume.add_chunk(chunk)?;
        self.space.add_group(volume, start, length, flags)
    }

    /// Makes sure, before a change, that metadata has room for the most
    /// blocks it and the commit after it may copy: each path a copy and a
    /// split, down from the highest level. It makes metadata chunks if it
    /// must, and commits if what the last commit freed would make room.
    /// A change that frees space (`freeing`: removing, cutting) needs room
    /// only for the commit, so that a full filesystem can always be made
    /// less full, as on Linux.
    ///
    /// Fails with [`Error::NoSpace`] when there is no room, before the
    /// change starts, so it fails whole.
    pub(crate) fn make_room(&mut self, volume: &Volume, freeing: bool) -> Result<()> {
        let need = self.room_needed(volume, freeing);
        for _ in 0..MAX_NEW {
            if self.space.free_bytes(kind::METADATA) >= need {
                return Ok(());
            }
            match self.new_chunk(volume, kind::METADATA, 0) {
                Ok(()) => {}
                Err(Error::NoSpace) => break,
                Err(e) => return Err(e),
            }
        }
        if self.space.free_bytes(kind::METADATA) < need
            && self.space.pinned_bytes(kind::METADATA) > 0
        {
            self.commit(volume)?;
        }
        if self.space.free_bytes(kind::METADATA) >= need { Ok(()) } else { Err(Error::NoSpace) }
    }

    /// The metadata a change needs free before it starts: room for each
    /// path of blocks it and the commit after it may copy, a copy and a
    /// split down from the highest level, and for the extent tree's item
    /// of each block made or freed.
    fn room_needed(&self, volume: &Volume, freeing: bool) -> u64 {
        let nodesize = u64::from(volume.superblock().nodesize);
        let path = nodesize * 2 * u64::from(MAX_LEVEL);
        let mut need = path * COMMIT_PATHS + nodesize * (self.changes.len() as u64 / 64 + 1);
        if !freeing {
            need += path * CHANGE_PATHS;
        }
        need
    }

    /// The bytes `want` may still take: free now, free once the commit is
    /// durable, and what new chunks could hold of the device's unallocated
    /// bytes. Where data and metadata share block groups (`--mixed`),
    /// data's room leaves metadata what writing it back needs: room for
    /// two changes, and a 32nd of the data for its items.
    pub(crate) fn room(&mut self, volume: &Volume, want: u64) -> Result<u64> {
        let room = self.space.free_bytes(want) + self.space.pinned_bytes(want);
        let flags = self.space.flags_for(want).unwrap_or(0);
        let copies = match flags & kind::PROFILE {
            0 => 1,
            kind::DUP => 2,
            _ => return Ok(room),
        };
        if self.grown.device.is_none() {
            self.grown.device = Some(read_device(volume)?);
        }
        let device = self.grown.device.as_ref().ok_or(Error::Corrupt)?;
        let used: u64 = device.used.iter().map(|&(s, e)| e - s).sum();
        let unallocated = device.total.saturating_sub(RESERVED).saturating_sub(used);
        let room = room + unallocated / copies;
        if want == kind::DATA && flags & kind::METADATA != 0 {
            let kept = 2 * self.room_needed(volume, false) + room / 32;
            return Ok(room.saturating_sub(kept));
        }
        Ok(room)
    }

    /// Whether the chunk of block group `start` waits to be recorded.
    pub(crate) fn unrecorded(&self, start: u64) -> bool {
        self.grown.made.iter().any(|c| c.start == start)
    }

    /// Records the chunks made since the last time in the trees; returns
    /// whether there were any.
    pub(crate) fn record_chunks(&mut self, volume: &Volume) -> Result<bool> {
        if self.grown.made.is_empty() {
            return Ok(false);
        }
        let made = core::mem::replace(&mut self.grown.made, mem::vec());
        for chunk in &made {
            self.record(volume, chunk)?;
        }
        Ok(true)
    }

    fn record(&mut self, volume: &Volume, chunk: &Chunk) -> Result<()> {
        let sb = volume.superblock();
        let (sectorsize, devid, dev_uuid) = (sb.sectorsize, sb.devid, sb.dev_uuid);
        let size = Chunk::item_size(chunk.stripes.len());
        let key = Key::new(tree::FIRST_CHUNK, item::CHUNK_ITEM, chunk.start);
        self.insert_with(volume, tree::CHUNK, &key, size, |d| {
            chunk.write(d, sectorsize, &dev_uuid);
        })?;
        if chunk.kind & kind::SYSTEM != 0 {
            let mut entry = crate::zeroed(Key::SIZE + size)?;
            put_u64(&mut entry, 0, key.objectid);
            entry[8] = key.kind;
            put_u64(&mut entry, 9, key.offset);
            chunk.write(&mut entry[Key::SIZE..], sectorsize, &dev_uuid);
            mem::extend(&mut self.grown.sys_chunks, &entry)?;
        }
        let uuid = self.grown.device.as_ref().ok_or(Error::Corrupt)?.chunk_tree_uuid;
        for stripe in &chunk.stripes {
            let key = Key::new(stripe.devid, item::DEV_EXTENT, stripe.offset);
            self.insert_with(volume, tree::DEV, &key, DEV_EXTENT_SIZE, |d| {
                put_u64(d, 0, tree::CHUNK);
                put_u64(d, 8, tree::FIRST_CHUNK);
                put_u64(d, 16, chunk.start);
                put_u64(d, 24, chunk.length);
                d[32..48].copy_from_slice(&uuid);
            })?;
        }
        let bytes = chunk.length * chunk.stripes.len() as u64;
        let dev = Key::new(DEV_ITEMS, item::DEV_ITEM, devid);
        self.update(volume, tree::CHUNK, &dev, |d| put_u64(d, 16, u64_at(d, 16) + bytes))?;
        self.grown.dev_bytes += bytes;
        let group = Key::new(chunk.start, item::BLOCK_GROUP_ITEM, chunk.length);
        let group_tree = self.space.group_tree();
        self.insert_with(volume, group_tree, &group, GROUP_ITEM_SIZE, |d| {
            put_u64(d, 0, 0);
            put_u64(d, 8, tree::FIRST_CHUNK);
            put_u64(d, 16, chunk.kind);
        })?;
        let info = Key::new(chunk.start, item::FREE_SPACE_INFO, chunk.length);
        self.insert_with(volume, tree::FREE_SPACE, &info, FREE_SPACE_INFO_SIZE, |d| d.fill(0))
    }

    /// Writes what the chunks recorded change to the superblock `sb`: its
    /// device item's bytes used, and its system chunk array.
    pub(crate) fn grow_superblock(&self, sb: &mut [u8]) -> Result<()> {
        put_u64(sb, DEV_USED_AT, u64_at(sb, DEV_USED_AT) + self.grown.dev_bytes);
        let entries = &self.grown.sys_chunks;
        if !entries.is_empty() {
            let size = crate::format::u32_at(sb, SYS_ARRAY_SIZE_AT) as usize;
            if size + entries.len() > SYS_ARRAY_MAX {
                return Err(Error::Corrupt);
            }
            let at = SYS_ARRAY_AT + size;
            sb[at..at + entries.len()].copy_from_slice(entries);
            let size = (size + entries.len()) as u32;
            sb[SYS_ARRAY_SIZE_AT..SYS_ARRAY_SIZE_AT + 4].copy_from_slice(&size.to_le_bytes());
        }
        Ok(())
    }

    /// Forgets what [`Writer::grow_superblock`] wrote, its commit being
    /// durable.
    pub(crate) fn grown_committed(&mut self) {
        self.grown.dev_bytes = 0;
        self.grown.sys_chunks.clear();
    }
}

/// Reads the device's size and the ranges its device extents take.
fn read_device(volume: &Volume) -> Result<Device> {
    let devid = volume.superblock().devid;
    let total = u64_at(&volume.superblock_bytes()?, DEV_TOTAL_AT);
    let root = Writer::tree_root(volume, tree::DEV)?;
    let mut cursor = Cursor::seek(volume, &root, &Key::first(devid, item::DEV_EXTENT))?;
    let mut used = mem::vec();
    let mut chunk_tree_uuid = None;
    while let Some((key, data)) = cursor.get() {
        if !key.is(devid, item::DEV_EXTENT) {
            break;
        }
        if data.len() < DEV_EXTENT_SIZE {
            return Err(Error::Corrupt);
        }
        let len = u64_at(data, 24);
        let end = key.offset.checked_add(len).ok_or(Error::Corrupt)?;
        space::add(&mut used, key.offset, end)?;
        chunk_tree_uuid.get_or_insert_with(|| crate::format::uuid_at(data, 32));
        cursor.advance()?;
    }
    let chunk_tree_uuid = chunk_tree_uuid.ok_or(Error::Corrupt)?;
    Ok(Device { total, used, chunk_tree_uuid })
}

/// Where on a device of `total` bytes, whose ranges `used` are taken,
/// `copies` stripes of `want` bytes fit one after another: the first free
/// range large enough, or else the largest, with stripes as long as it
/// allows, but no shorter than `least`. Returns the first stripe's offset
/// and the stripes' length.
fn find_room(used: &Ranges, total: u64, want: u64, least: u64, copies: u64) -> Result<(u64, u64)> {
    let mut best = (0, 0);
    let mut from = RESERVED;
    let ends = used.iter().map(|&(s, e)| (s, e)).chain(core::iter::once((total, total)));
    for (s, e) in ends {
        let hole_end = s.min(total);
        if hole_end > from {
            let len = hole_end - from;
            if len >= want * copies {
                return Ok((from, want));
            }
            if len > best.1 {
                best = (from, len);
            }
        }
        from = from.max(e);
    }
    let stripe = (best.1 / copies) / STRIPE_LEN * STRIPE_LEN;
    if stripe < least {
        return Err(Error::NoSpace);
    }
    Ok((best.0, stripe))
}
