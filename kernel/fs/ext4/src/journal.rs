// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! The journal (jbd2), as Linux's `fs/jbd2` writes and replays it.
//!
//! The journal is a file (inode 8 by default) used as a ring of blocks.
//! Its first block is its superblock: its size, its features, where the
//! oldest transaction not yet written home starts (`s_start`, 0 when
//! there is none) and its sequence number. A transaction is one or more
//! descriptor blocks, each naming the blocks of the volume that follow it
//! as copies, then a commit block; revoke blocks name blocks earlier
//! transactions must not write back. Every number in it is big-endian.
//! With checksums (`csum_v3`, which Linux turns on for a volume with
//! `metadata_csum`), each copy's checksum is in its descriptor's tag, and
//! descriptor, revoke and commit blocks end in their own.
//!
//! At mount, a journal with transactions in it is replayed, as Linux's
//! `jbd2_journal_recover` does: a scan finds where the last whole
//! transaction ends (a block of the wrong sequence, or with a checksum
//! that does not match, ends it), then the revoke records of those
//! transactions are gathered, then each copy is written home unless a
//! revoke of the same or a later transaction names its block.
//!
//! Radix writes a transaction at each commit from the journal's first
//! block, after the superblock says where it starts, and writes the
//! volume's blocks home once its commit block is on the disk; the next
//! commit starts from the first block again. So the journal never holds
//! more than one transaction, and none of Radix's needs a revoke record.

use radix_block::mem::CachePage;
use radix_block::{BlockDevice, SECTOR, Segment};
use radix_vfs::mem::{self, KVec};
use radix_vfs::{Error, Result};

use crate::crc::crc32c;
use crate::extent::{Extents, Found};
use crate::{be32, putbe32};

/// Every journal block's first word.
pub const MAGIC: u32 = 0xc03b_3998;

/// Journal block types.
mod kind {
    pub const DESCRIPTOR: u32 = 1;
    pub const COMMIT: u32 = 2;
    pub const SUPER_V1: u32 = 3;
    pub const SUPER_V2: u32 = 4;
    pub const REVOKE: u32 = 5;
}

/// Journal features.
pub mod feature {
    pub const COMPAT_CHECKSUM: u32 = 0x1;
    pub const INCOMPAT_REVOKE: u32 = 0x1;
    pub const INCOMPAT_64BIT: u32 = 0x2;
    pub const INCOMPAT_ASYNC_COMMIT: u32 = 0x4;
    pub const INCOMPAT_CSUM_V2: u32 = 0x8;
    pub const INCOMPAT_CSUM_V3: u32 = 0x10;
}

/// A tag's flags.
mod tag {
    pub const ESCAPE: u32 = 0x1;
    pub const SAME_UUID: u32 = 0x2;
    pub const LAST: u32 = 0x8;
}

/// The incompatible features Radix replays.
const KNOWN_INCOMPAT: u32 = feature::INCOMPAT_REVOKE
    | feature::INCOMPAT_64BIT
    | feature::INCOMPAT_ASYNC_COMMIT
    | feature::INCOMPAT_CSUM_V2
    | feature::INCOMPAT_CSUM_V3;

/// Byte offsets of the journal superblock's fields.
mod at {
    pub const BLOCKTYPE: usize = 0x04;
    pub const BLOCKSIZE: usize = 0x0c;
    pub const MAXLEN: usize = 0x10;
    pub const FIRST: usize = 0x14;
    pub const SEQUENCE: usize = 0x18;
    pub const START: usize = 0x1c;
    pub const COMPAT: usize = 0x24;
    pub const INCOMPAT: usize = 0x28;
    pub const UUID: usize = 0x30;
    pub const CHECKSUM_TYPE: usize = 0x50;
    pub const CHECKSUM: usize = 0xfc;
}

/// The bytes of the journal superblock.
pub const SUPER_SIZE: usize = 1024;

/// A journal and where it is.
pub struct Journal {
    /// Its blocks on the device: the journal inode's extents.
    map: Extents,
    block_size: u32,
    maxlen: u32,
    first: u32,
    /// The sequence the next transaction gets.
    pub sequence: u32,
    /// Where the transactions in it start; 0 when it holds none.
    pub start: u32,
    compat: u32,
    incompat: u32,
    csum_seed: u32,
    /// Its superblock as last read or written.
    sb: KVec<u8>,
}

