// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Btrfs's trees: copy-on-write B-trees of items sorted by [`Key`].
//!
//! A tree block (`nodesize` bytes) starts with a header: its checksum, the
//! filesystem's UUID, its own logical address, the transaction that
//! wrote it, the tree it belongs to, its item count and its level. A
//! leaf (level 0) holds items: a key, and where the item's data is in the
//! leaf; the data fills the leaf from its end. A node above holds key
//! pointers: the first key of a child block, its address, and the
//! transaction that wrote it.
//!
//! Every block read is checked before use, as Linux checks it: its
//! checksum, its address, its UUID, its level, the transaction its parent
//! expects (a block from another transaction means the disk lost a
//! write), and its items' order and bounds. A block that fails on one
//! copy is read from the next ([`crate::chunk::Place`]).

use radix_sync::Mutex;
use radix_vfs::mem::{self, KArc, KVec};
use radix_vfs::{Error, Result};

use crate::csum::CSUM_SIZE;
use crate::format::{Key, u32_at, u64_at, uuid_at};
use crate::volume::Volume;

/// The size of a tree block's header.
pub const HEADER_SIZE: usize = 101;

/// The size of an item in a leaf: its key, then where its data is.
pub(crate) const ITEM_SIZE: usize = Key::SIZE + 8;

/// The size of a key pointer in a node: a key, an address and a
/// transaction.
pub(crate) const PTR_SIZE: usize = Key::SIZE + 16;

/// The deepest a tree goes: levels 0 to 7.
pub const MAX_LEVEL: u8 = 8;

/// The keys a block must hold, as its parent says: its first key, and a
/// key after its last (the parent's next key pointer's).
#[derive(Clone, Copy, Debug, Default)]
pub struct Bounds {
    /// Its first key.
    pub first: Option<Key>,
    /// The key its keys are all before.
    pub next: Option<Key>,
}

impl Bounds {
    /// The bounds of child `slot` of `node`.
    pub fn of_child(node: &Block, slot: usize) -> Bounds {
        let next = slot + 1;
        Bounds {
            first: Some(node.key(slot)),
            next: (next < node.nritems()).then(|| node.key(next)),
        }
    }

    /// Whether `block`'s keys are within them.
    fn hold(&self, block: &Block) -> bool {
        let n = block.nritems();
        let first = self.first.is_none_or(|k| n > 0 && block.key(0) == k);
        let next = self.next.is_none_or(|k| n == 0 || block.key(n - 1) < k);
        first && next
    }
}

/// A tree's root block, as the superblock or a root item names it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Root {
    /// Its logical address.
    pub bytenr: u64,
    /// Its level.
    pub level: u8,
    /// The transaction that wrote it.
    pub generation: u64,
}

/// A tree block, checked.
pub struct Block {
    pub(crate) bytenr: u64,
    pub(crate) bytes: KVec<u8>,
}

impl Block {
    /// Its logical address.
    pub fn bytenr(&self) -> u64 {
        self.bytenr
    }

    /// Its level: 0 for a leaf.
    pub fn level(&self) -> u8 {
        self.bytes[0x64]
    }

    /// How many items or key pointers it holds.
    pub fn nritems(&self) -> usize {
        u32_at(&self.bytes, 0x60) as usize
    }

    /// The transaction that wrote it.
    pub fn generation(&self) -> u64 {
        u64_at(&self.bytes, 0x50)
    }

    /// The tree it belongs to.
    pub fn owner(&self) -> u64 {
        u64_at(&self.bytes, 0x58)
    }

    /// Whether it is a leaf.
    pub fn is_leaf(&self) -> bool {
        self.level() == 0
    }

    /// Where item or key pointer `slot` starts.
    pub(crate) fn entry(&self, slot: usize) -> usize {
        HEADER_SIZE + slot * if self.is_leaf() { ITEM_SIZE } else { PTR_SIZE }
    }

    /// The key of item or key pointer `slot`.
    pub fn key(&self, slot: usize) -> Key {
        Key::read(&self.bytes, self.entry(slot))
    }

