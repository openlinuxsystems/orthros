// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! A disk in memory: for the storage crates' tests, and for the kernel's
//! own when there is no disk.

use core::sync::atomic::{AtomicU32, AtomicU64, Ordering};

use radix_sync::Mutex;

use crate::mem::CachePage;
use crate::pagemap::PageMap;
use crate::{
    BlockDevice, Error, Info, MAX_LOGICAL_BLOCK, PAGE_SIZE, SECTOR, SECTORS_PER_PAGE, Segment,
    SegmentMut, check, check_discard,
};

/// A disk whose sectors are pages of memory, allocated as they are
/// first written (unwritten sectors read as zeros).
pub struct RamDisk {
    sectors: u64,
    read_only: bool,
    /// The size of its logical blocks, in bytes.
    logical_block: u32,
    pages: Mutex<PageMap<CachePage>>,
    /// Requests that reached the disk.
    requests: AtomicU64,
    /// Flushes.
    flushes: AtomicU64,
    /// Sectors discarded.
    discarded: AtomicU64,
    /// Fail every request after this many more (u32::MAX: never), to test
    /// what sits above against a failing disk.
    fail_after: AtomicU32,
}

impl RamDisk {
    /// An empty disk of `sectors` sectors.
    pub fn new(sectors: u64, read_only: bool) -> RamDisk {
        RamDisk::with_blocks(sectors, read_only, SECTOR as u32)
    }

    /// An empty disk of `sectors` sectors in logical blocks of
    /// `logical_block` bytes (a power of two from 512 to
    /// [`MAX_LOGICAL_BLOCK`]), which refuses what is not whole blocks, as
    /// a disk with 4 KiB sectors does.
    ///
    /// # Panics
    ///
    /// If `logical_block` is not such a size, or `sectors` not whole
    /// blocks of it.
    pub fn with_blocks(sectors: u64, read_only: bool, logical_block: u32) -> RamDisk {
        assert!(
            logical_block.is_power_of_two()
                && (SECTOR as u32..=MAX_LOGICAL_BLOCK).contains(&logical_block)
                && sectors.is_multiple_of(u64::from(logical_block) / SECTOR),
            "a disk of {sectors} sectors in blocks of {logical_block} bytes"
        );
        RamDisk {
            sectors,
            read_only,
            logical_block,
            pages: Mutex::new(PageMap::new()),
            requests: AtomicU64::new(0),
            flushes: AtomicU64::new(0),
            discarded: AtomicU64::new(0),
            fail_after: AtomicU32::new(u32::MAX),
        }
    }

    /// Copies `bytes` onto the disk from byte `offset`, bypassing the
    /// read-only flag: for building images.
    pub fn load(&self, offset: u64, bytes: &[u8]) -> Result<(), Error> {
        let mut pages = self.pages.lock();
        let mut done = 0;
        while done < bytes.len() {
            let at = offset + done as u64;
            let (index, within) = (at / PAGE_SIZE as u64, (at % PAGE_SIZE as u64) as usize);
            let n = (PAGE_SIZE - within).min(bytes.len() - done);
            let chunk = &bytes[done..done + n];
            // A missing page reads as zeros, so zeros need no page.
            if pages.get(index).is_some() || chunk.iter().any(|&b| b != 0) {
                let page = page_mut(&mut pages, index)?;
                page.bytes_mut()[within..within + n].copy_from_slice(chunk);
            }
            done += n;
        }
        Ok(())
    }

    /// Copies `out.len()` bytes from byte `offset` of the disk.
    pub fn dump(&self, offset: u64, out: &mut [u8]) {
        let pages = self.pages.lock();
        let mut done = 0;
        while done < out.len() {
            let at = offset + done as u64;
            let (index, within) = (at / PAGE_SIZE as u64, (at % PAGE_SIZE as u64) as usize);
            let n = (PAGE_SIZE - within).min(out.len() - done);
            let chunk = &mut out[done..done + n];
            match pages.get(index) {
                Some(page) => chunk.copy_from_slice(&page.bytes()[within..within + n]),
                None => chunk.fill(0),
            }
            done += n;
        }
    }

