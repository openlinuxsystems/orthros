// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! A Btrfs filesystem's disks, opened: its superblock, its chunks and
//! its root tree, from which every other tree is found.
//!
//! A filesystem may span several disks (ADR 0153). Each has a superblock
//! of its own, the same but for its device item, and the chunk tree has a
//! device item for each. The disks found are matched to those items; one
//! not found is missing, and its copies are left out of every [`Place`].

use core::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};

use radix_block::DeviceRef;
use radix_block::cache::BlockCache;
use radix_sync::rcu::RcuCell;
use radix_sync::{Mutex, Once};
use radix_vfs::mem::{self, Alloc, KBox, KVec};
use radix_vfs::{Error, Result};

use crate::chunk::{Chunk, ChunkMap, Place, Site};
use crate::format::{Key, RootItem, item, tree, u64_at, uuid_at};
use crate::superblock::{self, Superblock};
use crate::tree::{Blocks, Cursor, Root};

/// The object ID of every device item in the chunk tree.
pub(crate) const DEV_ITEMS: u64 = 1;

/// Where a superblock keeps its disk's device item.
pub(crate) const DEV_ITEM_AT: usize = 0xc9;

/// A device item's size, and where in it the UUID is.
pub(crate) const DEV_ITEM_SIZE: usize = 98;
const DEV_UUID_AT: usize = 66;

/// The most disks a filesystem may span here.
pub const MAX_DISKS: usize = 16;

/// One of a filesystem's disks.
pub struct Disk {
    /// Its ID within the filesystem.
    pub devid: u64,
    /// Its UUID.
    pub uuid: [u8; 16],
    /// The disk, through a cache of its sectors; `None` when it is
    /// missing.
    cache: Option<BlockCache>,
    /// The disk that took its place while mounted, which it is from then
    /// on (ADR 0155).
    replacement: Once<BlockCache>,
    /// Its size, in bytes.
    bytes: AtomicU64,
    /// The generation of its superblock.
    generation: u64,
    /// Its superblock is older than the filesystem's: it missed commits
    /// while it was away, so its copies are read last.
    behind: AtomicBool,
    /// It failed a write while mounted, and is left out from then on.
    failed: AtomicBool,
    /// It was removed from the filesystem while mounted (ADR 0159).
    gone: AtomicBool,
}

impl Disk {
    /// The disk, through its cache of sectors, if it is there.
    pub fn cache(&self) -> Option<&BlockCache> {
        if self.failed.load(Ordering::Relaxed) || self.gone() {
            return None;
        }
        self.replacement.get().or(self.cache.as_ref())
    }

    /// The disk itself, if it is there.
    pub fn device(&self) -> Option<&DeviceRef> {
        self.cache().map(BlockCache::device)
    }

    /// Its size, in bytes; 0 when it is missing.
    pub fn bytes(&self) -> u64 {
        self.bytes.load(Ordering::Relaxed)
    }

    /// Whether it is there: found at the mount, and not failed since.
    pub fn present(&self) -> bool {
        self.cache().is_some()
    }

    /// Whether it was removed from the filesystem while mounted.
    pub fn gone(&self) -> bool {
        self.gone.load(Ordering::Relaxed)
    }

    /// Whether it missed commits while it was away.
    pub fn behind(&self) -> bool {
        self.behind.load(Ordering::Relaxed)
    }

    /// Whether `len` bytes from byte `at` are on the disk.
    pub fn holds(&self, at: u64, len: u64) -> bool {
        at.checked_add(len).is_some_and(|end| end <= self.bytes())
    }

    /// The disk it was before [`Volume::replace`] put another in its
    /// place, if that one was there.
    pub(crate) fn replaced(&self) -> Option<&BlockCache> {
        self.replacement.get().and(self.cache.as_ref())
    }
}

/// An opened Btrfs filesystem's disks.
pub struct Volume {
    sb: Superblock,
    /// Every disk the chunk tree names, then [`MAX_DISKS`] in all with
    /// places for disks added while mounted (ADR 0159), each with its
    /// device ID and UUID: the first `shown` are the filesystem's.
    disks: KVec<Disk>,
    shown: AtomicUsize,
    /// Read without a lock; a new chunk replaces the map whole.
    chunks: RcuCell<ChunkMap, Alloc>,
    blocks: Blocks,
    current: Mutex<Current>,
    /// Its quotas, while a writable mount keeps them (ADR 0170), under a
    /// lock taken last.
    quotas: Mutex<Option<crate::quota::Quotas>>,
    /// Whether a quota group has a limit, which writes keep to.
    quota_limited: AtomicBool,
}

