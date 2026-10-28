// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Taking and freeing blocks and inodes, through the groups' bitmaps.
//!
//! A run of blocks is taken from the first free one at or after a goal
//! (the block after the file's last, or its inode's group), in the goal's
//! group and then the ones after it, as long as the run is free, up to
//! what was asked; so a file written in order gets its blocks in order,
//! as Linux's allocator gives them. Blocks the running transaction freed
//! are skipped (see [`crate::fs`]). An inode is taken in its directory's
//! group, or, for a directory under the root, in groups in turn, so that
//! the root's directories spread over the volume as with Linux's Orlov
//! allocator. A group whose bitmap was never written gets one first.

use radix_vfs::{Error, Result};

use crate::crc::crc32c;
use crate::fs::{Inner, Txn};
use crate::group::{Group, flags};

/// A bitmap is read and changed this many bytes at a time.
const CHUNK: usize = 512;
/// Which of a group's bitmaps changed.
const BLOCKS: u8 = 1;
const INODES: u8 = 2;

impl Inner {
    /// Group `g`'s descriptor.
    pub(crate) fn group(&self, g: u32) -> Result<Group> {
        let mut raw = [0u8; 64];
        let n = self.geo.desc_size as usize;
        self.read(self.geo.desc_offset(g), &mut raw[..n])?;
        Group::parse(&self.geo, g, &raw[..n])
    }

    /// Writes group `g`'s descriptor.
    pub(crate) fn put_group(&self, txn: &mut Txn, g: u32, group: &Group) -> Result<()> {
        let mut raw = [0u8; 64];
        let n = self.geo.desc_size as usize;
        group.encode(&self.geo, g, &mut raw[..n]);
        self.write(txn, self.geo.desc_offset(g), &raw[..n])
    }

    /// Notes that group `g`'s bitmaps in `which` changed.
    fn mark(&self, txn: &mut Txn, g: u32, which: u8) -> Result<()> {
        match txn.groups.binary_search_by_key(&g, |&(k, _)| k) {
            Ok(i) => txn.groups[i].1 |= which,
            Err(i) => {
                if radix_vfs::mem::insert(&mut txn.groups, i, (g, which)).is_err() {
                    self.fail();
                    return Err(Error::NoMemory);
                }
            }
        }
        Ok(())
    }

    /// Whether block `block` was freed by the running transaction.
    fn pending(txn: &Txn, block: u64) -> bool {
        let i = txn.pending.partition_point(|&(s, _)| s <= block);
        i > 0 && block < txn.pending[i - 1].0 + txn.pending[i - 1].1
    }

    /// Notes blocks `start..start + n` as freed by the running
    /// transaction.
    fn add_pending(&self, txn: &mut Txn, start: u64, n: u64) -> Result<()> {
        let i = txn.pending.partition_point(|&(s, _)| s < start);
        if i > 0 && txn.pending[i - 1].0 + txn.pending[i - 1].1 == start {
            txn.pending[i - 1].1 += n;
        } else if radix_vfs::mem::insert(&mut txn.pending, i, (start, n)).is_err() {
            self.fail();
            return Err(Error::NoMemory);
        }
        Ok(())
    }

    /// Sets (`on`) or clears `n` bits from `first` of the bitmap in block
    /// `bitmap`; returns how many changed.
    fn set_bits(&self, txn: &mut Txn, bitmap: u64, first: u32, n: u32, on: bool) -> Result<u32> {
        let base = self.geo.offset(bitmap);
        let mut changed = 0;
        let mut bit = first;
        let end = first + n;
        let mut chunk = [0u8; CHUNK];
        while bit < end {
            let byte0 = bit / 8 / CHUNK as u32 * CHUNK as u32;
            let last = (end - 1) / 8;
            let len = ((last - byte0 + 1) as usize).min(CHUNK);
            self.read(base + u64::from(byte0), &mut chunk[..len])?;
            while bit < end && bit / 8 < byte0 + len as u32 {
                let (byte, mask) = (((bit / 8) - byte0) as usize, 1u8 << (bit % 8));
                if (chunk[byte] & mask != 0) != on {
                    chunk[byte] ^= mask;
                    changed += 1;
                }
                bit += 1;
            }
            self.write(txn, base + u64::from(byte0), &chunk[..len])?;
        }
        Ok(changed)
    }

