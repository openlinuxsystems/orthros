// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Chunks: where Btrfs's logical addresses lie on its devices.
//!
//! Every address in a tree, a root or an extent is logical. The chunk
//! tree splits the logical space into chunks, and each chunk lies on one
//! or more stripes, ranges of the devices, in one of the profiles below.
//! The chunks that hold the chunk tree itself are also in the
//! superblock's system chunk array, which is where reading starts.

use radix_vfs::mem::{self, KVec};
use radix_vfs::{Error, Result};

use crate::format::{Key, item, u16_at, u64_at};

/// What a chunk holds, and its profile: how its stripes keep the data.
pub mod kind {
    /// File data.
    pub const DATA: u64 = 1 << 0;
    /// The chunk tree.
    pub const SYSTEM: u64 = 1 << 1;
    /// Every other tree.
    pub const METADATA: u64 = 1 << 2;
    /// Data spread across the stripes, `stripe_len` bytes at a time.
    pub const RAID0: u64 = 1 << 3;
    /// Two copies, on two devices.
    pub const RAID1: u64 = 1 << 4;
    /// Two copies on one device.
    pub const DUP: u64 = 1 << 5;
    /// RAID0 across groups of `sub_stripes` copies.
    pub const RAID10: u64 = 1 << 6;
    /// Data spread across the stripes but one, with their parity.
    pub const RAID5: u64 = 1 << 7;
    /// Data spread across the stripes but two, with two parities.
    pub const RAID6: u64 = 1 << 8;
    /// Three copies, on three devices.
    pub const RAID1C3: u64 = 1 << 9;
    /// Four copies, on four devices.
    pub const RAID1C4: u64 = 1 << 10;

    /// The profile bits; none of them is "single", one copy.
    pub const PROFILE: u64 = RAID0 | RAID1 | DUP | RAID10 | RAID5 | RAID6 | RAID1C3 | RAID1C4;
}

/// The size of a chunk item before its stripes.
const ITEM_SIZE: usize = 48;

/// The size of a stripe in a chunk item.
const STRIPE_SIZE: usize = 32;

/// The most copies of a block any profile keeps (`RAID1C4`).
pub const MAX_COPIES: usize = 4;

/// A range of a device that a chunk lies on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Stripe {
    /// The device's ID within the filesystem.
    pub devid: u64,
    /// Where the stripe starts on it, in bytes.
    pub offset: u64,
}

/// A chunk: `length` bytes of logical space from `start`.
#[derive(Debug)]
pub struct Chunk {
    /// Its first logical address.
    pub start: u64,
    /// How many bytes of logical space it covers.
    pub length: u64,
    /// How many bytes of one stripe come before the next one's, for the
    /// striped profiles.
    pub stripe_len: u64,
    /// What it holds and its profile ([`kind`]).
    pub kind: u64,
    /// The copies in each group, for RAID10.
    pub sub_stripes: u16,
    /// Where it lies.
    pub stripes: KVec<Stripe>,
}

/// Where one logical range lies: up to [`MAX_COPIES`] places on this
/// device, each holding the same bytes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Place {
    /// Where each copy is on the device, in bytes.
    pub copies: [u64; MAX_COPIES],
    /// How many of `copies` there are.
    pub count: usize,
    /// How many bytes from the logical address are contiguous there: up
    /// to the end of the stripe or the chunk.
    pub len: u64,
}

impl Place {
    /// The copies.
    pub fn copies(&self) -> &[u64] {
        &self.copies[..self.count]
    }
}

impl Chunk {
    /// Reads the chunk item `bytes` of the chunk that starts at `start`,
    /// checking it as Linux's tree checker does.
    pub fn read(start: u64, bytes: &[u8], sectorsize: u32) -> Result<Chunk> {
        let length = u64_at(bytes, 0);
        let stripe_len = u64_at(bytes, 16);
        let kind = u64_at(bytes, 24);
        let num_stripes = u16_at(bytes, 44) as usize;
        let sub_stripes = u16_at(bytes, 46);
        let profile = kind & kind::PROFILE;
        let sector = u64::from(sectorsize);
        let fits = bytes.len() >= ITEM_SIZE + num_stripes * STRIPE_SIZE
            && num_stripes > 0
            && length > 0
            && start.is_multiple_of(sector)
            && length.is_multiple_of(sector)
            && start.checked_add(length).is_some()
            && stripe_len.is_power_of_two()
            && kind & (kind::DATA | kind::SYSTEM | kind::METADATA) != 0
            && profile.count_ones() <= 1
            && stripes_fit(profile, num_stripes, sub_stripes);
        if !fits {
            return Err(Error::Corrupt);
        }
        let mut stripes = mem::vec_with_capacity(num_stripes)?;
        for i in 0..num_stripes {
            let at = ITEM_SIZE + i * STRIPE_SIZE;
            let stripe = Stripe { devid: u64_at(bytes, at), offset: u64_at(bytes, at + 8) };
            if stripe.offset.checked_add(length).is_none() {
                return Err(Error::Corrupt);
            }
            mem::push(&mut stripes, stripe)?;
        }
        Ok(Chunk { start, length, stripe_len, kind, sub_stripes, stripes })
    }

