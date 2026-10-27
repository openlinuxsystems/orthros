// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! The Radix block layer (Phase 7): block devices and what sits on them.
//!
//! - [`BlockDevice`] is a disk, or a part of one, addressed in 512-byte
//!   sectors, as Linux addresses them, whatever the size of its logical
//!   blocks ([`Info::logical_block`]): 512 bytes, or 4 KiB on a disk with
//!   4 KiB sectors (4Kn), whose requests start and end on whole blocks of
//!   eight sectors. A request reads or writes sectors from one place into
//!   [`Segment`]s of pages, which the device reaches by DMA: nothing is
//!   copied on the way (spec 6.3). Requests block the calling thread until
//!   the device is done; the driver below keeps its own queue (a virtqueue
//!   for virtio-blk, a command list for AHCI), so many threads have
//!   requests in flight at once. Besides reads, writes and flushes, a
//!   device writes through its cache ([`BlockDevice::write_through`]) and
//!   forgets sectors ([`BlockDevice::discard`]) as well as it can.
//! - [`partition`] reads MBR and GPT partition tables, and [`Slice`] is a
//!   partition as a block device of its own.
//! - [`cache::BlockCache`] keeps a device's blocks in pages, for the
//!   metadata of the filesystems on it, written back when they sync.
//! - [`pagemap::PageMap`] is the radix tree that indexes page caches.
//! - [`mem`] is where the storage compartments get memory.
//!
//! The crate is a compartment: it has no `unsafe` code, and builds for
//! the host (feature `host`), where its tests run against [`ram::RamDisk`].

#![cfg_attr(not(any(test, kani, feature = "host")), no_std)]
#![forbid(unsafe_code)]
#![cfg_attr(not(kani), feature(allocator_ext))]
// Kani's own, older nightly has the allocator API unstable still.
#![cfg_attr(kani, feature(allocator_api))]
#![feature(vec_push_within_capacity)]

extern crate alloc;

pub mod cache;
pub mod crc32;
pub mod mem;
pub mod pagemap;
pub mod partition;
pub mod ram;

#[cfg(test)]
mod tests;

use core::fmt;

pub use radix_hw::{PAGE_SIZE, Page};

use crate::mem::KArc;

/// The size of a sector, the unit of block addresses.
pub const SECTOR: u64 = 512;

/// Sectors in a page.
pub const SECTORS_PER_PAGE: u64 = PAGE_SIZE as u64 / SECTOR;

/// The largest logical block a device may have, in bytes: a page.
pub const MAX_LOGICAL_BLOCK: u32 = PAGE_SIZE as u32;

/// The most frames one [`BlockDevice::read_frames`] or
/// [`BlockDevice::write_frames`] takes: a mebibyte of whole pages.
pub const MAX_FRAMES: usize = 256;

/// What can go wrong with a request.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
    /// The device failed to read or write.
    Io,
    /// The device refuses writes.
    ReadOnly,
    /// The request reaches past the end of the device, or is not whole
    /// logical blocks.
    OutOfRange,
    /// Out of memory for the request.
    NoMemory,
    /// The device does not do this kind of request.
    Unsupported,
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Error::Io => "input/output error",
            Error::ReadOnly => "the device is read-only",
            Error::OutOfRange => "the request is outside the device",
            Error::NoMemory => "out of memory",
            Error::Unsupported => "the device does not do that",
        })
    }
}

impl From<mem::NoMemory> for Error {
    fn from(_: mem::NoMemory) -> Error {
        Error::NoMemory
    }
}

/// What a block device is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Info {
    /// Its size in sectors, a whole number of logical blocks.
    pub sectors: u64,
    /// The size of its logical blocks in bytes: the least it reads or
    /// writes, a power of two from 512 to [`MAX_LOGICAL_BLOCK`] (4096 on
    /// a 4Kn disk). A request's first sector, and every segment's offset
    /// and length, are whole blocks.
    pub logical_block: u32,
    /// The smallest unit it writes without reading first, in bytes.
    pub block_size: u32,
    /// Whether it refuses writes.
    pub read_only: bool,
    /// The most segments one request takes.
    pub max_segments: usize,
    /// The most sectors it discards in one command, or 0 if it does not
    /// discard; [`BlockDevice::discard`] takes any number and splits them.
    pub discard_sectors: u64,
    /// Whether it spins, so that seeks cost: a hard disk.
    pub rotational: bool,
}

