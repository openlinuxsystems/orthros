// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Free space: which bytes of each block group a transaction may take
//! (ADR 0129).
//!
//! Each chunk is a block group, of data, of metadata or of the chunk tree
//! ("system"), with an item that counts the bytes it has allocated. The
//! free-space tree keeps each group's free ranges, as extents or as
//! bitmaps of sectors. The first write reads every group's free space
//! into memory. Taking a range takes it from memory at once; freeing one
//! puts it back once no tree on the disk can use it: at once for a block
//! the running transaction made, after the commit otherwise. Each
//! group's changes are kept in order, and a commit makes the free-space
//! tree and the group's item say the same.

use radix_vfs::mem::{self, KVec};
use radix_vfs::{Error, Result};

use crate::chunk::{MAX_COPIES, kind};
use crate::format::{Key, item, tree, u32_at, u64_at};
use crate::superblock::compat_ro;
use crate::tree::{Cursor, Root};
use crate::volume::Volume;

/// What a block group's item and its chunk say it holds.
const TYPE: u64 = kind::DATA | kind::METADATA | kind::SYSTEM;

/// The free-space info flag of a group kept as bitmaps.
const USING_BITMAPS: u32 = 1 << 0;

/// The superblock's copies, by where they are on the device; each keeps
/// a stripe's length of its block group from use, as on Linux.
const MIRRORS: [u64; 3] = [64 << 10, 64 << 20, 256 << 30];

/// The length of the range around each copy that is never allocated.
const MIRROR_GUARD: u64 = 64 << 10;

/// Ranges of bytes, `(start, end)`, sorted and apart.
pub(crate) type Ranges = KVec<(u64, u64)>;

/// Changes to free space, in order: each range, and whether it became
/// free.
pub(crate) type Log = KVec<(u64, u64, bool)>;

/// Adds `[start, end)` to `set`, joining the ranges it touches.
pub(crate) fn add(set: &mut Ranges, start: u64, end: u64) -> Result<()> {
    if start >= end {
        return Ok(());
    }
    // The first range that ends at `start` or after it.
    let first = set.partition_point(|r| r.1 < start);
    let mut last = first;
    let (mut s, mut e) = (start, end);
    while last < set.len() && set[last].0 <= end {
        s = s.min(set[last].0);
        e = e.max(set[last].1);
        last += 1;
    }
    if last == first {
        mem::insert(set, first, (s, e))?;
    } else {
        set[first] = (s, e);
        let len = set.len();
        set.copy_within(last.., first + 1);
        set.truncate(len - (last - first - 1));
    }
    Ok(())
}

/// Takes `[start, end)` out of `set`.
pub(crate) fn remove(set: &mut Ranges, start: u64, end: u64) -> Result<()> {
    if start >= end {
        return Ok(());
    }
    let mut at = set.partition_point(|r| r.1 <= start);
    while at < set.len() && set[at].0 < end {
        let (s, e) = set[at];
        if s < start && e > end {
            set[at] = (s, start);
            mem::insert(set, at + 1, (end, e))?;
            return Ok(());
        }
        if s < start {
            set[at].1 = start;
            at += 1;
        } else if e > end {
            set[at].0 = end;
            return Ok(());
        } else {
            set.remove(at);
        }
    }
    Ok(())
}

/// One block group's space.
pub(crate) struct Group {
    /// Its first logical address.
    pub start: u64,
    /// Its length.
    pub len: u64,
    /// What it holds and its profile ([`kind`]).
    pub flags: u64,
    /// What may be taken now.
    free: Ranges,
    /// The changes the free-space tree lacks, in order.
    log: Log,
    /// Bytes taken less bytes freed since its item was last written.
    used_delta: i64,
    /// Where the last range taken ended: the next is looked for after it.
    hint: u64,
    /// The bytes of `free`.
    free_bytes: u64,
    /// The bytes freed that may be taken once the commit is durable.
    pinned: u64,
}

/// Every block group's space.
pub struct Space {
    groups: KVec<Group>,
    /// Where block group items are: the extent tree, or the block group
    /// tree.
    group_tree: u64,
}

impl Space {
    /// Reads every block group and its free space, for a first write.
    ///
    /// Fails with [`Error::NotSupported`] for a filesystem whose free
    /// space Radix cannot keep yet: one without a valid free-space tree.
    pub fn load(volume: &Volume) -> Result<Space> {
        let sb = volume.superblock();
        let need = compat_ro::FREE_SPACE_TREE | compat_ro::FREE_SPACE_TREE_VALID;
        if sb.compat_ro_flags & need != need {
            return Err(Error::NotSupported);
        }
        let group_tree = if sb.compat_ro_flags & compat_ro::BLOCK_GROUP_TREE != 0 {
            tree::BLOCK_GROUP
        } else {
            tree::EXTENT
        };
        let mut space = Space { groups: read_groups(volume, group_tree)?, group_tree };
        let free_tree = volume.tree(tree::FREE_SPACE)?.ok_or(Error::Corrupt)?;
        for at in 0..space.groups.len() {
            let (start, len) = (space.groups[at].start, space.groups[at].len);
            let free = read_free(volume, &free_tree, start, len)?.0;
            let group = &mut space.groups[at];
            group.free = free;
            exclude_mirrors(volume, group)?;
            group.free_bytes = group.free.iter().map(|&(s, e)| e - s).sum();
        }
        Ok(space)
    }