    /// The first run of free blocks of group `g`, from bit `from` of its
    /// `len`, at most `max` long.
    fn find_run(
        &self,
        txn: &Txn,
        g: u32,
        bitmap: u64,
        from: u32,
        len: u32,
        max: u32,
    ) -> Result<Option<(u32, u32)>> {
        let base = self.geo.group_start(g);
        let at = self.geo.offset(bitmap);
        let mut chunk = [0u8; CHUNK];
        let mut chunk_at = u32::MAX;
        let mut start: Option<u32> = None;
        let mut bit = from;
        let quick = txn.pending.is_empty();
        while bit < len {
            let c = bit / (CHUNK as u32 * 8) * (CHUNK as u32 * 8);
            if c != chunk_at {
                self.read(at + u64::from(c / 8), &mut chunk)?;
                chunk_at = c;
            }
            let byte = chunk[((bit - c) / 8) as usize];
            if bit.is_multiple_of(8) && bit + 8 <= len {
                // Whole bytes at a time where they decide nothing.
                if start.is_none() && byte == 0xff {
                    bit += 8;
                    continue;
                }
                if let Some(s) = start
                    && byte == 0
                    && quick
                    && bit + 8 - s <= max
                {
                    bit += 8;
                    continue;
                }
            }
            let used = byte & (1 << (bit % 8)) != 0 || Inner::pending(txn, base + u64::from(bit));
            match (start, used) {
                (None, false) => start = Some(bit),
                (Some(s), true) => return Ok(Some((s, bit - s))),
                _ => {}
            }
            bit += 1;
            if let Some(s) = start
                && bit - s >= max
            {
                return Ok(Some((s, max)));
            }
        }
        Ok(start.map(|s| (s, len - s)))
    }

    /// Gives group `g`, which has none yet, its block bitmap: only its own
    /// metadata in use, as Linux's `ext4_init_block_bitmap` makes it. A
    /// count that does not match the descriptor's is damage.
    fn init_block_bitmap(&self, txn: &mut Txn, g: u32, group: &mut Group) -> Result<()> {
        let geo = &self.geo;
        let bs = geo.block_size as usize;
        let start = geo.group_start(g);
        let blocks = geo.group_blocks(g);
        let mut buf = core::mem::replace(&mut txn.buf, radix_vfs::mem::vec());
        buf[..bs].fill(0);
        let set = |b: u64, buf: &mut [u8]| {
            if b >= start && b < start + u64::from(blocks) {
                let bit = (b - start) as usize;
                buf[bit / 8] |= 1 << (bit % 8);
            }
        };
        for k in 0..u64::from(geo.base_blocks(g)) {
            set(start + k, &mut buf);
        }
        set(group.block_bitmap, &mut buf);
        set(group.inode_bitmap, &mut buf);
        let table = (u64::from(geo.inodes_per_group) * u64::from(geo.inode_size))
            .div_ceil(u64::from(geo.block_size));
        for k in 0..table {
            set(group.inode_table + k, &mut buf);
        }
        let used: u32 = buf[..bs].iter().map(|b| b.count_ones()).sum();
        for bit in blocks as usize..bs * 8 {
            buf[bit / 8] |= 1 << (bit % 8);
        }
        let ok = blocks - used == group.free_blocks;
        let written =
            if ok { self.write(txn, geo.offset(group.block_bitmap), &buf[..bs]) } else { Ok(()) };
        txn.buf = buf;
        if !ok {
            return Err(Error::Corrupt);
        }
        written?;
        group.flags &= !flags::BLOCK_UNINIT;
        self.mark(txn, g, BLOCKS)
    }