    /// The data of item `slot`, in a leaf.
    pub fn item(&self, slot: usize) -> &[u8] {
        let at = self.entry(slot) + Key::SIZE;
        let start = HEADER_SIZE + u32_at(&self.bytes, at) as usize;
        let size = u32_at(&self.bytes, at + 4) as usize;
        &self.bytes[start..start + size]
    }

    /// The child of key pointer `slot`, in a node.
    pub fn child(&self, slot: usize) -> Root {
        let at = self.entry(slot) + Key::SIZE;
        Root {
            bytenr: u64_at(&self.bytes, at),
            level: self.level() - 1,
            generation: u64_at(&self.bytes, at + 8),
        }
    }

    /// The first slot whose key is `key` or after it; `nritems` if every
    /// key is before it.
    pub fn lower_bound(&self, key: &Key) -> usize {
        let size = if self.is_leaf() { ITEM_SIZE } else { PTR_SIZE };
        let (mut low, mut high) = (0, self.nritems());
        while low < high {
            let mid = low + (high - low) / 2;
            if Key::read(&self.bytes, HEADER_SIZE + mid * size) < *key {
                low = mid + 1;
            } else {
                high = mid;
            }
        }
        low
    }

    /// Checks a block read from the disk, before anything else reads it:
    /// its checksum, its address and UUID, that it is the block its parent
    /// expects, and that its items are in order and within it.
    fn check(&self, volume: &Volume, want: &Root, bounds: &Bounds) -> Result<()> {
        let sb = volume.superblock();
        let b = &self.bytes;
        let header_fits = sb.csum_type.matches(&b[CSUM_SIZE..], &b[..CSUM_SIZE])
            && u64_at(b, 0x30) == self.bytenr
            && uuid_at(b, 0x20) == sb.metadata_uuid
            && self.level() == want.level
            && self.generation() == want.generation;
        if !header_fits {
            return Err(Error::Corrupt);
        }
        let n = self.nritems();
        let room = b.len() - HEADER_SIZE;
        if self.is_leaf() {
            if n > room / ITEM_SIZE {
                return Err(Error::Corrupt);
            }
            // The data fills the leaf from its end, item 0's last, each
            // item's right before the one before it.
            let mut end = room;
            for slot in 0..n {
                let at = self.entry(slot) + Key::SIZE;
                let offset = u32_at(b, at) as usize;
                let size = u32_at(b, at + 4) as usize;
                if offset.checked_add(size) != Some(end) {
                    return Err(Error::Corrupt);
                }
                end = offset;
            }
            if HEADER_SIZE + n * ITEM_SIZE > HEADER_SIZE + end {
                return Err(Error::Corrupt);
            }
        } else {
            if n == 0 || n > room / PTR_SIZE {
                return Err(Error::Corrupt);
            }
            let sector = u64::from(sb.sectorsize);
            for slot in 0..n {
                let child = self.child(slot);
                if child.bytenr == 0 || !child.bytenr.is_multiple_of(sector) {
                    return Err(Error::Corrupt);
                }
            }
        }
        for slot in 1..n {
            if self.key(slot - 1) >= self.key(slot) {
                return Err(Error::Corrupt);
            }
        }
        // A node's key pointer names its child's first key, and the next
        // one a key after its last: a block that holds other keys is not
        // the one written there.
        if !bounds.hold(self) {
            return Err(Error::Corrupt);
        }
        Ok(())
    }
}

/// How many bytes of tree blocks are kept read and checked: 512 blocks of
/// the default 16 KiB.
const CACHE_BYTES: usize = 8 << 20;

/// Tree blocks read and checked, by address, so that walking a tree again
/// reads and checks each block once.
pub struct Blocks {
    blocks: Mutex<Cached>,
}

struct Cached {
    /// The blocks, by address, each with whether it was used since the
    /// clock hand last passed it.
    entries: KVec<(u64, KArc<Block>, bool)>,
    /// The running transaction's blocks, by address: never dropped, and
    /// found before `entries` (ADR 0129).
    dirty: KVec<(u64, KArc<Block>)>,
    hand: usize,
    /// The most blocks kept.
    limit: usize,
}

