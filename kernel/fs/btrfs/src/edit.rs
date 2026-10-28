// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Changing tree blocks in memory: the edits a transaction makes to the
//! blocks it owns before its commit writes them (ADR 0129).
//!
//! A leaf's items are their keys, in order, from the header on; their
//! data fills the leaf from its end, item 0's last, each item's right
//! before the one before it. Every edit keeps that layout, moving only
//! the data of the items after the one it changes, as [`Block::check`]
//! expects of a block read back.
//!
//! [`Block::check`]: crate::tree

use crate::csum::{CSUM_SIZE, CsumType};
use crate::format::{Key, u32_at, u64_at};
use crate::tree::{Block, HEADER_SIZE, ITEM_SIZE, PTR_SIZE, Root};

/// The header flag of a block that has been written: it is copied before
/// it changes again.
pub const WRITTEN: u64 = 1 << 0;

/// The back reference revision of every block Radix writes, in the top
/// byte of the header's flags: "mixed" back references, as Linux writes.
const MIXED_BACKREF_REV: u64 = 1 << 56;

/// Where the header keeps its fields.
mod at {
    pub const BYTENR: usize = 0x30;
    pub const FLAGS: usize = 0x38;
    pub const GENERATION: usize = 0x50;
    pub const OWNER: usize = 0x58;
    pub const NRITEMS: usize = 0x60;
    pub const LEVEL: usize = 0x64;
}

fn put_u32(bytes: &mut [u8], at: usize, value: u32) {
    bytes[at..at + 4].copy_from_slice(&value.to_le_bytes());
}

fn put_u64(bytes: &mut [u8], at: usize, value: u64) {
    bytes[at..at + 8].copy_from_slice(&value.to_le_bytes());
}

fn put_key(bytes: &mut [u8], at: usize, key: &Key) {
    put_u64(bytes, at, key.objectid);
    bytes[at + 8] = key.kind;
    put_u64(bytes, at + 9, key.offset);
}

impl Block {
    /// A block of `bytes`, at `bytenr`.
    pub(crate) fn from_bytes(bytenr: u64, bytes: radix_vfs::mem::KVec<u8>) -> Block {
        Block { bytenr, bytes }
    }

    /// Its bytes.
    pub(crate) fn bytes(&self) -> &[u8] {
        &self.bytes
    }

    /// Its header's flags.
    pub fn flags(&self) -> u64 {
        u64_at(&self.bytes, at::FLAGS)
    }

    /// Makes it the block at `bytenr` of transaction `generation` in tree
    /// `owner`, at `level`, not yet written. The rest of its header (the
    /// filesystem's and the chunk tree's UUIDs) stays as it was.
    pub(crate) fn set_header(&mut self, bytenr: u64, generation: u64, owner: u64, level: u8) {
        self.bytenr = bytenr;
        let b = &mut self.bytes;
        put_u64(b, at::BYTENR, bytenr);
        put_u64(b, at::FLAGS, MIXED_BACKREF_REV);
        put_u64(b, at::GENERATION, generation);
        put_u64(b, at::OWNER, owner);
        b[at::LEVEL] = level;
    }

    fn set_nritems(&mut self, n: usize) {
        put_u32(&mut self.bytes, at::NRITEMS, n as u32);
    }

    /// Marks it written and fills in its checksum, as the disk keeps it.
    pub(crate) fn seal(&mut self, csum: CsumType) {
        let flags = self.flags() | WRITTEN;
        put_u64(&mut self.bytes, at::FLAGS, flags);
        let sum = csum.sum(&self.bytes[CSUM_SIZE..]);
        self.bytes[..CSUM_SIZE].copy_from_slice(&sum);
    }

    /// The room after the header.
    fn room(&self) -> usize {
        self.bytes.len() - HEADER_SIZE
    }

    /// Where item `slot`'s data starts, after the header.
    fn data_at(&self, slot: usize) -> usize {
        u32_at(&self.bytes, self.entry(slot) + Key::SIZE) as usize
    }