    /// Takes a run of up to `max` free blocks, the first at or after
    /// `goal` that it can; returns its first block and length.
    pub(crate) fn take_blocks(&self, txn: &mut Txn, goal: u64, max: u32) -> Result<(u64, u32)> {
        let geo = &self.geo;
        let goal = if (geo.first_data_block..geo.blocks).contains(&goal) {
            goal
        } else {
            geo.first_data_block
        };
        let g0 = geo.group_of(goal);
        let max = max.max(1);
        // The goal's group from the goal, the others from their start,
        // then the goal's group from its start.
        for i in 0..=geo.groups {
            let g = (g0 + i) % geo.groups;
            let mut group = self.group(g)?;
            if group.free_blocks == 0 {
                continue;
            }
            if group.flags & flags::BLOCK_UNINIT != 0 {
                self.init_block_bitmap(txn, g, &mut group)?;
                self.put_group(txn, g, &group)?;
            }
            let from = if i == 0 { (goal - geo.group_start(g)) as u32 } else { 0 };
            let len = geo.group_blocks(g);
            let Some((bit, n)) = self.find_run(txn, g, group.block_bitmap, from, len, max)? else {
                continue;
            };
            let changed = self.set_bits(txn, group.block_bitmap, bit, n, true)?;
            if changed != n || group.free_blocks < n {
                self.fail();
                return Err(Error::Corrupt);
            }
            group.free_blocks -= n;
            self.put_group(txn, g, &group)?;
            self.mark(txn, g, BLOCKS)?;
            let mut space = self.space.lock();
            space.free_blocks = space.free_blocks.saturating_sub(u64::from(n));
            return Ok((geo.group_start(g) + u64::from(bit), n));
        }
        Err(Error::NoSpace)
    }

    /// Frees blocks `start..start + n`. They are not taken again until
    /// the transaction commits.
    pub(crate) fn free_blocks(&self, txn: &mut Txn, start: u64, n: u64) -> Result<()> {
        let geo = &self.geo;
        let end = start.checked_add(n).ok_or(Error::Corrupt)?;
        if start < geo.first_data_block || end > geo.blocks {
            self.fail();
            return Err(Error::Corrupt);
        }
        let mut b = start;
        while b < end {
            let g = geo.group_of(b);
            let gs = geo.group_start(g);
            let e = end.min(gs + u64::from(geo.group_blocks(g)));
            let mut group = self.group(g)?;
            if group.flags & flags::BLOCK_UNINIT != 0 {
                self.fail();
                return Err(Error::Corrupt);
            }
            let cleared =
                self.set_bits(txn, group.block_bitmap, (b - gs) as u32, (e - b) as u32, false)?;
            group.free_blocks += cleared;
            self.put_group(txn, g, &group)?;
            self.mark(txn, g, BLOCKS)?;
            self.add_pending(txn, b, e - b)?;
            {
                let mut space = self.space.lock();
                space.free_blocks += u64::from(cleared);
                space.pending += u64::from(cleared);
            }
            b = e;
        }
        Ok(())
    }

