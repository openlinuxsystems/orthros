// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! New chunks: more of the device for data, metadata or the chunk tree,
//! when every block group of that kind is full (ADR 0131).
//!
//! A chunk is made in memory first: the free ranges of the disks it lies
//! on, its logical addresses after every other chunk, and its block
//! group's free space, so that its space may be taken at once, even by
//! the changes that record it. The commit records it, as Linux does at
//! the end of the transaction that made it: its chunk item, and for a
//! chunk of the chunk tree its copy in the superblock's system chunk
//! array; a device extent for each stripe; each disk's bytes used; its
//! block group item; and its free-space info.
//!
//! Its size is Linux's: 1 GiB for data, 256 MiB for metadata (1 GiB on a
//! filesystem over 50 GiB) and 32 MiB for the chunk tree, but at most a
//! tenth of the filesystem's disks, rounded up to 16 MiB; and less when
//! no free range of the disks is that large.
//!
//! Its stripes go on the disks with the largest free ranges, as Linux's
//! `gather_device_info` picks them (ADR 0153): one for single, two on one
//! disk for DUP, one on each of two, three or four disks for RAID1,
//! RAID1C3 and RAID1C4, and on as many disks as have room for RAID0 and,
//! in pairs, RAID10. Missing disks get none: on a filesystem that lacks
//! one, a profile that needs it makes no chunk, rather than one with
//! fewer copies than asked for.

use radix_vfs::mem::{self, KVec};
use radix_vfs::{Error, Result};

use crate::chunk::{Chunk, Stripe, kind};
use crate::cow::Writer;
use crate::format::{Key, item, tree, u64_at};
use crate::space::{self, Ranges};
use crate::tree::{Cursor, MAX_LEVEL};
use crate::volume::{DEV_ITEMS, MAX_DISKS, Volume};

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

/// Where the superblock keeps its total bytes and its count of disks.
const TOTAL_BYTES_AT: usize = 0x70;
const NUM_DEVICES_AT: usize = 0x88;

/// Where a device item keeps its total and used bytes.
const DEV_TOTAL_AT: usize = 8;
const DEV_USED_AT: usize = 16;

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

/// A disk as chunks use it.
pub(crate) struct Device {
    devid: u64,
    /// Whether chunks may go on it: it is there, and writable.
    usable: bool,
    /// Its size, as its device item says.
    total: u64,
    /// The ranges its device extents take, and those of chunks made.
    used: Ranges,
}

impl Device {
    /// Its bytes no chunk takes.
    fn unallocated(&self) -> u64 {
        let used: u64 = self.used.iter().map(|&(s, e)| e - s).sum();
        self.total.saturating_sub(RESERVED).saturating_sub(used)
    }
}

/// The disks as chunks use them, read at the first chunk made.
pub(crate) struct Devices {
    list: KVec<Device>,
    /// The chunk tree's UUID, which every device extent names.
    chunk_tree_uuid: [u8; 16],
}

/// The chunks a transaction made, and what they change that the commit
/// writes to the superblock.
pub(crate) struct Grown {
    /// Chunks made that the trees do not record yet.
    pub made: KVec<Chunk>,
    /// The disks, read at the first chunk made.
    pub devices: Option<Devices>,
    /// Entries for the system chunk array: a key and a chunk item each.
    pub sys_chunks: KVec<u8>,
    /// The chunks a balance removed that the system chunk array names, by
    /// their start, for the commit to take out (ADR 0159).
    pub sys_dropped: KVec<u64>,
    /// The flags new chunks of data, metadata and the chunk tree take
    /// while a balance converts them to another profile.
    pub targets: [Option<u64>; 3],
    /// Disks added less disks removed since the last commit, and their
    /// bytes, for the superblock's count and total.
    pub disks: (i64, i64),
}

impl Grown {
    /// No chunks made.
    pub(crate) fn new() -> Grown {
        Grown {
            made: mem::vec(),
            devices: None,
            sys_chunks: mem::vec(),
            sys_dropped: mem::vec(),
            targets: [None; 3],
            disks: (0, 0),
        }
    }

    /// Where `targets` keeps the target of `want`: data, metadata, or
    /// the chunk tree.
    pub(crate) fn target_at(want: u64) -> usize {
        if want & kind::DATA != 0 {
            0
        } else if want & kind::METADATA != 0 {
            1
        } else {
            2
        }
    }
}