    /// The size of item `slot`'s data.
    pub(crate) fn item_size(&self, slot: usize) -> usize {
        u32_at(&self.bytes, self.entry(slot) + Key::SIZE + 4) as usize
    }

    /// Sets the data offset of each item from `slot` to `end` of a leaf
    /// to `f` of it, as an edit moves their data: in one pass over their
    /// headers, without a bounds check for each.
    fn move_data_at(&mut self, slot: usize, end: usize, f: impl Fn(usize) -> usize) {
        if slot >= end {
            return;
        }
        let (from, to) = (self.entry(slot), self.entry(end));
        let Some(headers) = self.bytes.get_mut(from..to) else { return };
        for header in headers.as_chunks_mut::<ITEM_SIZE>().0 {
            let at = &mut header[Key::SIZE..Key::SIZE + 4];
            let offset = u32::from_le_bytes([at[0], at[1], at[2], at[3]]) as usize;
            at.copy_from_slice(&(f(offset) as u32).to_le_bytes());
        }
    }

    /// Where the data of item `slot - 1` starts: the end of item `slot`'s.
    fn top(&self, slot: usize) -> usize {
        if slot == 0 { self.room() } else { self.data_at(slot - 1) }
    }

    /// The bytes a leaf's items take, with their keys.
    pub(crate) fn leaf_used(&self) -> usize {
        let n = self.nritems();
        n * ITEM_SIZE + self.room() - self.top(n)
    }

    /// The bytes a leaf has free.
    pub(crate) fn leaf_free(&self) -> usize {
        self.room() - self.leaf_used()
    }

    /// The most bytes of data one item may have.
    pub(crate) fn max_item(nodesize: u32) -> usize {
        nodesize as usize - HEADER_SIZE - ITEM_SIZE
    }

    /// The most key pointers a node holds.
    pub(crate) fn max_ptrs(nodesize: u32) -> usize {
        (nodesize as usize - HEADER_SIZE) / PTR_SIZE
    }

    /// The data of item `slot`, to change.
    pub(crate) fn item_mut(&mut self, slot: usize) -> &mut [u8] {
        let start = HEADER_SIZE + self.data_at(slot);
        let size = self.item_size(slot);
        &mut self.bytes[start..start + size]
    }

    /// Sets the key of item or key pointer `slot`.
    pub(crate) fn set_key(&mut self, slot: usize, key: &Key) {
        let at = self.entry(slot);
        put_key(&mut self.bytes, at, key);
    }

    /// Makes room for an item of `size` bytes at `slot` of a leaf with
    /// [`Block::leaf_free`] of at least `size + ITEM_SIZE`, and returns
    /// its data, zeroed.
    pub(crate) fn insert_item(&mut self, slot: usize, key: &Key, size: usize) -> &mut [u8] {
        let n = self.nritems();
        let top = self.top(slot);
        let end = self.top(n);
        // The data of the items from `slot` on moves down by `size`.
        let h = HEADER_SIZE;
        self.bytes.copy_within(h + end..h + top, h + end - size);
        self.move_data_at(slot, n, |offset| offset - size);
        let from = self.entry(slot);
        let to = self.entry(n);
        self.bytes.copy_within(from..to, from + ITEM_SIZE);
        put_key(&mut self.bytes, from, key);
        put_u32(&mut self.bytes, from + Key::SIZE, (top - size) as u32);
        put_u32(&mut self.bytes, from + Key::SIZE + 4, size as u32);
        self.set_nritems(n + 1);
        let data = &mut self.bytes[h + top - size..h + top];
        data.fill(0);
        data
    }