impl Blocks {
    /// An empty cache of blocks of `nodesize` bytes.
    pub fn new(nodesize: u32) -> Blocks {
        Blocks::with_limit(CACHE_BYTES / nodesize.max(1) as usize)
    }

    /// An empty cache of up to `limit` blocks.
    pub fn with_limit(limit: usize) -> Blocks {
        let limit = limit.max(1);
        let cached = Cached { entries: mem::vec(), dirty: mem::vec(), hand: 0, limit };
        Blocks { blocks: Mutex::new(cached) }
    }

    fn get(&self, bytenr: u64) -> Option<KArc<Block>> {
        let mut cached = self.blocks.lock();
        if let Ok(at) = cached.dirty.binary_search_by_key(&bytenr, |e| e.0) {
            return Some(cached.dirty[at].1.clone());
        }
        let at = cached.entries.binary_search_by_key(&bytenr, |e| e.0).ok()?;
        let entry = &mut cached.entries[at];
        entry.2 = true;
        Some(entry.1.clone())
    }

    /// Adds a block of the running transaction, or puts back one taken.
    pub(crate) fn put_dirty(&self, block: KArc<Block>) -> Result<()> {
        let mut cached = self.blocks.lock();
        // A clean copy at the same address is older: a block freed by a
        // commit and taken again.
        if let Ok(at) = cached.entries.binary_search_by_key(&block.bytenr, |e| e.0) {
            cached.entries.remove(at);
        }
        match cached.dirty.binary_search_by_key(&block.bytenr, |e| e.0) {
            Ok(at) => cached.dirty[at].1 = block,
            Err(at) => mem::insert(&mut cached.dirty, at, (block.bytenr, block))?,
        }
        Ok(())
    }

    /// Takes the running transaction's block at `bytenr` out, to change
    /// it: `None` if it has none there.
    pub(crate) fn take_dirty(&self, bytenr: u64) -> Option<KArc<Block>> {
        let mut cached = self.blocks.lock();
        let at = cached.dirty.binary_search_by_key(&bytenr, |e| e.0).ok()?;
        Some(cached.dirty.remove(at).1)
    }

    /// Runs `f` on the running transaction's block at `bytenr`, to change
    /// it. Fails with [`Error::InUse`] while a reader holds it.
    pub(crate) fn edit<R>(&self, bytenr: u64, f: impl FnOnce(&mut Block) -> R) -> Result<R> {
        let mut cached = self.blocks.lock();
        let at = cached.dirty.binary_search_by_key(&bytenr, |e| e.0).map_err(|_| Error::Corrupt)?;
        let block = KArc::get_mut(&mut cached.dirty[at].1).ok_or(Error::InUse)?;
        Ok(f(block))
    }

    /// Runs `f` on the running transaction's block at `bytenr`.
    pub(crate) fn view<R>(&self, bytenr: u64, f: impl FnOnce(&Block) -> R) -> Result<R> {
        let cached = self.blocks.lock();
        let at = cached.dirty.binary_search_by_key(&bytenr, |e| e.0).map_err(|_| Error::Corrupt)?;
        Ok(f(&cached.dirty[at].1))
    }

    /// Makes room for `more` blocks of the running transaction.
    pub(crate) fn reserve_dirty(&self, more: usize) -> Result<()> {
        let mut cached = self.blocks.lock();
        cached.dirty.try_reserve(more).map_err(|_| Error::NoMemory)
    }

    /// Whether the running transaction has a block at `bytenr`.
    pub(crate) fn is_dirty(&self, bytenr: u64) -> bool {
        self.blocks.lock().dirty.binary_search_by_key(&bytenr, |e| e.0).is_ok()
    }

    /// Takes every block of the running transaction out, by address.
    pub(crate) fn take_all_dirty(&self) -> KVec<(u64, KArc<Block>)> {
        core::mem::replace(&mut self.blocks.lock().dirty, mem::vec())
    }

    fn put(&self, block: &KArc<Block>) {
        let mut cached = self.blocks.lock();
        let Err(mut at) = cached.entries.binary_search_by_key(&block.bytenr, |e| e.0) else {
            return;
        };
        if cached.entries.len() >= cached.limit
            && let Some(victim) = cached.victim()
        {
            cached.entries.remove(victim);
            if victim < at {
                at -= 1;
            }
        }
        if cached.entries.len() < cached.limit {
            let _ = mem::insert(&mut cached.entries, at, (block.bytenr, block.clone(), false));
        }
    }