/// What changes as transactions commit: the roots of the two trees the
/// superblock names, as the running transaction has them, and the
/// superblock last written.
struct Current {
    root: Root,
    chunk: Root,
    /// The roots of the last few trees looked up, as their root items in
    /// the root tree give them: a change to the root tree forgets them.
    trees: [Option<(u64, Root)>; TREES],
    /// The superblock on the disk, as read or last written.
    superblock: KVec<u8>,
}

/// The trees whose roots [`Current`] keeps: a subvolume's, and those its
/// changes reach.
const TREES: usize = 4;

impl Volume {
    /// Opens the Btrfs filesystem on `device`, alone ([`Volume::open_disks`]).
    pub fn open(device: DeviceRef) -> Result<KBox<Volume>> {
        Volume::open_disks(&[device])
    }

    /// Opens the Btrfs filesystem on `devices[0]`, and on those of the
    /// others that hold the rest of it: reads their superblocks and its
    /// chunks. Its disks not among them are missing; [`Volume::missing`]
    /// says how many, and [`Volume::readable`] whether every chunk still
    /// has a copy.
    ///
    /// Fails with [`Error::UnknownFilesystem`] when `devices[0]` holds no
    /// Btrfs, [`Error::Corrupt`] when what it holds is damaged, and
    /// [`Error::NotSupported`] for a filesystem on more than
    /// [`MAX_DISKS`] disks. A log of fsyncs is the mount's to replay (ADR
    /// 0151).
    pub fn open_disks(devices: &[DeviceRef]) -> Result<KBox<Volume>> {
        // The chunk tree's reads sleep on the disk: this frame, below
        // them, holds only the box.
        let mut volume = Volume::boxed(devices)?;
        volume.read_chunk_tree()?;
        Ok(volume)
    }

    /// The volume of the disks among `devices` that hold the filesystem
    /// on `devices[0]`, on the heap, its chunks those of its superblock's
    /// system chunk array ([`Volume::open_disks`]).
    #[inline(never)]
    fn boxed(devices: &[DeviceRef]) -> Result<KBox<Volume>> {
        let first = devices.first().ok_or(Error::NotFound)?;
        let (sb, bytes) = read_superblock(first)?;
        if sb.num_devices == 0 || sb.num_devices > MAX_DISKS as u64 {
            return Err(Error::NotSupported);
        }
        let mut disks = mem::vec_with_capacity(sb.num_devices as usize)?;
        mem::push(&mut disks, Disk::found(first, &sb, &bytes)?)?;
        let mut newest = (sb, bytes);
        // A filesystem of one disk needs no other read.
        if newest.0.num_devices > 1 {
            Volume::find_disks(&devices[1..], &mut disks, &mut newest)?;
        }
        let (sb, bytes) = newest;
        let chunks =
            RcuCell::new_in(ChunkMap::bootstrap(sb.sys_chunks(), sb.sectorsize)?, mem::alloc())
                .map_err(|_| Error::NoMemory)?;
        let blocks = Blocks::new(sb.nodesize);
        let current = Mutex::new(Current {
            root: Root { bytenr: sb.root, level: sb.root_level, generation: sb.generation },
            chunk: Root {
                bytenr: sb.chunk_root,
                level: sb.chunk_root_level,
                generation: sb.chunk_root_generation,
            },
            trees: [None; TREES],
            superblock: bytes,
        });
        // On the heap at once: a volume is large, and the mount's frames
        // above this one need not each hold a copy.
        let shown = AtomicUsize::new(disks.len());
        let (quotas, quota_limited) = (Mutex::new(None), AtomicBool::new(false));
        Ok(mem::boxed(Volume { sb, disks, shown, chunks, blocks, current, quotas, quota_limited })?)
    }