/// What [`Journal::replay`] found and did.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Replayed {
    /// Whole transactions replayed.
    pub transactions: u32,
    /// Blocks written home.
    pub blocks: u32,
}

/// One thing a walk of the journal finds.
enum Event {
    /// A copy of volume block `home` at journal block `at`, of
    /// transaction `seq`.
    Copy { seq: u32, home: u64, at: u32, flags: u32, sum: u32 },
    /// A revoke of `home` by transaction `seq`.
    Revoke { seq: u32, home: u64 },
}

impl Journal {
    /// Reads the superblock `sb` of the journal whose blocks `map` holds,
    /// on a volume of `block_size`-byte blocks.
    pub fn new(map: Extents, block_size: u32, sb: &[u8]) -> Result<Journal> {
        if be32(sb, 0) != MAGIC {
            return Err(Error::Corrupt);
        }
        let v2 = match be32(sb, at::BLOCKTYPE) {
            kind::SUPER_V1 => false,
            kind::SUPER_V2 => true,
            _ => return Err(Error::Corrupt),
        };
        let maxlen = be32(sb, at::MAXLEN);
        let first = be32(sb, at::FIRST);
        if be32(sb, at::BLOCKSIZE) != block_size
            || first == 0
            || first >= maxlen
            || u64::from(maxlen) > map.blocks()
        {
            return Err(Error::Corrupt);
        }
        let (compat, incompat) =
            if v2 { (be32(sb, at::COMPAT), be32(sb, at::INCOMPAT)) } else { (0, 0) };
        let mut copy = mem::vec_with_capacity(SUPER_SIZE)?;
        mem::extend(&mut copy, &sb[..SUPER_SIZE])?;
        let journal = Journal {
            map,
            block_size,
            maxlen,
            first,
            sequence: be32(sb, at::SEQUENCE),
            start: be32(sb, at::START),
            compat,
            incompat,
            csum_seed: crc32c(!0, &sb[at::UUID..at::UUID + 16]),
            sb: copy,
        };
        if journal.checksums() && super_sum(sb) != be32(sb, at::CHECKSUM) {
            return Err(Error::Corrupt);
        }
        if journal.start != 0 && (journal.start < first || journal.start >= maxlen) {
            return Err(Error::Corrupt);
        }
        Ok(journal)
    }

    /// Whether Radix can replay it.
    pub fn replayable(&self) -> bool {
        self.incompat & !KNOWN_INCOMPAT == 0
    }

    /// Whether it holds transactions to replay.
    pub fn needs_replay(&self) -> bool {
        self.start != 0
    }

    /// Whether its blocks carry checksums (`csum_v2` or `csum_v3`).
    fn checksums(&self) -> bool {
        self.incompat & (feature::INCOMPAT_CSUM_V2 | feature::INCOMPAT_CSUM_V3) != 0
    }

    fn v3(&self) -> bool {
        self.incompat & feature::INCOMPAT_CSUM_V3 != 0
    }

    fn bit64(&self) -> bool {
        self.incompat & feature::INCOMPAT_64BIT != 0
    }

    /// The bytes of a descriptor's tag (Linux's `journal_tag_bytes`).
    fn tag_bytes(&self) -> usize {
        if self.v3() {
            return 16;
        }
        let mut size = 12;
        if self.incompat & feature::INCOMPAT_CSUM_V2 != 0 {
            size += 2;
        }
        if self.bit64() { size } else { size - 4 }
    }

    /// The bytes at the end of a descriptor or revoke block its checksum
    /// takes.
    fn tail(&self) -> usize {
        if self.checksums() { 4 } else { 0 }
    }

    /// The device block of journal block `n`.
    pub fn physical(&self, n: u32) -> Result<u64> {
        match self.map.find(n) {
            Found::Mapped { physical, unwritten: false, .. } => Ok(physical),
            _ => Err(Error::Corrupt),
        }
    }

    /// Journal block `n` blocks after `at`, round the ring.
    fn after(&self, at: u32, n: u32) -> u32 {
        let ring = self.maxlen - self.first;
        self.first + (at - self.first + n % ring) % ring
    }

    /// Whether a block's checksum tail matches (descriptor and revoke
    /// blocks: Linux's `jbd2_descriptor_block_csum_verify`).
    fn tail_ok(&self, b: &mut [u8]) -> bool {
        if !self.checksums() {
            return true;
        }
        let at = b.len() - 4;
        let stored = be32(b, at);
        b[at..].fill(0);
        let ok = crc32c(self.csum_seed, b) == stored;
        putbe32(b, at, stored);
        ok
    }