    /// Requests that reached the disk so far.
    pub fn requests(&self) -> u64 {
        self.requests.load(Ordering::Relaxed)
    }

    /// Flushes so far.
    pub fn flushes(&self) -> u64 {
        self.flushes.load(Ordering::Relaxed)
    }

    /// Sectors discarded so far.
    pub fn discarded(&self) -> u64 {
        self.discarded.load(Ordering::Relaxed)
    }

    /// Makes every request after the next `count` fail with an I/O error.
    pub fn fail_after(&self, count: u32) {
        self.fail_after.store(count, Ordering::Relaxed);
    }

    /// The sectors of its logical blocks.
    fn unit(&self) -> u64 {
        u64::from(self.logical_block) / SECTOR
    }

    fn start(&self) -> Result<(), Error> {
        self.requests.fetch_add(1, Ordering::Relaxed);
        let left = self.fail_after.load(Ordering::Relaxed);
        if left == 0 {
            return Err(Error::Io);
        }
        if left != u32::MAX {
            self.fail_after.store(left - 1, Ordering::Relaxed);
        }
        Ok(())
    }
}

fn page_mut(pages: &mut PageMap<CachePage>, index: u64) -> Result<&mut CachePage, Error> {
    if pages.get(index).is_none() {
        let page = CachePage::new()?;
        pages.insert(index, page).map_err(|_| Error::NoMemory)?;
    }
    pages.get_mut(index).ok_or(Error::NoMemory)
}

impl BlockDevice for RamDisk {
    fn info(&self) -> Info {
        Info {
            sectors: self.sectors,
            logical_block: self.logical_block,
            block_size: self.logical_block,
            read_only: self.read_only,
            max_segments: 64,
            discard_sectors: self.sectors,
            rotational: false,
        }
    }

    fn read(&self, sector: u64, segments: &mut [SegmentMut<'_>]) -> Result<(), Error> {
        check(self.sectors, self.unit(), 64, sector, segments.iter().map(|s| (s.offset, s.len)))?;
        self.start()?;
        let mut at = sector * SECTOR;
        for segment in segments.iter_mut() {
            let out = &mut segment.page.bytes_mut()[segment.offset..segment.offset + segment.len];
            self.dump(at, out);
            at += segment.len as u64;
        }
        Ok(())
    }

    fn write(&self, sector: u64, segments: &[Segment<'_>]) -> Result<(), Error> {
        check(self.sectors, self.unit(), 64, sector, segments.iter().map(|s| (s.offset, s.len)))?;
        if self.read_only {
            return Err(Error::ReadOnly);
        }
        self.start()?;
        let mut at = sector * SECTOR;
        for segment in segments {
            self.load(at, &segment.page.bytes()[segment.offset..segment.offset + segment.len])?;
            at += segment.len as u64;
        }
        Ok(())
    }

    fn flush(&self) -> Result<(), Error> {
        self.start()?;
        self.flushes.fetch_add(1, Ordering::Relaxed);
        Ok(())
    }

    /// Discarded sectors read as zeros: whole pages are let go, and the
    /// rest zeroed.
    fn discard(&self, sector: u64, count: u64) -> Result<(), Error> {
        check_discard(self.sectors, self.unit(), sector, count)?;
        if self.read_only {
            return Err(Error::ReadOnly);
        }
        self.start()?;
        let mut pages = self.pages.lock();
        let (mut at, end) = (sector, sector + count);
        while at < end {
            let index = at / SECTORS_PER_PAGE;
            let within = at % SECTORS_PER_PAGE;
            let n = (SECTORS_PER_PAGE - within).min(end - at);
            if n == SECTORS_PER_PAGE {
                pages.remove(index);
            } else if let Some(page) = pages.get_mut(index) {
                let from = (within * SECTOR) as usize;
                page.bytes_mut()[from..from + (n * SECTOR) as usize].fill(0);
            }
            at += n;
        }
        self.discarded.fetch_add(count, Ordering::Relaxed);
        Ok(())
    }
}