    /// Adds to `disks` those of `devices` that hold the filesystem of the
    /// superblock `newest`, and makes it the newest of theirs. A frame of
    /// its own: the mount's stack is small.
    #[inline(never)]
    fn find_disks(
        devices: &[DeviceRef],
        disks: &mut KVec<Disk>,
        newest: &mut (Superblock, KVec<u8>),
    ) -> Result<()> {
        for device in devices {
            // A disk of another filesystem, or of none, is not one of ours.
            let Ok((other, bytes)) = read_superblock(device) else { continue };
            if other.fsid != newest.0.fsid || disks.len() == MAX_DISKS {
                continue;
            }
            let Ok(disk) = Disk::found(device, &other, &bytes) else { continue };
            match disks.iter_mut().find(|d| d.devid == disk.devid) {
                // Two disks of one ID, as a copy of one makes: the newer.
                Some(same) if same.generation >= disk.generation => continue,
                Some(same) => *same = disk,
                None => mem::push(disks, disk)?,
            }
            if other.generation > newest.0.generation {
                *newest = (other, bytes);
            }
        }
        Ok(())
    }

    /// Reads every chunk in the chunk tree, which the system chunk array
    /// only starts, and matches the disks found to its device items: a
    /// disk it has no item for is not one of the filesystem's any more,
    /// and an item no disk was found for is a missing disk.
    #[inline(never)]
    fn read_chunk_tree(&mut self) -> Result<()> {
        let mut chunks = ChunkMap::empty();
        let mut items: KVec<(u64, [u8; 16])> = mem::vec();
        {
            let root = self.chunk_root();
            let mut cursor = Cursor::seek(self, &root, &Key::new(0, 0, 0))?;
            while let Some((key, data)) = cursor.get() {
                if key.is(DEV_ITEMS, item::DEV_ITEM) {
                    if data.len() < DEV_ITEM_SIZE || items.len() >= MAX_DISKS {
                        return Err(Error::Corrupt);
                    }
                    mem::push(&mut items, (key.offset, uuid_at(data, DEV_UUID_AT)))?;
                } else if key.is(tree::FIRST_CHUNK, item::CHUNK_ITEM) {
                    insert_chunk(&mut chunks, key.offset, data, self.sb.sectorsize)?;
                }
                cursor.advance()?;
            }
        }
        self.adopt(chunks, &items)
    }

    /// Takes the chunks and device items `read_chunk_tree` read: in a
    /// frame of its own, apart from the reads, as disks and chunks are
    /// large.
    #[inline(never)]
    fn adopt(&mut self, mut chunks: ChunkMap, items: &[(u64, [u8; 16])]) -> Result<()> {
        let first = (self.disks[0].devid, self.disks[0].uuid);
        if items.len() as u64 != self.sb.num_devices || !items.contains(&first) {
            return Err(Error::Corrupt);
        }
        self.disks.retain(|d| items.contains(&(d.devid, d.uuid)));
        for &(devid, uuid) in items {
            if !self.disks.iter().any(|d| d.devid == devid) {
                mem::push(&mut self.disks, Disk::missing(devid, uuid))?;
            }
        }
        for disk in &mut self.disks {
            *disk.behind.get_mut() = disk.generation < self.sb.generation;
        }
        *self.shown.get_mut() = self.disks.len();
        // Places for disks added while mounted, past every device ID.
        let mut next = self.disks.iter().map(|d| d.devid).max().unwrap_or(0);
        while self.disks.len() < MAX_DISKS {
            next += 1;
            let mut uuid = [0; 16];
            radix_vfs::random(&mut uuid);
            // A random UUID, version 4, as Linux makes a disk's.
            uuid[6] = (uuid[6] & 0x0f) | 0x40;
            uuid[8] = (uuid[8] & 0x3f) | 0x80;
            mem::push(&mut self.disks, Disk::missing(next, uuid))?;
        }
        // The system chunk array's chunks are in the tree too.
        let bootstrap = core::mem::replace(self.chunks.get_mut(), ChunkMap::empty());
        for chunk in bootstrap.into_chunks() {
            chunks.insert(chunk)?;
        }
        *self.chunks.get_mut() = chunks;
        // Its blocks were checked against the bootstrap chunks; they are
        // the same blocks.
        Ok(())
    }

    /// The disks, the first the one the mount named; one removed while
    /// mounted is among them still, [`Disk::gone`].
    pub fn disks(&self) -> &[Disk] {
        &self.disks[..self.shown.load(Ordering::Acquire).min(self.disks.len())]
    }

    /// The disk at `index` among [`Volume::disks`].
    pub fn disk(&self, index: usize) -> Result<&Disk> {
        self.disks().get(index).ok_or(Error::Corrupt)
    }

    /// How many of the filesystem's disks are not there: missing at the
    /// mount, or failed since.
    pub fn missing(&self) -> usize {
        self.disks().iter().filter(|d| !d.present() && !d.gone()).count()
    }