    /// Whether a commit block's checksum matches.
    fn commit_ok(&self, b: &mut [u8]) -> bool {
        if !self.checksums() {
            return true;
        }
        let stored = be32(b, 16);
        putbe32(b, 16, 0);
        let ok = crc32c(self.csum_seed, b) == stored;
        putbe32(b, 16, stored);
        ok
    }

    /// Walks the transactions from the start, reading each block with
    /// `read`, and calls `f` with what it finds; returns the sequence after
    /// the last whole transaction. What `f` gets of a transaction past it
    /// is to be ignored.
    fn walk(
        &self,
        buf: &mut [u8],
        read: &mut dyn FnMut(u64, &mut [u8]) -> Result<()>,
        f: &mut dyn FnMut(Event) -> Result<()>,
    ) -> Result<u32> {
        let mut seq = self.sequence;
        let mut at = self.start;
        let mut steps = 0u32;
        let tag_bytes = self.tag_bytes();
        let end = buf.len() - self.tail();
        loop {
            if steps >= self.maxlen {
                return Ok(seq);
            }
            read(self.physical(at)?, buf)?;
            if be32(buf, 0) != MAGIC || be32(buf, 8) != seq {
                return Ok(seq);
            }
            match be32(buf, 4) {
                kind::DESCRIPTOR => {
                    if !self.tail_ok(buf) {
                        return Ok(seq);
                    }
                    let mut off = 12;
                    let mut n = 0u32;
                    while off + tag_bytes <= end {
                        let lo = u64::from(be32(buf, off));
                        let flags = if self.v3() {
                            be32(buf, off + 4)
                        } else {
                            u32::from(u16::from_be_bytes([buf[off + 6], buf[off + 7]]))
                        };
                        let hi = if self.bit64() { u64::from(be32(buf, off + 8)) } else { 0 };
                        let sum = if self.v3() {
                            be32(buf, off + 12)
                        } else {
                            u32::from(u16::from_be_bytes([buf[off + 4], buf[off + 5]]))
                        };
                        n += 1;
                        let copy = self.after(at, n);
                        f(Event::Copy { seq, home: lo | (hi << 32), at: copy, flags, sum })?;
                        off += tag_bytes;
                        if flags & tag::SAME_UUID == 0 {
                            off += 16;
                        }
                        if flags & tag::LAST != 0 {
                            break;
                        }
                    }
                    at = self.after(at, n + 1);
                    steps += n + 1;
                }
                kind::REVOKE => {
                    if !self.tail_ok(buf) {
                        return Ok(seq);
                    }
                    let count = (be32(buf, 12) as usize).min(end);
                    let size = if self.bit64() { 8 } else { 4 };
                    let mut off = 16;
                    while off + size <= count {
                        let home = if size == 8 {
                            (u64::from(be32(buf, off)) << 32) | u64::from(be32(buf, off + 4))
                        } else {
                            u64::from(be32(buf, off))
                        };
                        f(Event::Revoke { seq, home })?;
                        off += size;
                    }
                    at = self.after(at, 1);
                    steps += 1;
                }
                kind::COMMIT => {
                    if !self.commit_ok(buf) {
                        return Ok(seq);
                    }
                    seq = seq.wrapping_add(1);
                    at = self.after(at, 1);
                    steps += 1;
                }
                _ => return Ok(seq),
            }
        }
    }