    /// Removes `count` items from `slot` of a leaf.
    pub(crate) fn delete_items(&mut self, slot: usize, count: usize) {
        if count == 0 {
            return;
        }
        let n = self.nritems();
        let top = self.top(slot);
        let low = self.data_at(slot + count - 1);
        let gone = top - low;
        let end = self.top(n);
        let h = HEADER_SIZE;
        self.bytes.copy_within(h + end..h + low, h + end + gone);
        self.move_data_at(slot + count, n, |offset| offset + gone);
        let from = self.entry(slot + count);
        let to = self.entry(n);
        let dst = self.entry(slot);
        self.bytes.copy_within(from..to, dst);
        self.set_nritems(n - count);
    }

    /// Changes the size of item `slot` of a leaf to `size`, keeping the
    /// start of its data; new bytes are zero. Growing needs the leaf to
    /// have that many bytes free.
    pub(crate) fn resize_item(&mut self, slot: usize, size: usize) {
        let n = self.nritems();
        let old = self.item_size(slot);
        let at = self.data_at(slot);
        let end = self.top(n);
        let h = HEADER_SIZE;
        if size > old {
            let more = size - old;
            // This item's data and the data after it move down; the item
            // then ends where it ended.
            self.bytes.copy_within(h + end..h + at + old, h + end - more);
            self.move_data_at(slot, n, |offset| offset - more);
            let start = h + at - more + old;
            self.bytes[start..start + more].fill(0);
        } else {
            let less = old - size;
            self.bytes.copy_within(h + end..h + at + size, h + end + less);
            self.move_data_at(slot, n, |offset| offset + less);
        }
        let entry = self.entry(slot);
        put_u32(&mut self.bytes, entry + Key::SIZE + 4, size as u32);
    }

    /// Appends the `count` items of `src` from `start` to the end of this
    /// leaf, which has the room.
    pub(crate) fn append_items(&mut self, src: &Block, start: usize, count: usize) {
        for slot in start..start + count {
            let n = self.nritems();
            let data = src.item(slot);
            self.insert_item(n, &src.key(slot), data.len()).copy_from_slice(data);
        }
    }

    /// Inserts a key pointer to `child`, whose first key is `key`, at
    /// `slot` of a node that has room for it.
    pub(crate) fn insert_ptr(&mut self, slot: usize, key: &Key, child: &Root) {
        let n = self.nritems();
        let from = self.entry(slot);
        let to = self.entry(n);
        self.bytes.copy_within(from..to, from + PTR_SIZE);
        self.set_nritems(n + 1);
        self.set_key(slot, key);
        self.set_child(slot, child);
    }

    /// Removes `count` key pointers from `slot` of a node.
    pub(crate) fn delete_ptrs(&mut self, slot: usize, count: usize) {
        let n = self.nritems();
        let from = self.entry(slot + count);
        let to = self.entry(n);
        let dst = self.entry(slot);
        self.bytes.copy_within(from..to, dst);
        self.set_nritems(n - count);
    }

    /// Points key pointer `slot` of a node at `child`.
    pub(crate) fn set_child(&mut self, slot: usize, child: &Root) {
        let at = self.entry(slot) + Key::SIZE;
        put_u64(&mut self.bytes, at, child.bytenr);
        put_u64(&mut self.bytes, at + 8, child.generation);
    }

    /// Appends the `count` key pointers of `src` from `start` to the end of
    /// this node, which has the room.
    pub(crate) fn append_ptrs(&mut self, src: &Block, start: usize, count: usize) {
        let n = self.nritems();
        let from = src.entry(start);
        let len = count * PTR_SIZE;
        let to = self.entry(n);
        self.bytes[to..to + len].copy_from_slice(&src.bytes[from..from + len]);
        self.set_nritems(n + count);
    }

    /// Empties it of items or key pointers.
    pub(crate) fn clear(&mut self) {
        self.set_nritems(0);
        self.bytes[HEADER_SIZE..].fill(0);
    }

    /// The level and generation its parent's key pointer names it by.
    pub(crate) fn root(&self) -> Root {
        Root { bytenr: self.bytenr, level: self.level(), generation: self.generation() }
    }
}