    /// Adds `device` as a new disk of the filesystem, in the next place
    /// kept for one (ADR 0159), and returns its place, device ID and UUID.
    /// It holds nothing yet: the caller, which holds the transaction,
    /// adds its device item, and the commit writes it its superblock.
    ///
    /// Fails with [`Error::NotSupported`] once the filesystem has
    /// [`MAX_DISKS`] disks, counting those removed since the mount.
    pub(crate) fn add_disk(&self, device: &DeviceRef) -> Result<(usize, u64, [u8; 16])> {
        let index = self.shown.load(Ordering::Acquire);
        let disk = self.disks.get(index).ok_or(Error::NotSupported)?;
        let size = device.info().sectors * radix_block::SECTOR;
        let _ = disk.replacement.set(BlockCache::new(device.clone()));
        disk.bytes.store(size, Ordering::Relaxed);
        self.shown.store(index + 1, Ordering::Release);
        Ok((index, disk.devid, disk.uuid))
    }

    /// Marks disk `index` removed from the filesystem: no chunk is on it
    /// any more, and its superblock is not written again (ADR 0159). The
    /// caller holds the transaction. Returns the disk, through its cache,
    /// if it is there, for its superblocks to be wiped.
    pub(crate) fn remove_disk(&self, index: usize) -> Option<&BlockCache> {
        let disk = self.disks().get(index)?;
        let cache = disk.cache();
        disk.gone.store(true, Ordering::Relaxed);
        cache
    }

    /// Whether every chunk has a copy of each of its bytes on the disks
    /// that are there.
    pub fn readable(&self) -> bool {
        let present = |devid| self.disks().iter().any(|d| d.devid == devid && d.present());
        self.chunks.read(|map| map.chunks().iter().all(|c| c.readable(present)))
    }

    /// Takes a write to disk `index` failing with `error`. On a filesystem
    /// of several disks the disk is left out from then on, as the others
    /// hold copies of what it held, and the write is done if they still
    /// hold every chunk; it fails with [`Error::Io`] when they do not. On
    /// one disk, the write fails with `error`, and the disk stays.
    pub(crate) fn write_failed(&self, index: usize, error: Error) -> Result<()> {
        let disk = self.disk(index)?;
        if self.disks().iter().filter(|d| !d.gone()).count() == 1 {
            return Err(error);
        }
        disk.failed.store(true, Ordering::Relaxed);
        if self.readable() { Ok(()) } else { Err(Error::Io) }
    }

    /// Puts `device` in the place of disk `index`, missing or there, as a
    /// disk that has missed every commit: read last, its superblock left
    /// alone, until a scrub has written every copy it should hold (ADR
    /// 0155). The caller holds the transaction, which it committed first.
    ///
    /// Fails with [`Error::InUse`] when the disk was replaced already
    /// since the mount.
    pub(crate) fn replace(&self, index: usize, device: &DeviceRef) -> Result<()> {
        let disk = self.disk(index)?;
        if disk.replacement.get().is_some() {
            return Err(Error::InUse);
        }
        let size = device.info().sectors * radix_block::SECTOR;
        // Both disks hold what is in the size of the smaller, whichever a
        // write meanwhile finds.
        let old = if disk.present() { disk.bytes() } else { size };
        disk.bytes.store(old.min(size), Ordering::Relaxed);
        disk.behind.store(true, Ordering::Relaxed);
        // The transaction held, no other replace runs.
        let _ = disk.replacement.set(BlockCache::new(device.clone()));
        disk.bytes.store(size, Ordering::Relaxed);
        disk.failed.store(false, Ordering::Relaxed);
        Ok(())
    }

    /// Marks every disk as up to date, after a scrub found every copy good
    /// or wrote it over.
    pub(crate) fn caught_up(&self) {
        for disk in self.disks() {
            disk.behind.store(false, Ordering::Relaxed);
        }
    }

    /// The superblock.
    pub fn superblock(&self) -> &Superblock {
        &self.sb
    }

    /// Runs `f` on the chunks, which must not sleep.
    pub fn with_chunks<R>(&self, f: impl FnOnce(&ChunkMap) -> R) -> R {
        self.chunks.read(f)
    }

    /// Adds `chunk`, which a transaction made. It waits until no reader
    /// sees the old map, so it sleeps.
    pub(crate) fn add_chunk(&self, chunk: Chunk) -> Result<()> {
        let map = self.chunks.read(|map| map.with(chunk))?;
        self.chunks.replace(map).map_err(|_| Error::NoMemory)?;
        Ok(())
    }