    /// Replays the journal: reads its blocks with `read`, and writes each
    /// volume block a whole transaction holds with `home`, which checks
    /// the block is on the volume. Afterwards it holds nothing (in
    /// memory: [`Journal::super_bytes`] gives the superblock to write).
    pub fn replay(
        &mut self,
        buf: &mut [u8],
        read: &mut dyn FnMut(u64, &mut [u8]) -> Result<()>,
        home: &mut dyn FnMut(u64, &[u8]) -> Result<()>,
    ) -> Result<Replayed> {
        let mut done = Replayed::default();
        if self.start == 0 {
            return Ok(done);
        }
        let end = self.walk(buf, read, &mut |_| Ok(()))?;
        let whole = |seq: u32| seq.wrapping_sub(self.sequence) < end.wrapping_sub(self.sequence);
        // The revokes: each block's latest.
        let mut revoked: KVec<(u64, u32)> = mem::vec();
        self.walk(buf, read, &mut |event| {
            if let Event::Revoke { seq, home } = event
                && whole(seq)
            {
                match revoked.binary_search_by_key(&home, |&(b, _)| b) {
                    Ok(i) => revoked[i].1 = seq,
                    Err(i) => mem::insert(&mut revoked, i, (home, seq))?,
                }
            }
            Ok(())
        })?;
        // The copies to write, in order: those of whole transactions that
        // no revoke of the same or a later one names.
        let first = self.sequence;
        let mut copies: KVec<(u32, u64, u32, u32, u32)> = mem::vec();
        self.walk(buf, read, &mut |event| {
            let Event::Copy { seq, home: block, at, flags, sum } = event else { return Ok(()) };
            if !whole(seq) {
                return Ok(());
            }
            if let Ok(i) = revoked.binary_search_by_key(&block, |&(b, _)| b)
                && revoked[i].1.wrapping_sub(first) >= seq.wrapping_sub(first)
            {
                return Ok(());
            }
            Ok(mem::push(&mut copies, (seq, block, at, flags, sum))?)
        })?;
        for &(seq, block, at, flags, sum) in &copies {
            read(self.physical(at)?, buf)?;
            if self.checksums() {
                let mut s = crc32c(self.csum_seed, &seq.to_be_bytes());
                s = crc32c(s, buf);
                let ok = if self.v3() { s == sum } else { s & 0xffff == sum };
                // A copy that does not match is not written, as on Linux.
                if !ok {
                    continue;
                }
            }
            if flags & tag::ESCAPE != 0 {
                putbe32(buf, 0, MAGIC);
            }
            home(block, buf)?;
            done.blocks += 1;
        }
        done.transactions = end.wrapping_sub(self.sequence);
        self.sequence = end;
        self.start = 0;
        Ok(done)
    }

    /// Sets the features Linux gives a journal it writes: 64-bit block
    /// numbers on a 64-bit volume, `csum_v3` on one with `metadata_csum`,
    /// as `ext4_set_journal_csum_feature_set` does. Only for an empty
    /// version 2 journal.
    pub fn set_features(&mut self, bit64: bool, csum: bool) {
        if be32(&self.sb, at::BLOCKTYPE) != kind::SUPER_V2 {
            return;
        }
        let mut incompat = self.incompat
            & !(feature::INCOMPAT_CSUM_V2
                | feature::INCOMPAT_CSUM_V3
                | feature::INCOMPAT_ASYNC_COMMIT);
        if bit64 {
            incompat |= feature::INCOMPAT_64BIT;
        }
        if csum {
            incompat |= feature::INCOMPAT_CSUM_V3;
        }
        self.incompat = incompat;
        self.compat &= !feature::COMPAT_CHECKSUM;
        if csum {
            self.sb[at::CHECKSUM_TYPE] = 4;
        }
    }

    /// The superblock as it now is, to be written at the journal's first
    /// block.
    pub fn super_bytes(&mut self) -> &[u8] {
        putbe32(&mut self.sb, at::SEQUENCE, self.sequence);
        putbe32(&mut self.sb, at::START, self.start);
        if be32(&self.sb, at::BLOCKTYPE) == kind::SUPER_V2 {
            putbe32(&mut self.sb, at::COMPAT, self.compat);
            putbe32(&mut self.sb, at::INCOMPAT, self.incompat);
            if self.checksums() {
                let sum = super_sum(&self.sb);
                putbe32(&mut self.sb, at::CHECKSUM, sum);
            }
        }
        &self.sb
    }

    /// The most volume blocks one transaction may hold.
    pub fn capacity(&self) -> usize {
        let ring = (self.maxlen - self.first) as usize;
        let per = self.per_descriptor();
        // Each `per` blocks take a descriptor; the commit takes one.
        (ring.saturating_sub(1)) * per / (per + 1)
    }

    /// The tags a descriptor holds: the first followed by the UUID.
    fn per_descriptor(&self) -> usize {
        let room = self.block_size as usize - 12 - self.tail() - 16;
        room / self.tag_bytes()
    }