    /// Takes a free inode, in group `near` or the first after it that has
    /// one; `dir` counts it as a directory.
    pub(crate) fn take_inode(&self, txn: &mut Txn, near: u32, dir: bool) -> Result<u32> {
        let geo = &self.geo;
        let ipg = geo.inodes_per_group;
        for i in 0..geo.groups {
            let g = (near + i) % geo.groups;
            let mut group = self.group(g)?;
            if group.free_inodes == 0 {
                continue;
            }
            let bitmap = self.geo.offset(group.inode_bitmap);
            if group.flags & flags::INODE_UNINIT != 0 {
                // Never written: no inode in use, and the bits past the
                // group's inodes set, as Linux leaves them.
                let bs = geo.block_size as usize;
                let mut buf = core::mem::replace(&mut txn.buf, radix_vfs::mem::vec());
                buf[..bs].fill(0);
                for bit in ipg as usize..bs * 8 {
                    buf[bit / 8] |= 1 << (bit % 8);
                }
                let written = self.write(txn, bitmap, &buf[..bs]);
                txn.buf = buf;
                written?;
                group.flags &= !flags::INODE_UNINIT;
            }
            // The first inodes are the system's own.
            let first = if g == 0 { geo.first_ino.saturating_sub(1) } else { 0 };
            let mut chunk = [0u8; CHUNK];
            let mut found = None;
            let mut byte0 = first / 8 / CHUNK as u32 * CHUNK as u32;
            while found.is_none() && byte0 < ipg.div_ceil(8) {
                let len = ((ipg.div_ceil(8) - byte0) as usize).min(CHUNK);
                self.read(bitmap + u64::from(byte0), &mut chunk[..len])?;
                for (k, &byte) in chunk[..len].iter().enumerate() {
                    if byte == 0xff {
                        continue;
                    }
                    let bit0 = (byte0 + k as u32) * 8;
                    if let Some(bit) = (0..8)
                        .map(|b| bit0 + b)
                        .find(|&bit| bit >= first && bit < ipg && byte & (1 << (bit % 8)) == 0)
                    {
                        found = Some(bit);
                        break;
                    }
                }
                byte0 += len as u32;
            }
            let Some(index) = found else {
                self.fail();
                return Err(Error::Corrupt);
            };
            self.set_bits(txn, group.inode_bitmap, index, 1, true)?;
            group.free_inodes -= 1;
            if dir {
                group.dirs += 1;
            }
            if geo.csum != crate::sb::Csum::None && index >= ipg.saturating_sub(group.itable_unused)
            {
                group.itable_unused = ipg - index - 1;
            }
            self.put_group(txn, g, &group)?;
            self.mark(txn, g, INODES)?;
            let mut space = self.space.lock();
            space.free_inodes = space.free_inodes.saturating_sub(1);
            return Ok(g * ipg + index + 1);
        }
        Err(Error::NoSpace)
    }

    /// Frees inode `ino`; `dir` if it was a directory.
    pub(crate) fn free_inode(&self, txn: &mut Txn, ino: u32, dir: bool) -> Result<()> {
        let ipg = self.geo.inodes_per_group;
        let (g, index) = ((ino - 1) / ipg, (ino - 1) % ipg);
        let mut group = self.group(g)?;
        if self.set_bits(txn, group.inode_bitmap, index, 1, false)? != 1 {
            self.fail();
            return Err(Error::Corrupt);
        }
        group.free_inodes += 1;
        if dir {
            group.dirs = group.dirs.saturating_sub(1);
        }
        self.put_group(txn, g, &group)?;
        self.mark(txn, g, INODES)?;
        self.space.lock().free_inodes += 1;
        Ok(())
    }

    /// Sets the checksums of the bitmaps that changed, in their groups'
    /// descriptors (`metadata_csum`), before a commit.
    pub(crate) fn seal_groups(&self, txn: &mut Txn) -> Result<()> {
        if !self.geo.metadata_csum() {
            return Ok(());
        }
        let groups = core::mem::replace(&mut txn.groups, radix_vfs::mem::vec());
        let done = (|| {
            for &(g, which) in &groups {
                let mut group = self.group(g)?;
                if which & BLOCKS != 0 {
                    let sum = self.bitmap_sum(group.block_bitmap, self.geo.blocks_per_group / 8)?;
                    group.block_csum = Group::bitmap_csum(&self.geo, sum);
                }
                if which & INODES != 0 {
                    let sum = self.bitmap_sum(group.inode_bitmap, self.geo.inodes_per_group / 8)?;
                    group.inode_csum = Group::bitmap_csum(&self.geo, sum);
                }
                self.put_group(txn, g, &group)?;
            }
            Ok(())
        })();
        txn.groups = groups;
        done
    }

    /// The CRC-32C of the first `len` bytes of the bitmap in `block`.
    fn bitmap_sum(&self, block: u64, len: u32) -> Result<u32> {
        let mut sum = self.geo.seed;
        let mut chunk = [0u8; CHUNK];
        let mut done = 0;
        while done < len {
            let n = ((len - done) as usize).min(CHUNK);
            self.read(self.geo.offset(block) + u64::from(done), &mut chunk[..n])?;
            sum = crc32c(sum, &chunk[..n]);
            done += n as u32;
        }
        Ok(sum)
    }
}