    /// The bytes a chunk item with `num_stripes` stripes takes.
    pub fn item_size(num_stripes: usize) -> usize {
        ITEM_SIZE + num_stripes * STRIPE_SIZE
    }

    /// Writes its chunk item to `out`, [`Chunk::item_size`] bytes, as
    /// Linux writes one: the extent tree owns it, and its stripes are on
    /// the device `dev_uuid`.
    pub fn write(&self, out: &mut [u8], sectorsize: u32, dev_uuid: &[u8; 16]) {
        /// The extent tree, which owns every chunk.
        const OWNER: u64 = 2;
        let put_u64 = |out: &mut [u8], at: usize, v: u64| {
            out[at..at + 8].copy_from_slice(&v.to_le_bytes());
        };
        let io = u32::try_from(self.stripe_len).unwrap_or(u32::MAX);
        put_u64(out, 0, self.length);
        put_u64(out, 8, OWNER);
        put_u64(out, 16, self.stripe_len);
        put_u64(out, 24, self.kind);
        out[32..36].copy_from_slice(&io.to_le_bytes());
        out[36..40].copy_from_slice(&io.to_le_bytes());
        out[40..44].copy_from_slice(&sectorsize.to_le_bytes());
        out[44..46].copy_from_slice(&(self.stripes.len() as u16).to_le_bytes());
        out[46..48].copy_from_slice(&self.sub_stripes.to_le_bytes());
        for (i, stripe) in self.stripes.iter().enumerate() {
            let at = ITEM_SIZE + i * STRIPE_SIZE;
            put_u64(out, at, stripe.devid);
            put_u64(out, at + 8, stripe.offset);
            out[at + 16..at + 32].copy_from_slice(dev_uuid);
        }
    }

    /// A copy of it.
    pub fn copy(&self) -> Result<Chunk> {
        let mut stripes = mem::vec_with_capacity(self.stripes.len())?;
        for &stripe in &self.stripes {
            mem::push(&mut stripes, stripe)?;
        }
        Ok(Chunk { stripes, ..*self })
    }

    /// Where the logical address `logical`, in the chunk, lies on device
    /// `devid`; copies on other devices are left out.
    ///
    /// Fails with [`Error::NotSupported`] for RAID5 and RAID6, which
    /// Radix does not read ([`crate::superblock::incompat::READ`]).
    pub fn place(&self, logical: u64, devid: u64) -> Result<Place> {
        let off = logical - self.start;
        let mut place = Place { copies: [0; MAX_COPIES], count: 0, len: self.length - off };
        let mut add = |stripe: &Stripe, at: u64| {
            if stripe.devid == devid && place.count < MAX_COPIES {
                place.copies[place.count] = stripe.offset + at;
                place.count += 1;
            }
        };
        let n = self.stripes.len() as u64;
        match self.kind & kind::PROFILE {
            kind::RAID0 | kind::RAID10 => {
                let sub =
                    if self.kind & kind::RAID10 != 0 { u64::from(self.sub_stripes) } else { 1 };
                let groups = n / sub;
                let stripe_nr = off / self.stripe_len;
                let within = off % self.stripe_len;
                let first = (stripe_nr % groups) * sub;
                let at = (stripe_nr / groups) * self.stripe_len + within;
                for stripe in &self.stripes[first as usize..(first + sub) as usize] {
                    add(stripe, at);
                }
                place.len = place.len.min(self.stripe_len - within);
            }
            kind::RAID5 | kind::RAID6 => return Err(Error::NotSupported),
            // One copy, or every stripe a whole copy.
            _ => {
                for stripe in &self.stripes {
                    add(stripe, off);
                }
            }
        }
        Ok(place)
    }
}