    /// Drops the chunk at `start`, which a balance emptied (ADR 0159). It
    /// waits until no reader sees the old map, so it sleeps.
    pub(crate) fn remove_chunk(&self, start: u64) -> Result<()> {
        let map = self.chunks.read(|map| map.without(start))?;
        self.chunks.replace(map).map_err(|_| Error::NoMemory)?;
        Ok(())
    }

    /// The tree blocks read so far.
    pub fn blocks(&self) -> &Blocks {
        &self.blocks
    }

    /// Its quotas, as [`crate::quota`] keeps them.
    pub(crate) fn quotas(&self) -> &Mutex<Option<crate::quota::Quotas>> {
        &self.quotas
    }

    /// Whether a quota group has a limit.
    pub(crate) fn quota_limited(&self) -> &AtomicBool {
        &self.quota_limited
    }

    /// Where `logical` is on the disks that are there; copies on disks
    /// that missed commits come last.
    pub fn place(&self, logical: u64) -> Result<Place> {
        let disk_of = |devid| self.disks().iter().position(|d| d.devid == devid && d.present());
        let mut place = self.chunks.read(|map| map.place(logical, disk_of))?;
        let behind = |site: &Site| self.disks()[site.disk].behind();
        let copies = &mut place.copies[..place.count];
        for i in 1..copies.len() {
            let mut j = i;
            while j > 0 && behind(&copies[j - 1]) && !behind(&copies[j]) {
                copies.swap(j - 1, j);
                j -= 1;
            }
        }
        Ok(place)
    }

    /// Reads `out.len()` bytes at `site`, through its disk's cache.
    pub fn read_site(&self, site: Site, out: &mut [u8]) -> Result<()> {
        let disk = self.disk(site.disk)?;
        let cache = disk.cache().ok_or(Error::NoDevice)?;
        if !disk.holds(site.at, out.len() as u64) {
            return Err(Error::Corrupt);
        }
        Ok(cache.read(site.at, out)?)
    }

    /// Whether any disk that is there is read-only.
    pub fn read_only(&self) -> bool {
        self.disks().iter().filter_map(Disk::device).any(|d| d.info().read_only)
    }

    /// The bytes of its disks, as the last commit counts them: disks
    /// added and removed while mounted change it (ADR 0159).
    pub fn total_bytes(&self) -> u64 {
        u64_at(&self.current.lock().superblock, 0x70)
    }

    /// The chunk tree's root block.
    pub fn chunk_root(&self) -> Root {
        self.current.lock().chunk
    }

    /// The root tree's root block.
    pub fn root_tree(&self) -> Root {
        self.current.lock().root
    }

    /// Moves the chunk tree's root, in the running transaction.
    pub(crate) fn set_chunk_root(&self, root: Root) {
        self.current.lock().chunk = root;
    }

    /// Moves the root tree's root, in the running transaction.
    pub(crate) fn set_root_tree(&self, root: Root) {
        self.current.lock().root = root;
    }

    /// A copy of the superblock last written, or read.
    pub(crate) fn superblock_bytes(&self) -> Result<KVec<u8>> {
        Ok(radix_vfs::mem::copy(&self.current.lock().superblock)?)
    }

    /// Records the superblock a commit wrote.
    pub(crate) fn set_superblock_bytes(&self, bytes: KVec<u8>) {
        self.current.lock().superblock = bytes;
    }

    /// The root block of tree `id` (a [`tree`] number, or a subvolume's),
    /// from its root item in the root tree; `None` if there is no such
    /// tree.
    pub fn tree(&self, id: u64) -> Result<Option<Root>> {
        // Every change to a file looks its tree up, and more than once.
        if let Some(&(_, root)) = self.current.lock().trees.iter().flatten().find(|t| t.0 == id) {
            return Ok(Some(root));
        }
        let cursor = Cursor::seek(self, &self.root_tree(), &Key::first(id, item::ROOT_ITEM))?;
        let Some((key, data)) = cursor.get() else { return Ok(None) };
        if !key.is(id, item::ROOT_ITEM) {
            return Ok(None);
        }
        let item = RootItem::read(data).ok_or(Error::Corrupt)?;
        let root = Root { bytenr: item.bytenr, level: item.level, generation: item.generation };
        let mut current = self.current.lock();
        let trees = &mut current.trees;
        trees.rotate_right(1);
        trees[0] = Some((id, root));
        Ok(Some(root))
    }