    /// Adds the block group of a chunk just made, `[start, start + len)`
    /// with `flags`: free but around the superblock's copies. The commit
    /// writes its free space as it does every group's changes.
    pub(crate) fn add_group(
        &mut self,
        volume: &Volume,
        start: u64,
        len: u64,
        flags: u64,
    ) -> Result<()> {
        let mut group = Group {
            start,
            len,
            flags,
            free: mem::vec(),
            log: mem::vec(),
            used_delta: 0,
            hint: start,
            free_bytes: 0,
            pinned: 0,
        };
        add(&mut group.free, start, start + len)?;
        exclude_mirrors(volume, &mut group)?;
        group.free_bytes = group.free.iter().map(|&(s, e)| e - s).sum();
        for &(s, e) in &group.free {
            mem::push(&mut group.log, (s, e, true))?;
        }
        let at = self.groups.partition_point(|g| g.start < start);
        mem::insert(&mut self.groups, at, group)?;
        Ok(())
    }

    /// The bytes that may be taken now for `want`.
    pub(crate) fn free_bytes(&self, want: u64) -> u64 {
        self.groups.iter().filter(|g| g.flags & TYPE & want != 0).map(|g| g.free_bytes).sum()
    }

    /// The bytes for `want` that may be taken once the commit is durable.
    pub(crate) fn pinned_bytes(&self, want: u64) -> u64 {
        self.groups.iter().filter(|g| g.flags & TYPE & want != 0).map(|g| g.pinned).sum()
    }

    /// The flags of the first block group that holds `want`, which a new
    /// one for `want` takes; `None` if there is none.
    pub(crate) fn flags_for(&self, want: u64) -> Option<u64> {
        self.groups.iter().find(|g| g.flags & TYPE & want != 0).map(|g| g.flags)
    }

    /// The tree that holds block group items.
    pub(crate) fn group_tree(&self) -> u64 {
        self.group_tree
    }

    fn group_mut(&mut self, logical: u64) -> Result<&mut Group> {
        let at = self.groups.partition_point(|g| g.start + g.len <= logical);
        match self.groups.get_mut(at) {
            Some(g) if g.start <= logical => Ok(g),
            _ => Err(Error::Corrupt),
        }
    }

    /// Takes `size` bytes, aligned to `align`, from a block group that
    /// holds `want` (a [`kind`] of data, metadata or system): a group of
    /// both data and metadata (`mkfs.btrfs --mixed`) holds either.
    pub(crate) fn take(&mut self, want: u64, size: u64, align: u64) -> Result<u64> {
        for group in &mut self.groups {
            if group.flags & TYPE & want == 0 {
                continue;
            }
            let found = group.free.iter().find_map(|&(s, e)| {
                let at = s.max(group.hint).next_multiple_of(align);
                (at + size <= e).then_some(at)
            });
            let found = found.or_else(|| {
                group.free.iter().find_map(|&(s, e)| {
                    let at = s.next_multiple_of(align);
                    (at + size <= e).then_some(at)
                })
            });
            if let Some(at) = found {
                remove(&mut group.free, at, at + size)?;
                mem::push(&mut group.log, (at, at + size, false))?;
                group.free_bytes -= size;
                group.used_delta += size as i64;
                group.hint = at + size;
                return Ok(at);
            }
        }
        Err(Error::NoSpace)
    }

    /// Frees `[start, start + len)`, which no tree on the disk uses: it
    /// may be taken again at once.
    pub(crate) fn free_now(&mut self, start: u64, len: u64) -> Result<()> {
        let group = self.group_mut(start)?;
        add(&mut group.free, start, start + len)?;
        mem::push(&mut group.log, (start, start + len, true))?;
        group.free_bytes += len;
        group.used_delta -= len as i64;
        Ok(())
    }

    /// Frees `[start, start + len)`, which the tree on the disk may still
    /// use: the free-space tree has it free from this commit on, and it
    /// may be taken once this commit is durable ([`Space::unpin`]).
    pub(crate) fn free_pinned(&mut self, start: u64, len: u64) -> Result<()> {
        let group = self.group_mut(start)?;
        mem::push(&mut group.log, (start, start + len, true))?;
        group.pinned += len;
        group.used_delta -= len as i64;
        Ok(())
    }

