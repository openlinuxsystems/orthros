// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! A page cache of a block device's sectors, for filesystem metadata.
//!
//! Filesystems read and write their metadata (FAT's tables and
//! directories) as bytes at offsets on the device; the cache keeps the
//! device's pages in memory, page `n` holding the device's bytes from
//! `n * PAGE_SIZE`, and remembers per sector which ones it read
//! (`valid`) and which it changed (`dirty`), in whole logical blocks of
//! the device (eight sectors at a time on a disk with 4 KiB sectors). A
//! read fetches only the blocks it lacks; a write of whole blocks fetches
//! nothing first; and
//! [`BlockCache::sync`] writes only the dirty sectors, in runs, then
//! flushes the device. So writes stay in memory until the filesystem
//! syncs (on the writeback thread's schedule, or when asked), and
//! [`BlockCache::discard`] can forget sectors exactly, without touching
//! their neighbours in the same page: a filesystem discards a
//! directory's clusters when it frees them, so that stale metadata is
//! never written over file data later put there.
//!
//! Clean pages can be dropped at any time ([`BlockCache::shrink`]); the
//! cache drops some by itself once it holds [`SOFT_LIMIT`] pages.

use radix_sync::Mutex;

use crate::mem::CachePage;
use crate::pagemap::PageMap;
use crate::{DeviceRef, Error, PAGE_SIZE, SECTOR, SECTORS_PER_PAGE, Segment, SegmentMut};

/// The number of pages above which the cache drops clean ones by itself.
pub const SOFT_LIMIT: usize = 4096;

/// Sectors of a page, as bits of a mask.
type Mask = u8;

struct Block {
    page: CachePage,
    /// The sectors read from the device or written whole.
    valid: Mask,
    /// The sectors changed since they were last written to the device.
    dirty: Mask,
}

struct State {
    blocks: PageMap<Block>,
    /// Pages with dirty sectors.
    dirty: usize,
}

/// A cache of a device's sectors.
pub struct BlockCache {
    device: DeviceRef,
    sectors: u64,
    /// The sectors of the device's logical blocks.
    unit: u64,
    state: Mutex<State>,
}

/// The sectors of page `index` in the logical blocks of `unit` sectors
/// with bytes in `start..end` of the device, as a mask.
fn mask(index: u64, unit: u64, start: u64, end: u64) -> Mask {
    let page_start = index * PAGE_SIZE as u64;
    let first = start.max(page_start) - page_start;
    let last = end.min(page_start + PAGE_SIZE as u64) - page_start;
    let first = first / SECTOR / unit * unit;
    let last = last.div_ceil(SECTOR).next_multiple_of(unit);
    let mut m = 0;
    for s in first..last {
        m |= 1 << s;
    }
    m
}

/// The sectors of `index` in the logical blocks of `unit` sectors that
/// are wholly inside `start..end`.
fn whole(index: u64, unit: u64, start: u64, end: u64) -> Mask {
    let page_start = index * PAGE_SIZE as u64;
    let mut m = 0;
    for s in (0..SECTORS_PER_PAGE).step_by(unit as usize) {
        let (a, b) = (page_start + s * SECTOR, page_start + (s + unit) * SECTOR);
        if a >= start && b <= end {
            m |= run_mask(s, unit);
        }
    }
    m
}

/// The mask of `count` sectors from `first`.
fn run_mask(first: u64, count: u64) -> Mask {
    (((1u16 << count) - 1) << first) as Mask
}

/// Runs of set bits in `m`: (first sector, count).
fn runs(m: Mask) -> impl Iterator<Item = (u64, u64)> {
    let mut s = 0u64;
    core::iter::from_fn(move || {
        while s < SECTORS_PER_PAGE && m & (1 << s) == 0 {
            s += 1;
        }
        if s == SECTORS_PER_PAGE {
            return None;
        }
        let first = s;
        while s < SECTORS_PER_PAGE && m & (1 << s) != 0 {
            s += 1;
        }
        Some((first, s - first))
    })
}

impl BlockCache {
    /// An empty cache of `device`.
    pub fn new(device: DeviceRef) -> BlockCache {
        let info = device.info();
        BlockCache {
            device,
            sectors: info.sectors,
            unit: info.unit(),
            state: Mutex::new(State { blocks: PageMap::new(), dirty: 0 }),
        }
    }

    /// The device.
    pub fn device(&self) -> &DeviceRef {
        &self.device
    }

