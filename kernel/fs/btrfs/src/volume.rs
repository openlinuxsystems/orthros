// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! A Btrfs filesystem's device, opened: its superblock, its chunks and
//! its root tree, from which every other tree is found.

use radix_block::DeviceRef;
use radix_block::cache::BlockCache;
use radix_sync::Mutex;
use radix_vfs::mem::KVec;
use radix_vfs::{Error, Result};

use crate::chunk::{Chunk, ChunkMap, Place};
use crate::format::{Key, RootItem, item, tree, u64_at};
use crate::superblock::{self, Superblock};
use crate::tree::{Blocks, Cursor, Root};

/// The object ID of every device item in the chunk tree.
const DEV_ITEMS: u64 = 1;

/// An opened Btrfs device.
pub struct Volume {
    sb: Superblock,
    /// The device, through a cache of its sectors.
    cache: BlockCache,
    /// Its size, in bytes.
    device_bytes: u64,
    chunks: ChunkMap,
    blocks: Blocks,
    current: Mutex<Current>,
}

/// What changes as transactions commit: the roots of the two trees the
/// superblock names, as the running transaction has them, and the
/// superblock last written.
struct Current {
    root: Root,
    chunk: Root,
    /// The superblock on the disk, as read or last written.
    superblock: KVec<u8>,
}

impl Volume {
    /// Opens the Btrfs filesystem on `device`: reads its superblock and
    /// its chunks.
    ///
    /// Fails with [`Error::UnknownFilesystem`] when the device holds no
    /// Btrfs, [`Error::Corrupt`] when what it holds is damaged, and
    /// [`Error::NotSupported`] for what Radix cannot read yet: a
    /// filesystem on more than one device, or one whose log of fsyncs has
    /// not been replayed (both Phase 10e and 10c).
    #[inline(never)]
    pub fn open(device: DeviceRef) -> Result<Volume> {
        let info = device.info();
        let device_bytes = info.sectors * radix_block::SECTOR;
        if device_bytes < superblock::OFFSET + superblock::SIZE as u64 {
            return Err(Error::UnknownFilesystem);
        }
        let cache = BlockCache::new(device);
        // On the heap: kernel stack frames are small.
        let mut bytes = crate::zeroed(superblock::SIZE)?;
        cache.read(superblock::OFFSET, &mut bytes)?;
        let sb = Superblock::read(&bytes)?;
        if sb.num_devices != 1 || sb.log_root != 0 {
            return Err(Error::NotSupported);
        }
        // The device item's size: a device smaller than the filesystem
        // says has lost its end.
        if u64_at(&bytes, 0xc9 + 8) > device_bytes {
            return Err(Error::Corrupt);
        }
        let chunks = ChunkMap::bootstrap(sb.sys_chunks(), sb.sectorsize)?;
        let blocks = Blocks::new(sb.nodesize);
        let current = Mutex::new(Current {
            root: Root { bytenr: sb.root, level: sb.root_level, generation: sb.generation },
            chunk: Root {
                bytenr: sb.chunk_root,
                level: sb.chunk_root_level,
                generation: sb.chunk_root_generation,
            },
            superblock: bytes,
        });
        let mut volume = Volume { sb, cache, device_bytes, chunks, blocks, current };
        volume.read_chunk_tree()?;
        Ok(volume)
    }

    /// Reads every chunk in the chunk tree, which the system chunk array
    /// only starts.
    #[inline(never)]
    fn read_chunk_tree(&mut self) -> Result<()> {
        let mut chunks = ChunkMap::empty();
        let mut devices = 0;
        let mut ours = false;
        {
            let root = self.chunk_root();
            let mut cursor = Cursor::seek(self, &root, &Key::new(0, 0, 0))?;
            while let Some((key, data)) = cursor.get() {
                if key.is(DEV_ITEMS, item::DEV_ITEM) {
                    devices += 1;
                    ours |= key.offset == self.sb.devid;
                } else if key.is(tree::FIRST_CHUNK, item::CHUNK_ITEM) {
                    chunks.insert(Chunk::read(key.offset, data, self.sb.sectorsize)?)?;
                }
                cursor.advance()?;
            }
        }
        if devices != self.sb.num_devices || !ours {
            return Err(Error::Corrupt);
        }
        // The system chunk array's chunks are in the tree too.
        let bootstrap = core::mem::replace(&mut self.chunks, ChunkMap::empty());
        for chunk in bootstrap.into_chunks() {
            chunks.insert(chunk)?;
        }
        self.chunks = chunks;
        // Its blocks were checked against the bootstrap chunks; they are
        // the same blocks.
        Ok(())
    }

    /// The superblock.
    pub fn superblock(&self) -> &Superblock {
        &self.sb
    }

    /// The chunks.
    pub fn chunks(&self) -> &ChunkMap {
        &self.chunks
    }

    /// The tree blocks read so far.
    pub fn blocks(&self) -> &Blocks {
        &self.blocks
    }

    /// The device's size, in bytes.
    pub fn device_bytes(&self) -> u64 {
        self.device_bytes
    }

    /// Where `logical` is on the device.
    pub fn place(&self, logical: u64) -> Result<Place> {
        self.chunks.place(logical, self.sb.devid)
    }

    /// Reads `out.len()` bytes from byte `physical` of the device.
    pub fn read_device(&self, physical: u64, out: &mut [u8]) -> Result<()> {
        let end = physical.checked_add(out.len() as u64).ok_or(Error::Corrupt)?;
        if end > self.device_bytes {
            return Err(Error::Corrupt);
        }
        Ok(self.cache.read(physical, out)?)
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

    /// The device, through its cache of sectors: writes go there.
    pub(crate) fn cache(&self) -> &BlockCache {
        &self.cache
    }

    /// The root block of tree `id` (a [`tree`] number, or a subvolume's),
    /// from its root item in the root tree; `None` if there is no such
    /// tree.
    pub fn tree(&self, id: u64) -> Result<Option<Root>> {
        let cursor = Cursor::seek(self, &self.root_tree(), &Key::first(id, item::ROOT_ITEM))?;
        let Some((key, data)) = cursor.get() else { return Ok(None) };
        if !key.is(id, item::ROOT_ITEM) {
            return Ok(None);
        }
        let root = RootItem::read(data).ok_or(Error::Corrupt)?;
        Ok(Some(Root { bytenr: root.bytenr, level: root.level, generation: root.generation }))
    }

    /// Subvolume `id`'s tree and its top directory's inode number; `None`
    /// if there is no such subvolume, or it was deleted and waits to be
    /// cleaned up (no references are left to it).
    pub fn subvolume(&self, id: u64) -> Result<Option<(Root, u64)>> {
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
        Ok(Some((tree, root.root_dirid)))
    }

    /// The device.
    pub fn device(&self) -> &DeviceRef {
        self.cache.device()
    }

    /// Keeps up to `limit` tree blocks read, for tests of a full cache.
    #[cfg(test)]
    pub fn limit_blocks(&mut self, limit: usize) {
        self.blocks = Blocks::with_limit(limit);
    }

    /// Drops up to `max` tree blocks and cached sectors nobody holds;
    /// returns how many.
    pub fn shrink(&self, max: usize) -> usize {
        let blocks = self.blocks.shrink(max);
        blocks + self.cache.shrink(max.saturating_sub(blocks))
    }
}