/// Whether a profile can have `num_stripes` stripes, `sub_stripes` of
/// them in each group.
fn stripes_fit(profile: u64, num_stripes: usize, sub_stripes: u16) -> bool {
    match profile {
        0 => num_stripes == 1,
        kind::DUP | kind::RAID1 => num_stripes == 2,
        kind::RAID1C3 => num_stripes == 3,
        kind::RAID1C4 => num_stripes == 4,
        kind::RAID10 => sub_stripes == 2 && num_stripes >= 2 && num_stripes.is_multiple_of(2),
        kind::RAID5 => num_stripes >= 2,
        kind::RAID6 => num_stripes >= 3,
        _ => true,
    }
}

/// Every chunk, by where it starts.
pub struct ChunkMap {
    chunks: KVec<Chunk>,
}

impl ChunkMap {
    /// No chunks.
    pub fn empty() -> ChunkMap {
        ChunkMap { chunks: mem::vec() }
    }

    /// The chunks of the superblock's system chunk array: a key and a
    /// chunk item each.
    pub fn bootstrap(array: &[u8], sectorsize: u32) -> Result<ChunkMap> {
        let mut map = ChunkMap::empty();
        let mut at = 0;
        while at < array.len() {
            let key = Key::read(array, at);
            at += Key::SIZE;
            let num_stripes = u16_at(array, at + 44) as usize;
            let size = Chunk::item_size(num_stripes);
            if key.kind != item::CHUNK_ITEM || at + size > array.len() {
                return Err(Error::Corrupt);
            }
            map.insert(Chunk::read(key.offset, &array[at..at + size], sectorsize)?)?;
            at += size;
        }
        Ok(map)
    }

    /// Adds `chunk`, which the chunk tree names; the system chunk array's
    /// chunks are in the tree too, and must be the same.
    pub fn insert(&mut self, chunk: Chunk) -> Result<()> {
        let at = self.chunks.partition_point(|c| c.start < chunk.start);
        if let Some(same) = self.chunks.get(at).filter(|c| c.start == chunk.start) {
            let equal = same.length == chunk.length
                && same.kind == chunk.kind
                && same.stripes == chunk.stripes;
            return if equal { Ok(()) } else { Err(Error::Corrupt) };
        }
        let overlaps_before = at > 0 && {
            let before = &self.chunks[at - 1];
            before.start + before.length > chunk.start
        };
        let overlaps_after =
            self.chunks.get(at).is_some_and(|after| chunk.start + chunk.length > after.start);
        if overlaps_before || overlaps_after {
            return Err(Error::Corrupt);
        }
        mem::insert(&mut self.chunks, at, chunk)?;
        Ok(())
    }

    /// The chunk that holds `logical`.
    pub fn find(&self, logical: u64) -> Option<&Chunk> {
        let at = self.chunks.partition_point(|c| c.start <= logical);
        let chunk = self.chunks.get(at.checked_sub(1)?)?;
        (logical - chunk.start < chunk.length).then_some(chunk)
    }

    /// Where `logical` lies on device `devid`.
    ///
    /// Fails with [`Error::Corrupt`] when no chunk holds it, and
    /// [`Error::NoDevice`] when no copy of it is on the device.
    pub fn place(&self, logical: u64, devid: u64) -> Result<Place> {
        let place = self.find(logical).ok_or(Error::Corrupt)?.place(logical, devid)?;
        if place.count == 0 {
            return Err(Error::NoDevice);
        }
        Ok(place)
    }

    /// The chunks, in order.
    pub fn chunks(&self) -> &[Chunk] {
        &self.chunks
    }

    /// The chunks, taken out.
    pub fn into_chunks(self) -> KVec<Chunk> {
        self.chunks
    }

    /// A copy of the map, with `chunk` too.
    pub fn with(&self, chunk: Chunk) -> Result<ChunkMap> {
        let mut map = ChunkMap { chunks: mem::vec_with_capacity(self.chunks.len() + 1)? };
        for c in &self.chunks {
            mem::push(&mut map.chunks, c.copy()?)?;
        }
        map.insert(chunk)?;
        Ok(map)
    }

    /// The first logical address after every chunk.
    pub fn end(&self) -> u64 {
        self.chunks.iter().map(|c| c.start + c.length).max().unwrap_or(0)
    }
}