impl Info {
    /// The sectors in one of its logical blocks.
    pub fn unit(&self) -> u64 {
        (u64::from(self.logical_block) / SECTOR).max(1)
    }
}

/// Part of a page the device reads from: `len` bytes from `offset`, both
/// whole logical blocks ([`Info::logical_block`]).
pub struct Segment<'a> {
    /// The page.
    pub page: &'a Page,
    /// Where in the page.
    pub offset: usize,
    /// How many bytes.
    pub len: usize,
}

/// Part of a page the device writes into.
pub struct SegmentMut<'a> {
    /// The page.
    pub page: &'a mut Page,
    /// Where in the page.
    pub offset: usize,
    /// How many bytes.
    pub len: usize,
}

/// A disk, or part of one.
pub trait BlockDevice: Send + Sync {
    /// What it is.
    fn info(&self) -> Info;

    /// Reads the sectors from `sector` into `segments`, in order: as many
    /// as their lengths add up to. Returns once the device is done.
    fn read(&self, sector: u64, segments: &mut [SegmentMut<'_>]) -> Result<(), Error>;

    /// Writes `segments`, in order, to the sectors from `sector`. Returns
    /// once the device has the data (in its cache, until [`flush`]).
    ///
    /// [`flush`]: BlockDevice::flush
    fn write(&self, sector: u64, segments: &[Segment<'_>]) -> Result<(), Error>;

    /// Makes every write that returned durable.
    fn flush(&self) -> Result<(), Error>;

    /// Writes `segments` as [`write`] does, and returns once they are
    /// durable: a write with forced unit access, on a disk that has it.
    /// It makes no other write durable. The default writes, then flushes.
    ///
    /// [`write`]: BlockDevice::write
    fn write_through(&self, sector: u64, segments: &[Segment<'_>]) -> Result<(), Error> {
        self.write(sector, segments)?;
        self.flush()
    }

    /// Reads the sectors from `sector` straight into `frames` (physical
    /// address, length), in order, as [`read`] does into pages: memory
    /// that its owner, a program, lends for the request, at most
    /// [`MAX_FRAMES`] parts of pages, each whole logical blocks long and
    /// starting at one. They stay in place, and their owner's, until
    /// it returns, so it returns only once the device is done with every
    /// one of them, even when the request fails. Fails with
    /// [`Error::Unsupported`] on a device that cannot, which the default
    /// does; the caller then reads through pages of its own.
    ///
    /// [`read`]: BlockDevice::read
    fn read_frames(&self, _sector: u64, _frames: &[(u64, u32)]) -> Result<(), Error> {
        Err(Error::Unsupported)
    }

    /// Writes `frames` to the sectors from `sector`, as [`write`] does,
    /// from memory lent as [`BlockDevice::read_frames`] has it.
    ///
    /// [`write`]: BlockDevice::write
    fn write_frames(&self, _sector: u64, _frames: &[(u64, u32)]) -> Result<(), Error> {
        Err(Error::Unsupported)
    }

    /// Tells the device that the `count` sectors from `sector` hold
    /// nothing worth keeping (ATA's TRIM, NVMe's deallocate): it may free
    /// them, and what they read afterwards is the device's choice. Fails
    /// with [`Error::Unsupported`] on a device that does not discard
    /// ([`Info::discard_sectors`] is 0), which the default does.
    fn discard(&self, _sector: u64, _count: u64) -> Result<(), Error> {
        Err(Error::Unsupported)
    }
}

/// A block device, shared.
pub type DeviceRef = KArc<dyn BlockDevice>;

/// Checks a request: whole logical blocks of `unit` sectors ([`Info::unit`])
/// within a device of `sectors` sectors, segments inside their pages, and
/// no more than `max` of them. Returns the request's length in sectors.
pub fn check(
    sectors: u64,
    unit: u64,
    max: usize,
    sector: u64,
    lengths: impl Iterator<Item = (usize, usize)>,
) -> Result<u64, Error> {
    let block = unit * SECTOR;
    let mut bytes = 0u64;
    let mut count = 0;
    for (offset, len) in lengths {
        count += 1;
        let inside = offset.checked_add(len).is_some_and(|end| end <= PAGE_SIZE);
        if len == 0
            || !inside
            || !(offset as u64).is_multiple_of(block)
            || !(len as u64).is_multiple_of(block)
        {
            return Err(Error::OutOfRange);
        }
        bytes += len as u64;
    }
    if count == 0 || count > max || !sector.is_multiple_of(unit) {
        return Err(Error::OutOfRange);
    }
    let length = bytes / SECTOR;
    match sector.checked_add(length) {
        Some(end) if end <= sectors => Ok(length),
        _ => Err(Error::OutOfRange),
    }
}

/// Checks a request to or from lent memory ([`BlockDevice::read_frames`]),
/// as [`check`] does pages: each frame (physical address, length) whole
/// logical blocks inside its page, and no more than [`MAX_FRAMES`] of
/// them. Returns the request's length in sectors.
pub fn check_frames(
    sectors: u64,
    unit: u64,
    sector: u64,
    frames: &[(u64, u32)],
) -> Result<u64, Error> {
    let lengths =
        frames.iter().map(|&(phys, len)| ((phys % PAGE_SIZE as u64) as usize, len as usize));
    check(sectors, unit, MAX_FRAMES, sector, lengths)
}

/// Checks a discard of `count` sectors from `sector` on a device of
/// `sectors` sectors in logical blocks of `unit`: at least one block, all
/// on the device, and whole blocks.
pub fn check_discard(sectors: u64, unit: u64, sector: u64, count: u64) -> Result<(), Error> {
    match sector.checked_add(count) {
        Some(end)
            if count > 0
                && end <= sectors
                && sector.is_multiple_of(unit)
                && count.is_multiple_of(unit) =>
        {
            Ok(())
        }
        _ => Err(Error::OutOfRange),
    }
}

#[cfg(kani)]
mod proofs {
    use super::*;

    /// A request [`check`] accepts lies wholly on the device, in whole
    /// logical blocks, each segment inside its page, and its length is
    /// what the segments add up to.
    #[kani::proof]
    #[kani::unwind(4)]
    fn accepted_requests_fit() {
        let sectors: u64 = kani::any();
        let sector: u64 = kani::any();
        let unit: u64 = kani::any_where(|&unit: &u64| unit == 1 || unit == 8);
        let segments: [(usize, usize); 2] = kani::any();
        let count: usize = kani::any_where(|&c: &usize| c <= 2);
        if let Ok(len) = check(sectors, unit, 2, sector, segments[..count].iter().copied()) {
            assert!(sector.checked_add(len).is_some_and(|end| end <= sectors));
            assert!(sector % unit == 0 && len % unit == 0);
            let block = (unit * SECTOR) as usize;
            let mut bytes = 0u64;
            for &(offset, segment) in &segments[..count] {
                assert!(segment > 0 && offset + segment <= PAGE_SIZE);
                assert!(offset % block == 0 && segment % block == 0);
                bytes += segment as u64;
            }
            assert_eq!(bytes / SECTOR, len);
        }
    }

    /// A discard [`check_discard`] accepts covers whole logical blocks,
    /// all of them on the device.
    #[kani::proof]
    fn accepted_discards_fit() {
        let (sectors, sector, count): (u64, u64, u64) = kani::any();
        let unit: u64 = kani::any_where(|&unit: &u64| unit == 1 || unit == 8);
        if check_discard(sectors, unit, sector, count).is_ok() {
            assert!(count > 0 && sector < sectors && sectors - sector >= count);
            assert!(sector % unit == 0 && count % unit == 0);
        }
    }

    /// A partition [`partition::fit`] keeps is on the disk, after its
    /// first sector, not empty, and no bigger than the table says.
    #[kani::proof]
    fn partitions_fit() {
        let (first, sectors, disk): (u64, u64, u64) = kani::any();
        if let Some((kept, truncated)) = partition::fit(first, sectors, disk) {
            assert!(partition::within(first, kept, disk));
            assert!(kept <= sectors && truncated == (kept < sectors));
        }
    }
}

/// Part of a device as a device of its own: a partition.
pub struct Slice {
    device: DeviceRef,
    first: u64,
    sectors: u64,
    /// The sectors of the device's logical blocks.
    unit: u64,
}

impl Slice {
    /// The `sectors` sectors from `first` of `device`. `None` if they
    /// are not all on it, or not whole logical blocks of it.
    pub fn new(device: DeviceRef, first: u64, sectors: u64) -> Option<Slice> {
        let end = first.checked_add(sectors)?;
        let info = device.info();
        let unit = info.unit();
        let whole = first.is_multiple_of(unit) && sectors.is_multiple_of(unit);
        (sectors > 0 && end <= info.sectors && whole).then_some(Slice {
            device,
            first,
            sectors,
            unit,
        })
    }
}

impl BlockDevice for Slice {
    fn info(&self) -> Info {
        Info { sectors: self.sectors, ..self.device.info() }
    }

    fn read(&self, sector: u64, segments: &mut [SegmentMut<'_>]) -> Result<(), Error> {
        let lengths = segments.iter().map(|s| (s.offset, s.len));
        check(self.sectors, self.unit, usize::MAX, sector, lengths)?;
        self.device.read(self.first + sector, segments)
    }

    fn write(&self, sector: u64, segments: &[Segment<'_>]) -> Result<(), Error> {
        let lengths = segments.iter().map(|s| (s.offset, s.len));
        check(self.sectors, self.unit, usize::MAX, sector, lengths)?;
        self.device.write(self.first + sector, segments)
    }

    fn flush(&self) -> Result<(), Error> {
        self.device.flush()
    }

    fn write_through(&self, sector: u64, segments: &[Segment<'_>]) -> Result<(), Error> {
        let lengths = segments.iter().map(|s| (s.offset, s.len));
        check(self.sectors, self.unit, usize::MAX, sector, lengths)?;
        self.device.write_through(self.first + sector, segments)
    }

    fn read_frames(&self, sector: u64, frames: &[(u64, u32)]) -> Result<(), Error> {
        check_frames(self.sectors, self.unit, sector, frames)?;
        self.device.read_frames(self.first + sector, frames)
    }

    fn write_frames(&self, sector: u64, frames: &[(u64, u32)]) -> Result<(), Error> {
        check_frames(self.sectors, self.unit, sector, frames)?;
        self.device.write_frames(self.first + sector, frames)
    }

    fn discard(&self, sector: u64, count: u64) -> Result<(), Error> {
        check_discard(self.sectors, self.unit, sector, count)?;
        self.device.discard(self.first + sector, count)
    }
}

/// Reads sector `sector` of `device` into `out`, through a page: for
/// partition tables and superblocks. On a device with larger logical
/// blocks, the whole block that holds it is read.
pub fn read_sector(
    device: &dyn BlockDevice,
    sector: u64,
    out: &mut [u8; 512],
) -> Result<(), Error> {
    let unit = device.info().unit();
    let first = sector - sector % unit;
    let mut page = mem::CachePage::new()?;
    let len = (unit * SECTOR) as usize;
    device.read(first, &mut [SegmentMut { page: &mut page, offset: 0, len }])?;
    let at = ((sector - first) * SECTOR) as usize;
    out.copy_from_slice(&page.bytes()[at..at + 512]);
    Ok(())
}