    /// Keeps a block a commit wrote, as if read.
    pub(crate) fn put_clean(&self, block: &KArc<Block>) {
        self.put(block);
    }

    /// Drops up to `max` blocks nobody holds; returns how many.
    pub fn shrink(&self, max: usize) -> usize {
        let mut cached = self.blocks.lock();
        let before = cached.entries.len();
        let mut left = max;
        cached.entries.retain(|e| {
            if left > 0 && KArc::strong_count(&e.1) == 1 {
                left -= 1;
                false
            } else {
                true
            }
        });
        cached.hand = 0;
        before - cached.entries.len()
    }
}

impl Cached {
    /// The next block to drop, by the clock: one not used since the hand
    /// last passed it, and that nobody holds.
    fn victim(&mut self) -> Option<usize> {
        let len = self.entries.len();
        for _ in 0..2 * len {
            let at = self.hand % len;
            self.hand = at + 1;
            let entry = &mut self.entries[at];
            if core::mem::take(&mut entry.2) {
                continue;
            }
            if KArc::strong_count(&entry.1) == 1 {
                return Some(at);
            }
        }
        None
    }
}

/// Reads the tree block `want` names and checks it, with the keys its
/// parent says it holds.
pub fn read_block(volume: &Volume, want: &Root, bounds: &Bounds) -> Result<KArc<Block>> {
    if want.level >= MAX_LEVEL {
        return Err(Error::Corrupt);
    }
    if let Some(block) = volume.blocks().get(want.bytenr) {
        if block.level() != want.level
            || block.generation() != want.generation
            || !bounds.hold(&block)
        {
            return Err(Error::Corrupt);
        }
        return Ok(block);
    }
    let nodesize = volume.superblock().nodesize as usize;
    let place = volume.place(want.bytenr)?;
    if (place.len as usize) < nodesize {
        return Err(Error::Corrupt);
    }
    let mut failed = Error::Corrupt;
    for &physical in place.copies() {
        let mut bytes = crate::zeroed(nodesize)?;
        if let Err(e) = volume.read_device(physical, &mut bytes) {
            failed = e;
            continue;
        }
        let block = Block { bytenr: want.bytenr, bytes };
        match block.check(volume, want, bounds) {
            Ok(()) => {
                let block = mem::arc(block)?;
                volume.blocks().put(&block);
                return Ok(block);
            }
            Err(e) => failed = e,
        }
    }
    Err(failed)
}

/// What is wrong with a copy of a tree block a scrub reads.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Fault {
    /// Its checksum differs.
    Sum,
    /// Its checksum matches, but its header or its items do not hold.
    Header,
}

/// Checks the copy `bytes` of the tree block `want` names, read from the
/// disk, as [`read_block`] checks one, with no parent to say its keys;
/// gives the bytes back.
pub(crate) fn check_copy(
    volume: &Volume,
    want: &Root,
    bytes: KVec<u8>,
) -> (Option<Fault>, KVec<u8>) {
    let sb = volume.superblock();
    if bytes.len() < HEADER_SIZE || !sb.csum_type.matches(&bytes[CSUM_SIZE..], &bytes[..CSUM_SIZE])
    {
        return (Some(Fault::Sum), bytes);
    }
    let block = Block { bytenr: want.bytenr, bytes };
    let fault = block.check(volume, want, &Bounds::default()).err().map(|_| Fault::Header);
    (fault, block.bytes)
}

/// A place in a tree: the blocks from the root down to a leaf, and the
/// slot in each.
pub struct Cursor<'v> {
    volume: &'v Volume,
    path: [Option<(KArc<Block>, usize)>; MAX_LEVEL as usize],
    /// The leaf's level in `path`: the tree's root level, since `path[0]`
    /// is the leaf and `path[level]` the root.
    top: usize,
}