    /// Writes transaction `sequence` holding the volume blocks `blocks`
    /// (whose current contents `read` gives) into the journal, then its
    /// commit block, each written and flushed in turn: when it returns the
    /// transaction is durable, and the blocks may be written home. Its
    /// superblock is written first, saying the transaction starts at the
    /// first block.
    pub fn write(
        &mut self,
        device: &dyn BlockDevice,
        blocks: &[u64],
        read: &mut dyn FnMut(u64, &mut [u8]) -> Result<()>,
        time: u64,
    ) -> Result<()> {
        if blocks.len() > self.capacity() {
            return Err(Error::NoSpace);
        }
        let bs = self.block_size as usize;
        let seq = self.sequence;
        self.start = self.first;
        let mut desc = CachePage::new()?;
        let mut data = CachePage::new()?;
        {
            desc.bytes_mut()[..bs].fill(0);
            desc.bytes_mut()[..SUPER_SIZE].copy_from_slice(self.super_bytes());
            // The superblock's block may hold more than it (blocks of more
            // than 1 KiB); the rest is the block's own and stays zero.
            self.put(device, 0, &desc)?;
        }
        let per = self.per_descriptor();
        let tag_bytes = self.tag_bytes();
        let mut at = self.first;
        for chunk in blocks.chunks(per) {
            let d = desc.bytes_mut();
            d[..bs].fill(0);
            putbe32(d, 0, MAGIC);
            putbe32(d, 4, kind::DESCRIPTOR);
            putbe32(d, 8, seq);
            let mut off = 12;
            for (k, &home) in chunk.iter().enumerate() {
                let bytes = &mut data.bytes_mut()[..bs];
                read(home, bytes)?;
                let mut flags = 0;
                if be32(bytes, 0) == MAGIC {
                    putbe32(bytes, 0, 0);
                    flags |= tag::ESCAPE;
                }
                if k > 0 {
                    flags |= tag::SAME_UUID;
                }
                if k + 1 == chunk.len() {
                    flags |= tag::LAST;
                }
                let mut sum = 0;
                if self.checksums() {
                    sum = crc32c(crc32c(self.csum_seed, &seq.to_be_bytes()), bytes);
                }
                let d = desc.bytes_mut();
                putbe32(d, off, home as u32);
                if self.v3() {
                    putbe32(d, off + 4, flags);
                    putbe32(d, off + 12, sum);
                } else {
                    d[off + 6..off + 8].copy_from_slice(&(flags as u16).to_be_bytes());
                    if self.checksums() {
                        d[off + 4..off + 6].copy_from_slice(&(sum as u16).to_be_bytes());
                    }
                }
                if self.bit64() {
                    putbe32(d, off + 8, (home >> 32) as u32);
                }
                off += tag_bytes;
                if k == 0 {
                    d[off..off + 16].copy_from_slice(&self.sb[at::UUID..at::UUID + 16]);
                    off += 16;
                }
                self.put(device, at + 1 + k as u32, &data)?;
            }
            if self.checksums() {
                let d = &mut desc.bytes_mut()[..bs];
                let sum = crc32c(self.csum_seed, d);
                putbe32(d, bs - 4, sum);
            }
            self.put(device, at, &desc)?;
            at += 1 + chunk.len() as u32;
        }
        device.flush()?;
        let d = &mut desc.bytes_mut()[..bs];
        d.fill(0);
        putbe32(d, 0, MAGIC);
        putbe32(d, 4, kind::COMMIT);
        putbe32(d, 8, seq);
        let secs = time / 1_000_000_000;
        d[48..56].copy_from_slice(&secs.to_be_bytes());
        putbe32(d, 56, (time % 1_000_000_000) as u32);
        if self.checksums() {
            let sum = crc32c(self.csum_seed, d);
            putbe32(d, 16, sum);
        }
        let sector = self.physical(at)? * (bs as u64 / SECTOR);
        device.write_through(sector, &[Segment { page: &desc, offset: 0, len: bs }])?;
        self.sequence = seq.wrapping_add(1);
        Ok(())
    }

    /// Writes the first block of `page` to journal block `n`.
    fn put(&self, device: &dyn BlockDevice, n: u32, page: &CachePage) -> Result<()> {
        let bs = self.block_size as usize;
        let sector = self.physical(n)? * (bs as u64 / SECTOR);
        Ok(device.write(sector, &[Segment { page, offset: 0, len: bs }])?)
    }
}

/// The journal superblock's checksum, over it with the checksum zeroed.
fn super_sum(sb: &[u8]) -> u32 {
    let mut sum = crc32c(!0, &sb[..at::CHECKSUM]);
    sum = crc32c(sum, &[0; 4]);
    crc32c(sum, &sb[at::CHECKSUM + 4..SUPER_SIZE])
}