    /// Lets a range freed by [`Space::free_pinned`] be taken again, its
    /// commit being durable.
    pub(crate) fn unpin(&mut self, start: u64, len: u64) -> Result<()> {
        let group = self.group_mut(start)?;
        add(&mut group.free, start, start + len)?;
        group.pinned = group.pinned.saturating_sub(len);
        group.free_bytes += len;
        Ok(())
    }

    /// The block groups whose items or free space trail what was taken
    /// and freed: their starts.
    pub(crate) fn changed(&self) -> Result<KVec<u64>> {
        let mut out = mem::vec();
        for g in &self.groups {
            if !g.log.is_empty() || g.used_delta != 0 {
                mem::push(&mut out, g.start)?;
            }
        }
        Ok(out)
    }

    /// Takes a group's changes since they were last written: its start,
    /// its length, the changes to its free space in order, and the change
    /// to its used bytes.
    pub(crate) fn take_changes(&mut self, start: u64) -> Result<(u64, Log, i64)> {
        let group = self.group_mut(start)?;
        let log = core::mem::replace(&mut group.log, mem::vec());
        let delta = core::mem::take(&mut group.used_delta);
        Ok((group.len, log, delta))
    }
}

/// The block groups the tree `group_tree` holds, with no free space yet.
/// A frame of its own, as each of a mount's reads has: a mount runs in a
/// system call, on a 16 KiB stack.
#[inline(never)]
fn read_groups(volume: &Volume, group_tree: u64) -> Result<KVec<Group>> {
    let mut groups = mem::vec();
    let tree = volume.tree(group_tree)?.ok_or(Error::Corrupt)?;
    let mut cursor = Cursor::seek(volume, &tree, &Key::new(0, 0, 0))?;
    while let Some((key, data)) = cursor.get() {
        if key.kind == item::BLOCK_GROUP_ITEM {
            let flags = u64_at(data, 16);
            let group = Group {
                start: key.objectid,
                len: key.offset,
                flags,
                free: mem::vec(),
                log: mem::vec(),
                used_delta: 0,
                hint: key.objectid,
                free_bytes: 0,
                pinned: 0,
            };
            mem::push(&mut groups, group)?;
        }
        cursor.advance()?;
    }
    Ok(groups)
}

/// The free ranges the free-space tree has for the group `[start, start
/// + len)`, and whether it keeps them as bitmaps.
pub(crate) fn read_free(
    volume: &Volume,
    free_tree: &Root,
    start: u64,
    len: u64,
) -> Result<(Ranges, bool)> {
    let sector = u64::from(volume.superblock().sectorsize);
    let info = Key::new(start, item::FREE_SPACE_INFO, len);
    let mut cursor = Cursor::seek(volume, free_tree, &info)?;
    let bitmaps = match cursor.get() {
        Some((key, data)) if key == info => u32_at(data, 4) & USING_BITMAPS != 0,
        _ => return Err(Error::Corrupt),
    };
    cursor.advance()?;
    let mut free = mem::vec();
    while let Some((key, data)) = cursor.get() {
        if key.objectid >= start + len {
            break;
        }
        match key.kind {
            item::FREE_SPACE_EXTENT => add(&mut free, key.objectid, key.objectid + key.offset)?,
            item::FREE_SPACE_BITMAP => {
                let bits = key.offset / sector;
                for bit in 0..bits {
                    let byte = data.get((bit / 8) as usize).copied().unwrap_or(0);
                    if byte & (1 << (bit % 8)) != 0 {
                        let at = key.objectid + bit * sector;
                        add(&mut free, at, at + sector)?;
                    }
                }
            }
            _ => break,
        }
        cursor.advance()?;
    }
    Ok((free, bitmaps))
}

/// Keeps the ranges around the superblock's copies that lie in `group`
/// from use.
#[inline(never)]
fn exclude_mirrors(volume: &Volume, group: &mut Group) -> Result<()> {
    let devid = volume.superblock().devid;
    // Found under the chunk map's read lock, which allows no allocation.
    let mut ranges = [(0, 0); MIRRORS.len() * MAX_COPIES];
    let mut n = 0;
    volume.with_chunks(|map| {
        let Some(chunk) = map.find(group.start) else { return Err(Error::Corrupt) };
        for mirror in MIRRORS {
            for stripe in chunk.stripes.iter().filter(|s| s.devid == devid) {
                let first = mirror;
                let end = mirror + MIRROR_GUARD;
                if end <= stripe.offset || first >= stripe.offset + chunk.length {
                    continue;
                }
                // Single and DUP keep the chunk whole on each stripe.
                let lo = chunk.start + first.saturating_sub(stripe.offset);
                let hi = chunk.start + (end - stripe.offset).min(chunk.length);
                if let Some(slot) = ranges.get_mut(n) {
                    *slot = (lo, hi);
                    n += 1;
                }
            }
        }
        Ok(())
    })?;
    for &(lo, hi) in &ranges[..n] {
        remove(&mut group.free, lo, hi)?;
    }
    Ok(())
}