impl<'v> Cursor<'v> {
    /// A cursor at the first item of the tree `root` whose key is `key`
    /// or after it; past the end if there is none.
    pub fn seek(volume: &'v Volume, root: &Root, key: &Key) -> Result<Cursor<'v>> {
        let mut cursor = Cursor::descend(volume, root, key)?;
        cursor.settle()?;
        Ok(cursor)
    }

    /// A cursor at the last item of the tree `root` whose key is `key` or
    /// before it; `None` if there is none.
    pub fn seek_last(volume: &'v Volume, root: &Root, key: &Key) -> Result<Option<Cursor<'v>>> {
        let mut cursor = Cursor::descend(volume, root, key)?;
        let Some((leaf, slot)) = &mut cursor.path[0] else { return Ok(None) };
        if *slot < leaf.nritems() && leaf.key(*slot) == *key {
            return Ok(Some(cursor));
        }
        // The leaf's first key is its parent's key pointer's, which is
        // `key` or before it unless every key in the tree is after it.
        if *slot == 0 {
            return Ok(None);
        }
        *slot -= 1;
        Ok(Some(cursor))
    }

    /// A cursor at the slot of the leaf where `key` is or would be: down
    /// each node's last child whose first key is `key` or before it.
    fn descend(volume: &'v Volume, root: &Root, key: &Key) -> Result<Cursor<'v>> {
        let mut cursor = Cursor { volume, path: Default::default(), top: usize::from(root.level) };
        let mut block = read_block(volume, root, &Bounds::default())?;
        loop {
            if block.is_leaf() {
                let slot = block.lower_bound(key);
                cursor.path[0] = Some((block, slot));
                break;
            }
            // The last child whose first key is `key` or before it.
            let slot = block.lower_bound(key);
            let slot = if slot < block.nritems() && block.key(slot) == *key {
                slot
            } else {
                slot.saturating_sub(1)
            };
            let child = block.child(slot);
            let bounds = Bounds::of_child(&block, slot);
            let level = usize::from(block.level());
            cursor.path[level] = Some((block, slot));
            block = read_block(volume, &child, &bounds)?;
        }
        Ok(cursor)
    }

    /// Moves past the end of a leaf to the start of the next one.
    fn settle(&mut self) -> Result<()> {
        loop {
            let Some((leaf, slot)) = &self.path[0] else { return Ok(()) };
            if *slot < leaf.nritems() {
                return Ok(());
            }
            // Up to the first node with a child after the one taken.
            let mut level = 1;
            loop {
                if level > self.top {
                    // Past the tree's last item.
                    self.path[0] = None;
                    return Ok(());
                }
                let Some((node, slot)) = &mut self.path[level] else {
                    return Err(Error::Corrupt);
                };
                if *slot + 1 < node.nritems() {
                    *slot += 1;
                    break;
                }
                level += 1;
            }
            // And down its first children to a leaf.
            while level > 0 {
                let Some((node, slot)) = &self.path[level] else {
                    return Err(Error::Corrupt);
                };
                let child = node.child(*slot);
                let bounds = Bounds::of_child(node, *slot);
                let block = read_block(self.volume, &child, &bounds)?;
                level -= 1;
                self.path[level] = Some((block, 0));
            }
        }
    }

    /// The item the cursor is at: its key and data; `None` past the end.
    pub fn get(&self) -> Option<(Key, &[u8])> {
        let (leaf, slot) = self.path[0].as_ref()?;
        Some((leaf.key(*slot), leaf.item(*slot)))
    }

    /// The key of the item the cursor is at.
    pub fn key(&self) -> Option<Key> {
        let (leaf, slot) = self.path[0].as_ref()?;
        Some(leaf.key(*slot))
    }

    /// Moves to the next item.
    pub fn advance(&mut self) -> Result<()> {
        if let Some((_, slot)) = &mut self.path[0] {
            *slot += 1;
        }
        self.settle()
    }
}

/// The item of tree `root` whose key is `key`: a copy of its data.
pub fn find(volume: &Volume, root: &Root, key: &Key) -> Result<Option<KVec<u8>>> {
    let cursor = Cursor::seek(volume, root, key)?;
    match cursor.get() {
        Some((found, data)) if found == *key => Ok(Some(mem::copy(data)?)),
        _ => Ok(None),
    }
}
