// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! The Radix block layer (Phase 7): block devices and what sits on them.
//!
//! - [`BlockDevice`] is a disk, or a part of one, addressed in 512-byte
//!   sectors. A request reads or writes sectors from one place into
//!   [`Segment`]s of pages, which the device reaches by DMA: nothing is
//!   copied on the way (spec 6.3). Requests block the calling thread until
//!   the device is done; the driver below keeps its own queue (a virtqueue
//!   for virtio-blk), so many threads have requests in flight at once.
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

/// What can go wrong with a request.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
    /// The device failed to read or write.
    Io,
    /// The device refuses writes.
    ReadOnly,
    /// The request reaches past the end of the device, or is not whole
    /// sectors.
    OutOfRange,
    /// Out of memory for the request.
    NoMemory,
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Error::Io => "input/output error",
            Error::ReadOnly => "the device is read-only",
            Error::OutOfRange => "the request is outside the device",
            Error::NoMemory => "out of memory",
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
    /// Its size in sectors.
    pub sectors: u64,
    /// The smallest unit it writes without reading first, in bytes.
    pub block_size: u32,
    /// Whether it refuses writes.
    pub read_only: bool,
    /// The most segments one request takes.
    pub max_segments: usize,
}

/// Part of a page the device reads from: `len` bytes from `offset`, both
/// multiples of [`SECTOR`].
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
}

/// A block device, shared.
pub type DeviceRef = KArc<dyn BlockDevice>;

/// Checks a request: whole sectors, within a device of `sectors` sectors,
/// segments inside their pages, and no more than `max` of them. Returns
/// the request's length in sectors.
pub fn check(
    sectors: u64,
    max: usize,
    sector: u64,
    lengths: impl Iterator<Item = (usize, usize)>,
) -> Result<u64, Error> {
    let mut bytes = 0u64;
    let mut count = 0;
    for (offset, len) in lengths {
        count += 1;
        let inside = offset.checked_add(len).is_some_and(|end| end <= PAGE_SIZE);
        if len == 0
            || !inside
            || !(offset as u64).is_multiple_of(SECTOR)
            || !(len as u64).is_multiple_of(SECTOR)
        {
            return Err(Error::OutOfRange);
        }
        bytes += len as u64;
    }
    if count == 0 || count > max {
        return Err(Error::OutOfRange);
    }
    let length = bytes / SECTOR;
    match sector.checked_add(length) {
        Some(end) if end <= sectors => Ok(length),
        _ => Err(Error::OutOfRange),
    }
}

#[cfg(kani)]
mod proofs {
    use super::*;

    /// A request [`check`] accepts lies wholly on the device, in whole
    /// sectors, each segment inside its page, and its length is what the
    /// segments add up to.
    #[kani::proof]
    #[kani::unwind(4)]
    fn accepted_requests_fit() {
        let sectors: u64 = kani::any();
        let sector: u64 = kani::any();
        let segments: [(usize, usize); 2] = kani::any();
        let count: usize = kani::any_where(|&c: &usize| c <= 2);
        if let Ok(len) = check(sectors, 2, sector, segments[..count].iter().copied()) {
            assert!(sector.checked_add(len).is_some_and(|end| end <= sectors));
            let mut bytes = 0u64;
            for &(offset, segment) in &segments[..count] {
                assert!(segment > 0 && offset + segment <= PAGE_SIZE);
                assert!(offset % SECTOR as usize == 0 && segment % SECTOR as usize == 0);
                bytes += segment as u64;
            }
            assert_eq!(bytes / SECTOR, len);
        }
    }
}

/// Part of a device as a device of its own: a partition.
pub struct Slice {
    device: DeviceRef,
    first: u64,
    sectors: u64,
}

impl Slice {
    /// The `sectors` sectors from `first` of `device`. `None` if they
    /// are not all on it.
    pub fn new(device: DeviceRef, first: u64, sectors: u64) -> Option<Slice> {
        let end = first.checked_add(sectors)?;
        (sectors > 0 && end <= device.info().sectors).then_some(Slice { device, first, sectors })
    }
}

impl BlockDevice for Slice {
    fn info(&self) -> Info {
        Info { sectors: self.sectors, ..self.device.info() }
    }

    fn read(&self, sector: u64, segments: &mut [SegmentMut<'_>]) -> Result<(), Error> {
        let lengths = segments.iter().map(|s| (s.offset, s.len));
        check(self.sectors, usize::MAX, sector, lengths)?;
        self.device.read(self.first + sector, segments)
    }

    fn write(&self, sector: u64, segments: &[Segment<'_>]) -> Result<(), Error> {
        let lengths = segments.iter().map(|s| (s.offset, s.len));
        check(self.sectors, usize::MAX, sector, lengths)?;
        self.device.write(self.first + sector, segments)
    }

    fn flush(&self) -> Result<(), Error> {
        self.device.flush()
    }
}

/// Reads sector `sector` of `device` into `out`, through a page: for
/// partition tables and superblocks.
pub fn read_sector(
    device: &dyn BlockDevice,
    sector: u64,
    out: &mut [u8; 512],
) -> Result<(), Error> {
    let mut page = mem::CachePage::new()?;
    device.read(sector, &mut [SegmentMut { page: &mut page, offset: 0, len: 512 }])?;
    out.copy_from_slice(&page.bytes()[..512]);
    Ok(())
}