/// How many disks chunks of `profile` ([`kind`]) need, at least; `None`
/// for a profile Radix does not write.
pub(crate) fn disks_needed(profile: u64) -> Option<usize> {
    Layout::of(profile).map(|l| l.min)
}

/// How a profile lays a chunk's stripes out: on how few and how many
/// disks, with how many copies of each byte, and how many stripes on
/// each disk.
#[derive(Clone, Copy)]
struct Layout {
    min: usize,
    max: usize,
    /// Copies of each byte, on distinct disks but for DUP.
    copies: usize,
    /// The stripes of one disk: two for DUP.
    per_disk: usize,
}

impl Layout {
    /// The layout of `profile` ([`kind`]), or `None` for RAID5 and RAID6,
    /// which Radix does not write.
    fn of(profile: u64) -> Option<Layout> {
        let (min, max, copies, per_disk) = match profile {
            0 => (1, 1, 1, 1),
            kind::DUP => (1, 1, 2, 2),
            kind::RAID1 => (2, 2, 2, 1),
            kind::RAID1C3 => (3, 3, 3, 1),
            kind::RAID1C4 => (4, 4, 4, 1),
            kind::RAID0 => (1, MAX_DISKS, 1, 1),
            kind::RAID10 => (2, MAX_DISKS, 2, 1),
            _ => return None,
        };
        Some(Layout { min, max, copies, per_disk })
    }

    /// Whether its stripes go in pairs of copies on as many disks as
    /// have room: RAID10.
    fn paired(&self) -> bool {
        self.copies == 2 && self.per_disk == 1 && self.max > 2
    }

    /// How many stripes' worth of a chunk's bytes `disks` disks hold: the
    /// stripes that are not copies.
    fn data_stripes(&self, disks: usize) -> u64 {
        (disks * self.per_disk / self.copies).max(1) as u64
    }

    /// The bytes chunks of this layout could still hold on disks with
    /// `free` bytes each, its copies of each byte on distinct disks: the
    /// least, over `j` below `copies`, of the sum without the `j` largest
    /// over `copies - j`, as Linux's `btrfs_calc_avail_data_space`
    /// reckons it.
    fn room(&self, free: &mut [u64]) -> u64 {
        let sum: u64 = free.iter().sum();
        if self.per_disk > 1 {
            return sum / self.per_disk as u64;
        }
        free.sort_unstable_by(|a, b| b.cmp(a));
        let mut room = sum / self.copies as u64;
        let mut top = 0;
        for (j, &largest) in free.iter().enumerate().take(self.copies - 1) {
            top += largest;
            room = room.min((sum - top) / (self.copies - j - 1) as u64);
        }
        room
    }
}

impl Writer {
    /// Makes a chunk that holds `want` ([`kind`]), with room for `size`
    /// bytes, from the disks' free ranges.
    ///
    /// Fails with [`Error::NoSpace`], changing nothing, when no free range
    /// is large enough on as many disks as its profile needs, or there is
    /// no group of `want` to take a profile from.
    #[inline(never)]
    pub(crate) fn new_chunk(&mut self, volume: &Volume, want: u64, size: u64) -> Result<()> {
        let flags = match self.grown.targets[Grown::target_at(want)] {
            Some(flags) => flags,
            None => self.space.flags_for(want).ok_or(Error::NoSpace)?,
        };
        let layout = Layout::of(flags & kind::PROFILE).ok_or(Error::NoSpace)?;
        if self.grown.devices.is_none() {
            self.grown.devices = Some(read_devices(volume)?);
        }
        let devices = self.grown.devices.as_ref().ok_or(Error::Corrupt)?;
        let usable = devices.list.iter().filter(|d| d.usable).count();
        if flags & kind::SYSTEM != 0 {
            let written = crate::format::u32_at(&volume.superblock_bytes()?, SYS_ARRAY_SIZE_AT);
            let array = written as usize + self.grown.sys_chunks.len();
            let stripes = usable.min(layout.max) * layout.per_disk;
            let entry = Key::SIZE + Chunk::item_size(stripes);
            let pending = self.grown.made.iter().filter(|c| c.kind & kind::SYSTEM != 0).count();
            if array + entry * (pending + 1) > SYS_ARRAY_MAX {
                return Err(Error::NoSpace);
            }
        }
        let total: u64 = devices.list.iter().filter(|d| d.usable).map(|d| d.total).sum();
        let usual: u64 = if flags & kind::DATA != 0 {
            1 << 30
        } else if flags & kind::METADATA != 0 {
            if total > 50 << 30 { 1 << 30 } else { 256 << 20 }
        } else {
            32 << 20
        };
        let length = usual.min((total / 10).next_multiple_of(ROUND));
        let (picked, stripe, disks) = pick_disks(devices, layout, length, size)?;
        let start = volume.with_chunks(crate::chunk::ChunkMap::end);
        // From here on only memory runs short, and a half made chunk
        // leaves the writer refusing changes.
        let made = self.add_chunk(volume, start, flags, layout, &picked[..disks], stripe);
        if made.is_err() {
            self.break_off();
        }
        made
    }