    /// Checks that `len` bytes from `offset` are on the device; returns
    /// the end.
    fn range(&self, offset: u64, len: usize) -> Result<u64, Error> {
        let end = offset.checked_add(len as u64).ok_or(Error::OutOfRange)?;
        if end > self.sectors * SECTOR { Err(Error::OutOfRange) } else { Ok(end) }
    }

    /// The block of page `index`, with the sectors `need` read.
    fn block<'s>(
        &self,
        state: &'s mut State,
        index: u64,
        need: Mask,
    ) -> Result<&'s mut Block, Error> {
        if state.blocks.get(index).is_none() {
            if state.blocks.len() >= SOFT_LIMIT {
                shrink_state(state, SOFT_LIMIT / 4);
            }
            let block = Block { page: CachePage::new()?, valid: 0, dirty: 0 };
            state.blocks.insert(index, block).map_err(|_| Error::NoMemory)?;
        }
        let block = state.blocks.get_mut(index).ok_or(Error::NoMemory)?;
        let missing = need & !block.valid;
        for (first, count) in runs(missing) {
            let offset = (first * SECTOR) as usize;
            let len = (count * SECTOR) as usize;
            let mut segment = [SegmentMut { page: &mut block.page, offset, len }];
            self.device.read(index * SECTORS_PER_PAGE + first, &mut segment)?;
            radix_hw::device_wrote(&mut block.page.bytes_mut()[offset..offset + len]);
            block.valid |= run_mask(first, count);
        }
        Ok(block)
    }

    /// Reads `out.len()` bytes from byte `offset` of the device.
    pub fn read(&self, offset: u64, out: &mut [u8]) -> Result<(), Error> {
        let end = self.range(offset, out.len())?;
        let mut state = self.state.lock();
        let mut at = offset;
        while at < end {
            let index = at / PAGE_SIZE as u64;
            let within = (at % PAGE_SIZE as u64) as usize;
            let n = (PAGE_SIZE - within).min((end - at) as usize);
            let block = self.block(&mut state, index, mask(index, self.unit, at, at + n as u64))?;
            let done = (at - offset) as usize;
            out[done..done + n].copy_from_slice(&block.page.bytes()[within..within + n]);
            at += n as u64;
        }
        Ok(())
    }

    /// Writes `data` at byte `offset` of the device, in the cache: it
    /// reaches the device at the next [`BlockCache::sync`].
    pub fn write(&self, offset: u64, data: &[u8]) -> Result<(), Error> {
        self.change(offset, data.len(), |bytes, done| {
            bytes.copy_from_slice(&data[done..done + bytes.len()]);
        })
    }

    /// Writes `data` at byte `offset` of the device, whole logical blocks,
    /// and returns once it is durable there
    /// ([`BlockDevice::write_through`]): what else is dirty stays dirty.
    /// The cache keeps the data.
    ///
    /// [`BlockDevice::write_through`]: crate::BlockDevice::write_through
    pub fn write_through(&self, offset: u64, data: &[u8]) -> Result<(), Error> {
        self.write_now(offset, data, true)
    }

    /// Writes `data` at byte `offset` of the device, whole logical blocks,
    /// now rather than at the next [`BlockCache::sync`]: it is durable
    /// after the device's next flush. What else is dirty stays dirty.
    pub fn write_out(&self, offset: u64, data: &[u8]) -> Result<(), Error> {
        self.write_now(offset, data, false)
    }

    fn write_now(&self, offset: u64, data: &[u8], through: bool) -> Result<(), Error> {
        let mut state = self.state.lock();
        self.change_in(&mut state, offset, data.len(), |bytes, done| {
            bytes.copy_from_slice(&data[done..done + bytes.len()]);
        })?;
        let end = offset + data.len() as u64;
        let State { blocks, dirty } = &mut *state;
        let mut at = offset;
        while at < end {
            let index = at / PAGE_SIZE as u64;
            let next = ((index + 1) * PAGE_SIZE as u64).min(end);
            let block = blocks.get_mut(index).ok_or(Error::NoMemory)?;
            let touched = mask(index, self.unit, at, next) & block.dirty;
            for (first, count) in runs(touched) {
                let segment = Segment {
                    page: &block.page,
                    offset: (first * SECTOR) as usize,
                    len: (count * SECTOR) as usize,
                };
                let sector = index * SECTORS_PER_PAGE + first;
                if through {
                    self.device.write_through(sector, &[segment])?;
                } else {
                    self.device.write(sector, &[segment])?;
                }
                block.dirty &= !run_mask(first, count);
                if block.dirty == 0 {
                    *dirty -= 1;
                }
            }
            at = next;
        }
        Ok(())
    }

    /// Sets `len` bytes from `offset` to `byte`, in the cache.
    pub fn fill(&self, offset: u64, len: usize, byte: u8) -> Result<(), Error> {
        self.change(offset, len, |bytes, _| bytes.fill(byte))
    }

    fn change(
        &self,
        offset: u64,
        len: usize,
        f: impl FnMut(&mut [u8], usize),
    ) -> Result<(), Error> {
        self.change_in(&mut self.state.lock(), offset, len, f)
    }

    fn change_in(
        &self,
        state: &mut State,
        offset: u64,
        len: usize,
        mut f: impl FnMut(&mut [u8], usize),
    ) -> Result<(), Error> {
        if self.device.info().read_only {
            return Err(Error::ReadOnly);
        }
        let end = self.range(offset, len)?;
        let mut at = offset;
        while at < end {
            let index = at / PAGE_SIZE as u64;
            let within = (at % PAGE_SIZE as u64) as usize;
            let n = (PAGE_SIZE - within).min((end - at) as usize);
            let touched = mask(index, self.unit, at, at + n as u64);
            // Blocks written whole need not be read first.
            let need = touched & !whole(index, self.unit, at, at + n as u64);
            let block = self.block(state, index, need)?;
            f(&mut block.page.bytes_mut()[within..within + n], (at - offset) as usize);
            let was_dirty = block.dirty != 0;
            block.valid |= touched;
            block.dirty |= touched;
            if !was_dirty {
                state.dirty += 1;
            }
            at += n as u64;
        }
        Ok(())
    }

    /// Forgets `len` bytes from `offset` (whole logical blocks), dirty or
    /// not: they are read from the device again if needed.
    pub fn discard(&self, offset: u64, len: usize) {
        let Ok(end) = self.range(offset, len) else { return };
        let mut state = self.state.lock();
        let mut index = offset / PAGE_SIZE as u64;
        while index * (PAGE_SIZE as u64) < end {
            let gone = whole(index, self.unit, offset, end);
            if let Some(block) = state.blocks.get_mut(index) {
                let was_dirty = block.dirty != 0;
                block.valid &= !gone;
                block.dirty &= !gone;
                let now_clean = was_dirty && block.dirty == 0;
                let empty = block.valid == 0;
                if now_clean {
                    state.dirty -= 1;
                }
                if empty {
                    state.blocks.remove(index);
                }
            }
            index += 1;
        }
    }

    /// Whether anything waits to be written.
    pub fn is_dirty(&self) -> bool {
        self.state.lock().dirty > 0
    }

    /// Writes every dirty sector to the device, then flushes it.
    pub fn sync(&self) -> Result<(), Error> {
        {
            let mut state = self.state.lock();
            let State { blocks, dirty } = &mut *state;
            let mut failed = None;
            let mut index = 0;
            while let Some((found, _)) = blocks.next(index) {
                index = found + 1;
                let Some(block) = blocks.get_mut(found) else { break };
                if block.dirty == 0 {
                    continue;
                }
                let mut written = 0;
                for (first, count) in runs(block.dirty) {
                    let segment = Segment {
                        page: &block.page,
                        offset: (first * SECTOR) as usize,
                        len: (count * SECTOR) as usize,
                    };
                    match self.device.write(found * SECTORS_PER_PAGE + first, &[segment]) {
                        Ok(()) => written |= run_mask(first, count),
                        Err(err) => failed = Some(err),
                    }
                }
                block.dirty &= !written;
                if block.dirty == 0 {
                    *dirty -= 1;
                }
            }
            if let Some(err) = failed {
                return Err(err);
            }
        }
        self.device.flush()
    }

    /// Drops up to `max` clean pages. Returns how many went.
    pub fn shrink(&self, max: usize) -> usize {
        shrink_state(&mut self.state.lock(), max)
    }

    /// How many pages it holds.
    pub fn pages(&self) -> usize {
        self.state.lock().blocks.len()
    }
}

fn shrink_state(state: &mut State, max: usize) -> usize {
    let mut dropped = 0;
    state.blocks.retain(
        |_, block| {
            if dropped < max && block.dirty == 0 {
                dropped += 1;
                false
            } else {
                true
            }
        },
        drop,
    );
    dropped
}