    /// Forgets the trees' roots kept, as the root tree is about to change.
    /// The transaction's lock, held by every change and by every read of
    /// the trees on a writable mount, keeps a lookup from noting a root
    /// meanwhile.
    pub(crate) fn forget_trees(&self) {
        self.current.lock().trees = [None; TREES];
    }

    /// Subvolume `id`'s tree, its top directory's inode number and whether
    /// it is read-only; `None` if there is no such subvolume, or it was
    /// deleted and waits to be cleaned up (no references are left to it).
    pub fn subvolume(&self, id: u64) -> Result<Option<(Root, u64, bool)>> {
        if id != tree::FS && id < tree::FIRST_FREE {
            return Ok(None);
        }
        let cursor = Cursor::seek(self, &self.root_tree(), &Key::first(id, item::ROOT_ITEM))?;
        let Some((key, data)) = cursor.get() else { return Ok(None) };
        if !key.is(id, item::ROOT_ITEM) {
            return Ok(None);
        }
        let root = RootItem::read(data).ok_or(Error::Corrupt)?;
        if root.refs == 0 {
            return Ok(None);
        }
        let tree = Root { bytenr: root.bytenr, level: root.level, generation: root.generation };
        Ok(Some((tree, root.root_dirid, root.flags & RootItem::READ_ONLY != 0)))
    }

    /// Keeps up to `limit` tree blocks read, for tests of a full cache.
    #[cfg(test)]
    pub fn limit_blocks(&mut self, limit: usize) {
        self.blocks = Blocks::with_limit(limit);
    }

    /// Drops up to `max` tree blocks and cached sectors nobody holds;
    /// returns how many.
    pub fn shrink(&self, max: usize) -> usize {
        let mut dropped = self.blocks.shrink(max);
        for cache in self.disks().iter().filter_map(Disk::cache) {
            dropped += cache.shrink(max.saturating_sub(dropped));
        }
        dropped
    }
}

impl Disk {
    /// The disk `device`, whose superblock `bytes` read as `sb`.
    ///
    /// Fails with [`Error::Corrupt`] when the device is smaller than its
    /// device item says: it has lost its end.
    fn found(device: &DeviceRef, sb: &Superblock, bytes: &[u8]) -> Result<Disk> {
        let size = device.info().sectors * radix_block::SECTOR;
        if u64_at(bytes, DEV_ITEM_AT + 8) > size {
            return Err(Error::Corrupt);
        }
        Ok(Disk {
            devid: sb.devid,
            uuid: sb.dev_uuid,
            cache: Some(BlockCache::new(device.clone())),
            replacement: Once::new(),
            bytes: AtomicU64::new(size),
            generation: sb.generation,
            behind: AtomicBool::new(false),
            failed: AtomicBool::new(false),
            gone: AtomicBool::new(false),
        })
    }

    /// Disk `devid`, not found.
    fn missing(devid: u64, uuid: [u8; 16]) -> Disk {
        Disk {
            devid,
            uuid,
            cache: None,
            replacement: Once::new(),
            bytes: AtomicU64::new(0),
            generation: 0,
            behind: AtomicBool::new(false),
            failed: AtomicBool::new(false),
            gone: AtomicBool::new(false),
        }
    }
}

/// Adds the chunk at `logical` that the chunk item `data` describes to
/// `chunks`: in a frame of its own, apart from the chunk tree's reads, as
/// a chunk is large.
#[inline(never)]
fn insert_chunk(chunks: &mut ChunkMap, logical: u64, data: &[u8], sectorsize: u32) -> Result<()> {
    chunks.insert(Chunk::read(logical, data, sectorsize)?)
}

/// Reads the superblock of `device`, and its bytes.
pub(crate) fn read_superblock(device: &DeviceRef) -> Result<(Superblock, KVec<u8>)> {
    let size = device.info().sectors * radix_block::SECTOR;
    if size < superblock::OFFSET + superblock::SIZE as u64 {
        return Err(Error::UnknownFilesystem);
    }
    // On the heap: kernel stack frames are small.
    let mut bytes = crate::zeroed(superblock::SIZE)?;
    BlockCache::new(device.clone()).read(superblock::OFFSET, &mut bytes)?;
    Ok((Superblock::read(&bytes)?, bytes))
}
