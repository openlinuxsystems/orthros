// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Raw reads and writes of a block device, as `/dev/sda` and its kind
//! take them: straight to the device, through pages of the kernel's.
//!
//! Nothing is cached, as with FreeBSD's disks, or Linux's opened with
//! `O_DIRECT`: a read reads what the device holds when it is made, a
//! write has reached the device when it returns, and fsync(2) empties
//! the device's own cache. Unlike them, any offset and length work: a
//! write that covers part of a sector reads that sector first and writes
//! it back whole, under the node's lock so that two such writes to one
//! sector cannot lose each other's bytes. Each request moves at most
//! [`MAX_PAGES`] pages, as many as the device takes in one.

use radix_block::mem::CachePage;
use radix_block::{BlockDevice, PAGE_SIZE, SECTOR, Segment, SegmentMut};
use radix_sync::Mutex;

use crate::mem::{self, KVec};
use crate::{APPEND, Error, In, Out, Result};

/// The most pages one request moves.
pub const MAX_PAGES: usize = 16;

/// A sector's bytes, as a `usize`.
const SECTOR_BYTES: usize = SECTOR as usize;

/// One request's worth of a transfer: from byte `at` of the device,
/// `want` bytes of the caller's, inside the whole sectors from `sector`
/// that make `bytes` bytes; the caller's start `skip` bytes into them.
struct Chunk {
    sector: u64,
    skip: usize,
    want: usize,
    bytes: usize,
}

impl Chunk {
    /// The chunk at byte `at` of up to `left` bytes, in pages of at most
    /// `pages`.
    fn at(at: u64, left: usize, pages: usize) -> Chunk {
        let skip = (at % SECTOR) as usize;
        let want = left.min(pages * PAGE_SIZE - skip);
        Chunk {
            sector: at / SECTOR,
            skip,
            want,
            bytes: (skip + want).next_multiple_of(SECTOR_BYTES),
        }
    }

    fn pages(&self) -> usize {
        self.bytes.div_ceil(PAGE_SIZE)
    }
}

#[cfg(kani)]
mod proofs {
    use super::*;

    /// A chunk starts in the sector holding its first byte, takes at
    /// least one of the bytes left and no more, and lies in whole sectors
    /// within the pages it may use.
    #[kani::proof]
    fn chunks_fit_their_pages() {
        let at: u64 = kani::any();
        let left: usize = kani::any_where(|&left: &usize| left > 0);
        let pages: usize = kani::any_where(|&pages: &usize| (1..=MAX_PAGES).contains(&pages));
        let chunk = Chunk::at(at, left, pages);
        assert_eq!(chunk.sector * SECTOR + chunk.skip as u64, at);
        assert!(chunk.want > 0 && chunk.want <= left);
        assert!(chunk.bytes.is_multiple_of(SECTOR_BYTES));
        assert!(chunk.skip + chunk.want <= chunk.bytes);
        assert!(chunk.bytes - (chunk.skip + chunk.want) < SECTOR_BYTES);
        assert!(chunk.pages() <= pages);
    }
}

/// How many pages a request to `device` may move.
fn request_pages(device: &dyn BlockDevice) -> usize {
    device.info().max_segments.clamp(1, MAX_PAGES)
}

/// Makes `pages` hold at least `count` pages.
fn grow(pages: &mut KVec<CachePage>, count: usize) -> Result<()> {
    while pages.len() < count {
        mem::push(pages, CachePage::new()?)?;
    }
    Ok(())
}

/// Reads the `bytes` bytes (whole sectors) from `sector` into `pages`,
/// from the first page's start.
fn read_pages(
    device: &dyn BlockDevice,
    pages: &mut [CachePage],
    sector: u64,
    bytes: usize,
) -> Result<()> {
    let mut segments = mem::vec_with_capacity(pages.len())?;
    let mut left = bytes;
    for page in pages.iter_mut() {
        let len = left.min(PAGE_SIZE);
        mem::push(&mut segments, SegmentMut { page, offset: 0, len })?;
        left -= len;
    }
    Ok(device.read(sector, &mut segments)?)
}

/// Writes the `bytes` bytes (whole sectors) of `pages` from `sector`.
fn write_pages(
    device: &dyn BlockDevice,
    pages: &[CachePage],
    sector: u64,
    bytes: usize,
) -> Result<()> {
    let mut segments = mem::vec_with_capacity(pages.len())?;
    let mut left = bytes;
    for page in pages {
        let len = left.min(PAGE_SIZE);
        mem::push(&mut segments, Segment { page, offset: 0, len })?;
        left -= len;
    }
    Ok(device.write(sector, &segments)?)
}