    /// Adds the chunk of `layout` at `start`, its stripes of `stripe`
    /// bytes from where `picked` says on each disk.
    fn add_chunk(
        &mut self,
        volume: &Volume,
        start: u64,
        flags: u64,
        layout: Layout,
        picked: &[(usize, u64)],
        stripe: u64,
    ) -> Result<()> {
        let devices = self.grown.devices.as_mut().ok_or(Error::Corrupt)?;
        let mut stripes = mem::vec_with_capacity(picked.len() * layout.per_disk)?;
        for &(device, at) in picked {
            let device = devices.list.get_mut(device).ok_or(Error::Corrupt)?;
            for i in 0..layout.per_disk as u64 {
                mem::push(&mut stripes, Stripe { devid: device.devid, offset: at + i * stripe })?;
            }
            space::add(&mut device.used, at, at + layout.per_disk as u64 * stripe)?;
        }
        let sub_stripes = if flags & kind::RAID10 != 0 { 2 } else { 1 };
        let length = stripe * layout.data_stripes(picked.len());
        let chunk =
            Chunk { start, length, stripe_len: STRIPE_LEN, kind: flags, sub_stripes, stripes };
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

    /// Makes sure, before a change to the chunk tree, that the system
    /// chunks have room for a path of it copied and split, making one if
    /// they must, as [`Writer::make_room`] does for metadata; a balance
    /// moving the system chunk needs it (ADR 0159).
    pub(crate) fn make_system_room(&mut self, volume: &Volume) -> Result<()> {
        let need = u64::from(volume.superblock().nodesize) * 2 * u64::from(MAX_LEVEL);
        if self.space.free_bytes(kind::SYSTEM) < need {
            match self.new_chunk(volume, kind::SYSTEM, 0) {
                Ok(()) | Err(Error::NoSpace) => {}
                Err(e) => return Err(e),
            }
        }
        if self.space.free_bytes(kind::SYSTEM) >= need { Ok(()) } else { Err(Error::NoSpace) }
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
        let Some(layout) = Layout::of(flags & kind::PROFILE) else { return Ok(room) };
        if self.grown.devices.is_none() {
            self.grown.devices = Some(read_devices(volume)?);
        }
        let devices = self.grown.devices.as_ref().ok_or(Error::Corrupt)?;
        let mut free = [0; MAX_DISKS];
        let mut n = 0;
        for device in devices.list.iter().filter(|d| d.usable) {
            free[n] = device.unallocated();
            n += 1;
        }
        let room = room + if n >= layout.min { layout.room(&mut free[..n]) } else { 0 };
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
        let sectorsize = volume.superblock().sectorsize;
        let uuid_of =
            |devid| volume.disks().iter().find(|d| d.devid == devid).map_or([0; 16], |d| d.uuid);
        let size = Chunk::item_size(chunk.stripes.len());
        let key = Key::new(tree::FIRST_CHUNK, item::CHUNK_ITEM, chunk.start);
        self.insert_with(volume, tree::CHUNK, &key, size, |d| {
            chunk.write(d, sectorsize, uuid_of);
        })?;
        if chunk.kind & kind::SYSTEM != 0 {
            let mut entry = crate::zeroed(Key::SIZE + size)?;
            put_u64(&mut entry, 0, key.objectid);
            entry[8] = key.kind;
            put_u64(&mut entry, 9, key.offset);
            chunk.write(&mut entry[Key::SIZE..], sectorsize, uuid_of);
            mem::extend(&mut self.grown.sys_chunks, &entry)?;
        }
        let uuid = self.grown.devices.as_ref().ok_or(Error::Corrupt)?.chunk_tree_uuid;
        let bytes = chunk.stripe_bytes();
        for stripe in &chunk.stripes {
            let key = Key::new(stripe.devid, item::DEV_EXTENT, stripe.offset);
            self.insert_with(volume, tree::DEV, &key, DEV_EXTENT_SIZE, |d| {
                put_u64(d, 0, tree::CHUNK);
                put_u64(d, 8, tree::FIRST_CHUNK);
                put_u64(d, 16, chunk.start);
                put_u64(d, 24, bytes);
                d[32..48].copy_from_slice(&uuid);
            })?;
            // The commit copies each disk's item to its superblock.
            let dev = Key::new(DEV_ITEMS, item::DEV_ITEM, stripe.devid);
            self.update(volume, tree::CHUNK, &dev, |d| {
                put_u64(d, DEV_USED_AT, u64_at(d, DEV_USED_AT) + bytes);
            })?;
        }
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
    /// system chunk array, and its count of disks and their bytes.
    pub(crate) fn grow_superblock(&self, sb: &mut [u8]) -> Result<()> {
        for &start in &self.grown.sys_dropped {
            drop_sys_chunk(sb, start)?;
        }
        let (count, bytes) = self.grown.disks;
        if count != 0 || bytes != 0 {
            let devices = u64_at(sb, NUM_DEVICES_AT).checked_add_signed(count);
            let total = u64_at(sb, TOTAL_BYTES_AT).checked_add_signed(bytes);
            put_u64(sb, NUM_DEVICES_AT, devices.ok_or(Error::Corrupt)?);
            put_u64(sb, TOTAL_BYTES_AT, total.ok_or(Error::Corrupt)?);
        }
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
        self.grown.sys_chunks.clear();
        self.grown.sys_dropped.clear();
        self.grown.disks = (0, 0);
    }
}

impl Writer {
    /// The ranges of each disk that may take chunks and that no chunk
    /// takes, past the [`RESERVED`] start: its device ID, and where each
    /// starts and ends, for a trim (ADR 0158). The chunks made since the
    /// mount count, so that a trim never reaches one.
    pub(crate) fn unallocated(&mut self, volume: &Volume) -> Result<KVec<(u64, u64, u64)>> {
        if self.grown.devices.is_none() {
            self.grown.devices = Some(read_devices(volume)?);
        }
        let devices = self.grown.devices.as_ref().ok_or(Error::Corrupt)?;
        let mut out = mem::vec();
        for device in devices.list.iter().filter(|d| d.usable) {
            let mut from = RESERVED;
            let ends =
                device.used.iter().copied().chain(core::iter::once((device.total, device.total)));
            for (s, e) in ends {
                let end = s.min(device.total);
                if end > from {
                    mem::push(&mut out, (device.devid, from, end))?;
                }
                from = from.max(e);
            }
        }
        Ok(out)
    }
}

impl Writer {
    /// Removes the chunk of block group `start`, which a balance emptied
    /// and a commit settled (ADR 0159): its chunk item and, for a chunk of
    /// the chunk tree, its entry in the system chunk array; its device
    /// extents, and the bytes each disk's item counts used; its block
    /// group item and its free-space items. Its range of each disk may
    /// take a chunk at once: it holds nothing the last commit needs.
    #[inline(never)]
    pub(crate) fn remove_chunk(&mut self, volume: &Volume, start: u64) -> Result<()> {
        let chunk = volume
            .with_chunks(|map| map.find(start).filter(|c| c.start == start).map(Chunk::copy))
            .ok_or(Error::Corrupt)??;
        let bytes = chunk.stripe_bytes();
        self.delete(volume, tree::CHUNK, &Key::new(tree::FIRST_CHUNK, item::CHUNK_ITEM, start))?;
        if chunk.kind & kind::SYSTEM != 0 {
            mem::push(&mut self.grown.sys_dropped, start)?;
        }
        for stripe in &chunk.stripes {
            self.delete(
                volume,
                tree::DEV,
                &Key::new(stripe.devid, item::DEV_EXTENT, stripe.offset),
            )?;
            let dev = Key::new(DEV_ITEMS, item::DEV_ITEM, stripe.devid);
            self.update(volume, tree::CHUNK, &dev, |d| {
                put_u64(d, DEV_USED_AT, u64_at(d, DEV_USED_AT).saturating_sub(bytes));
            })?;
            let device = self.grown.devices.as_mut().and_then(|devices| {
                devices.list.iter_mut().find(|device| device.devid == stripe.devid)
            });
            if let Some(device) = device {
                space::remove(&mut device.used, stripe.offset, stripe.offset + bytes)?;
            }
        }
        let group_tree = self.space.group_tree();
        self.delete(volume, group_tree, &Key::new(start, item::BLOCK_GROUP_ITEM, chunk.length))?;
        let end = start + chunk.length;
        let mut free = mem::vec();
        {
            let root = Writer::tree_root(volume, tree::FREE_SPACE)?;
            let mut cursor = Cursor::seek(volume, &root, &Key::new(start, 0, 0))?;
            while let Some(key) = cursor.key() {
                if key.objectid >= end {
                    break;
                }
                if (item::FREE_SPACE_INFO..=item::FREE_SPACE_BITMAP).contains(&key.kind) {
                    mem::push(&mut free, key)?;
                }
                cursor.advance()?;
            }
        }
        for key in free {
            self.delete(volume, tree::FREE_SPACE, &key)?;
        }
        self.space.remove_group(start)?;
        volume.remove_chunk(start)
    }

    /// Lets no new chunk go on disk `devid`, or lets them again, as a
    /// removal of the disk moves its chunks off it (ADR 0159).
    pub(crate) fn keep_chunks_off(&mut self, volume: &Volume, devid: u64, off: bool) -> Result<()> {
        if self.grown.devices.is_none() {
            self.grown.devices = Some(read_devices(volume)?);
        }
        let devices = self.grown.devices.as_mut().ok_or(Error::Corrupt)?;
        let device = devices.list.iter_mut().find(|d| d.devid == devid).ok_or(Error::NotFound)?;
        device.usable = !off && !volume.read_only();
        Ok(())
    }

    /// Lets new chunks go on disk `devid`, put in the place of a missing
    /// one and rebuilt (ADR 0155): the disks may have been counted while
    /// it was missing.
    pub(crate) fn disk_rebuilt(&mut self, volume: &Volume, devid: u64) {
        let devices = self.grown.devices.as_mut();
        if let Some(device) = devices.and_then(|d| d.list.iter_mut().find(|d| d.devid == devid)) {
            device.usable = !volume.read_only();
        }
    }

    /// Notes disk `devid`, of `total` bytes, added with no chunk on it yet.
    pub(crate) fn device_added(&mut self, devid: u64, total: u64) -> Result<()> {
        if let Some(devices) = self.grown.devices.as_mut() {
            mem::push(&mut devices.list, Device { devid, usable: true, total, used: mem::vec() })?;
        }
        self.grown.disks.0 += 1;
        self.grown.disks.1 += total as i64;
        Ok(())
    }

    /// Notes disk `devid`, of `total` bytes, removed, with no chunk left
    /// on it.
    pub(crate) fn device_removed(&mut self, devid: u64, total: u64) {
        if let Some(devices) = self.grown.devices.as_mut() {
            devices.list.retain(|d| d.devid != devid);
        }
        self.grown.disks.0 -= 1;
        self.grown.disks.1 -= total as i64;
    }
}

/// Takes the entry of the chunk at `start` out of the superblock `sb`'s
/// system chunk array.
fn drop_sys_chunk(sb: &mut [u8], start: u64) -> Result<()> {
    let size = crate::format::u32_at(sb, SYS_ARRAY_SIZE_AT) as usize;
    if size > SYS_ARRAY_MAX {
        return Err(Error::Corrupt);
    }
    let mut at = 0;
    while at < size {
        let key = Key::read(&sb[SYS_ARRAY_AT..], at);
        let stripes_at = SYS_ARRAY_AT + at + Key::SIZE + 44;
        let stripes = crate::format::u16_at(sb, stripes_at) as usize;
        let entry = Key::SIZE + Chunk::item_size(stripes);
        if at + entry > size {
            return Err(Error::Corrupt);
        }
        if key.kind == item::CHUNK_ITEM && key.offset == start {
            let from = SYS_ARRAY_AT + at;
            sb.copy_within(from + entry..SYS_ARRAY_AT + size, from);
            sb[SYS_ARRAY_AT + size - entry..SYS_ARRAY_AT + size].fill(0);
            let size = (size - entry) as u32;
            sb[SYS_ARRAY_SIZE_AT..SYS_ARRAY_SIZE_AT + 4].copy_from_slice(&size.to_le_bytes());
            return Ok(());
        }
        at += entry;
    }
    Ok(())
}

/// Reads each disk's size, from its device item, and the ranges its
/// device extents take.
#[inline(never)]
fn read_devices(volume: &Volume) -> Result<Devices> {
    let mut list = mem::vec_with_capacity(volume.disks().len())?;
    let mut chunk_tree_uuid = None;
    let read_only = volume.read_only();
    let chunk_root = volume.chunk_root();
    let dev_root = Writer::tree_root(volume, tree::DEV)?;
    for disk in volume.disks().iter().filter(|d| !d.gone()) {
        let key = Key::new(DEV_ITEMS, item::DEV_ITEM, disk.devid);
        let cursor = Cursor::seek(volume, &chunk_root, &key)?;
        let total = match cursor.get() {
            Some((found, data)) if found == key && data.len() >= DEV_TOTAL_AT + 8 => {
                u64_at(data, DEV_TOTAL_AT)
            }
            _ => return Err(Error::Corrupt),
        };
        let first = Key::first(disk.devid, item::DEV_EXTENT);
        let mut cursor = Cursor::seek(volume, &dev_root, &first)?;
        let mut used = mem::vec();
        while let Some((key, data)) = cursor.get() {
            if !key.is(disk.devid, item::DEV_EXTENT) {
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
        let usable = disk.present() && !read_only;
        mem::push(&mut list, Device { devid: disk.devid, usable, total, used })?;
    }
    let chunk_tree_uuid = chunk_tree_uuid.ok_or(Error::Corrupt)?;
    Ok(Devices { list, chunk_tree_uuid })
}

/// What [`pick_disks`] picks: each disk's place among the devices and
/// where its stripes start; the length of each stripe; and how many disks.
type Picked = ([(usize, u64); MAX_DISKS], u64, usize);

/// The disks a chunk of `layout` goes on, with room for at least `size`
/// bytes and, where they have it, `length`. The disks with the largest
/// free ranges come first, as Linux's `gather_device_info` sorts them.
fn pick_disks(devices: &Devices, layout: Layout, length: u64, size: u64) -> Result<Picked> {
    let pairs = |n: usize| if layout.paired() { n - n % 2 } else { n };
    let mut disks = pairs(devices.list.iter().filter(|d| d.usable).count().min(layout.max));
    while disks >= layout.min {
        let data = layout.data_stripes(disks);
        let least = size.div_ceil(data).next_multiple_of(STRIPE_LEN).max(MIN_STRIPE);
        let want = length.div_ceil(data).next_multiple_of(STRIPE_LEN).max(least);
        let per = layout.per_disk as u64;
        let mut found = [(0, 0, 0); MAX_DISKS];
        let mut n = 0;
        for (i, device) in devices.list.iter().enumerate().filter(|(_, d)| d.usable) {
            if let Ok((at, len)) = find_room(&device.used, device.total, want, least, per) {
                found[n] = (i, at, len);
                n += 1;
            }
        }
        // The largest first; the disks' own order else.
        let found = &mut found[..n];
        for i in 1..found.len() {
            let mut j = i;
            while j > 0 && found[j - 1].2 < found[j].2 {
                found.swap(j - 1, j);
                j -= 1;
            }
        }
        if n >= disks {
            let stripe = found[..disks].iter().map(|f| f.2).min().unwrap_or(0);
            let mut picked = [(0, 0); MAX_DISKS];
            for (slot, &(i, at, _)) in picked.iter_mut().zip(&found[..disks]) {
                *slot = (i, at);
            }
            return Ok((picked, stripe, disks));
        }
        // Fewer disks have room: a profile that may, tries with those.
        if layout.min == layout.max {
            break;
        }
        disks = pairs(n);
    }
    Err(Error::NoSpace)
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