/// Reads sector `sector` into the chunk's pages, `at` bytes from their
/// start.
fn read_sector(
    device: &dyn BlockDevice,
    pages: &mut [CachePage],
    sector: u64,
    at: usize,
) -> Result<()> {
    let page = &mut pages[at / PAGE_SIZE];
    let segment = SegmentMut { page, offset: at % PAGE_SIZE, len: SECTOR_BYTES };
    Ok(device.read(sector, &mut [segment])?)
}

/// Reads from byte `offset` of `device` into `out`: as much as `out`
/// holds, fewer at the device's end. An error after some bytes came ends
/// the read short.
pub fn read(device: &dyn BlockDevice, offset: u64, out: &mut dyn Out) -> Result<usize> {
    let size = device.info().sectors * SECTOR;
    if offset >= size || out.is_empty() {
        return Ok(0);
    }
    let len = (out.len() as u64).min(size - offset) as usize;
    let most = request_pages(device);
    let mut pages = mem::vec();
    let mut done = 0;
    while done < len {
        let chunk = Chunk::at(offset + done as u64, len - done, most);
        let result = grow(&mut pages, chunk.pages())
            .and_then(|()| {
                read_pages(device, &mut pages[..chunk.pages()], chunk.sector, chunk.bytes)
            })
            .and_then(|()| put(&pages, chunk.skip, chunk.want, out, done));
        match result {
            Ok(()) => done += chunk.want,
            Err(_) if done > 0 => return Ok(done),
            Err(err) => return Err(err),
        }
    }
    Ok(len)
}

/// Copies `want` bytes from byte `skip` of `pages` to `out`, at `to`.
fn put(pages: &[CachePage], skip: usize, want: usize, out: &mut dyn Out, to: usize) -> Result<()> {
    let mut copied = 0;
    while copied < want {
        let at = skip + copied;
        let (index, within) = (at / PAGE_SIZE, at % PAGE_SIZE);
        let n = (PAGE_SIZE - within).min(want - copied);
        out.put(to + copied, &pages[index].bytes()[within..within + n])?;
        copied += n;
    }
    Ok(())
}

/// Copies `want` bytes of `data` from `from` to byte `skip` of `pages`.
fn get(
    pages: &mut [CachePage],
    skip: usize,
    want: usize,
    data: &dyn In,
    from: usize,
) -> Result<()> {
    let mut copied = 0;
    while copied < want {
        let at = skip + copied;
        let (index, within) = (at / PAGE_SIZE, at % PAGE_SIZE);
        let n = (PAGE_SIZE - within).min(want - copied);
        data.get(from + copied, &mut pages[index].bytes_mut()[within..within + n])?;
        copied += n;
    }
    Ok(())
}

/// Writes `data` at byte `offset` of `device`: as much as fits before
/// its end, and none at or past it ([`Error::NoSpace`], as Linux has
/// it). `partial` keeps writes of parts of sectors apart. An error after
/// some bytes went ends the write short.
pub fn write(
    device: &dyn BlockDevice,
    partial: &Mutex<()>,
    offset: u64,
    data: &dyn In,
) -> Result<usize> {
    let info = device.info();
    if info.read_only {
        return Err(Error::ReadOnly);
    }
    if data.is_empty() {
        return Ok(0);
    }
    let size = info.sectors * SECTOR;
    if offset == APPEND || offset >= size {
        return Err(Error::NoSpace);
    }
    let len = (data.len() as u64).min(size - offset) as usize;
    let most = request_pages(device);
    let mut pages = mem::vec();
    let mut done = 0;
    while done < len {
        let chunk = Chunk::at(offset + done as u64, len - done, most);
        let result = grow(&mut pages, chunk.pages()).and_then(|()| {
            let pages = &mut pages[..chunk.pages()];
            let head = chunk.skip != 0;
            let tail = !(chunk.skip + chunk.want).is_multiple_of(SECTOR_BYTES);
            if !head && !tail {
                get(pages, 0, chunk.want, data, done)?;
                return write_pages(device, pages, chunk.sector, chunk.bytes);
            }
            let _partial = partial.lock();
            if head {
                read_sector(device, pages, chunk.sector, 0)?;
            }
            let last = chunk.bytes - SECTOR_BYTES;
            if tail && !(head && last == 0) {
                read_sector(device, pages, chunk.sector + (last / SECTOR_BYTES) as u64, last)?;
            }
            get(pages, chunk.skip, chunk.want, data, done)?;
            write_pages(device, pages, chunk.sector, chunk.bytes)
        });
        match result {
            Ok(()) => done += chunk.want,
            Err(_) if done > 0 => return Ok(done),
            Err(err) => return Err(err),
        }
    }
    Ok(len)
}
