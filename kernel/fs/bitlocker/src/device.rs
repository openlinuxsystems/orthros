// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! A BitLocker volume unlocked, as a block device of its own (for the
//! kernel): its sectors decrypted as they are read and encrypted as they
//! are written, its first sectors read from and written to their copy,
//! and its metadata reading as zeros and never written.
//!
//! A volume Windows is encrypting or decrypting, part of it encrypted and
//! the rest not, is only read: Windows goes on from where it stopped, and
//! must find each part as it left it.

use radix_block::mem::{self, CachePage, KBox};
use radix_block::{
    BlockDevice, DeviceRef, Error as BlockError, Health, Info, MAX_LOGICAL_BLOCK, Page, SECTOR,
    Segment, SegmentMut, check,
};

use crate::cipher::{Aes, Method, Sectors};
use crate::format::{self, ENTRIES, Error, Layout, Metadata, Place, Volume};

/// Why a volume was not unlocked.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OpenError {
    /// Its BitLocker: not BitLocker, damaged, not read, or the key not its.
    BitLocker(Error),
    /// Its device failed, or memory ran out.
    Device(BlockError),
}

impl From<Error> for OpenError {
    fn from(err: Error) -> OpenError {
        OpenError::BitLocker(err)
    }
}

impl From<BlockError> for OpenError {
    fn from(err: BlockError) -> OpenError {
        OpenError::Device(err)
    }
}

impl From<mem::NoMemory> for OpenError {
    fn from(_: mem::NoMemory) -> OpenError {
        OpenError::Device(BlockError::NoMemory)
    }
}

/// A BitLocker volume, unlocked.
pub struct Unlocked {
    device: DeviceRef,
    layout: Layout,
    method: Method,
    data: KBox<Aes>,
    tweak: Option<KBox<Aes>>,
    info: Info,
    /// The 512-byte sectors of its logical blocks.
    unit: u64,
}

impl Unlocked {
    /// The BitLocker volume on `device`, unlocked with its key `fvek` of
    /// the method numbered `method`, as unlock(8) unwrapped them; only
    /// read if `read_only`. Fails with [`Error::NotBitLocker`] if it is no
    /// BitLocker volume, [`Error::NotSupported`] if it is one not read,
    /// [`Error::Damaged`] if no copy of its metadata reads, and
    /// [`Error::WrongKey`] if the key is not its: if its first sector,
    /// decrypted, is no boot sector.
    pub fn open(
        device: DeviceRef,
        method: u16,
        fvek: &[u8],
        read_only: bool,
    ) -> Result<Unlocked, OpenError> {
        let (layout, volume_method) = layout_of(&device)?;
        if method != volume_method {
            return Err(Error::WrongKey.into());
        }
        let method = Method::from_number(method).ok_or(Error::NotSupported)?;
        let (data, tweak) = method.keys(fvek).ok_or(Error::WrongKey)?;
        let data = expand(data)?;
        let tweak = match tweak {
            Some(tweak) => Some(expand(tweak)?),
            None => None,
        };
        let inner = device.info();
        let logical = inner.logical_block.max(layout.sector as u32);
        let fits = |n: u64| n.is_multiple_of(u64::from(logical));
        if logical > MAX_LOGICAL_BLOCK || !fits(layout.header) || !fits(layout.header_size) {
            return Err(Error::NotSupported.into());
        }
        let info = Info {
            logical_block: logical,
            block_size: inner.block_size.max(logical),
            read_only: inner.read_only || read_only || !layout.whole(),
            discard_sectors: 0,
            ..inner
        };
        let unit = u64::from(logical) / SECTOR;
        let unlocked = Unlocked { device, layout, method, data, tweak, info, unit };
        Sectors::new(method, &unlocked.data, unlocked.tweak.as_deref(), layout.sector)
            .ok_or(Error::WrongKey)?;
        unlocked.check_key()?;
        Ok(unlocked)
    }

    /// Where its parts are.
    pub fn layout(&self) -> &Layout {
        &self.layout
    }

    /// How it is encrypted.
    pub fn method(&self) -> Method {
        self.method
    }

    /// Fails with [`Error::WrongKey`] unless its first sector, decrypted,
    /// is a boot sector, as every filesystem BitLocker encrypts has.
    #[inline(never)]
    fn check_key(&self) -> Result<(), OpenError> {
        let mut first = [0u8; 512];
        radix_block::read_sector(self, 0, &mut first)?;
        if first[510..] != [0x55, 0xaa] {
            return Err(Error::WrongKey.into());
        }
        Ok(())
    }

    fn sectors(&self) -> Result<Sectors<'_>, BlockError> {
        Sectors::new(self.method, &self.data, self.tweak.as_deref(), self.layout.sector)
            .ok_or(BlockError::Io)
    }

    /// Reads the part of `segment`, which is at `at` on the volume, that
    /// is among its first sectors from their copy.
    fn read_moved(&self, at: u64, segment: &mut SegmentMut<'_>) -> Result<(), BlockError> {
        let len = (self.layout.header_size - at).min(segment.len as u64) as usize;
        let mut moved = [SegmentMut { page: &mut *segment.page, offset: segment.offset, len }];
        self.device.read((self.layout.header + at) / SECTOR, &mut moved)
    }

    /// Writes `segments` at `sector`, as [`BlockDevice::write`] does,
    /// encrypted; through to the disk if `through`.
    fn put(&self, sector: u64, segments: &[Segment<'_>], through: bool) -> Result<(), BlockError> {
        if self.info.read_only {
            return Err(BlockError::ReadOnly);
        }
        let lengths = segments.iter().map(|s| (s.offset, s.len));
        check(self.info.sectors, self.unit, usize::MAX, sector, lengths)?;
        let sectors = self.sectors()?;
        let size = self.layout.sector;
        let mut pages = mem::vec_with_capacity(segments.len())?;
        let mut at = sector * SECTOR;
        for segment in segments {
            let mut page = CachePage::new()?;
            let range = segment.offset..segment.offset + segment.len;
            let out = &mut page.bytes_mut()[range.clone()];
            out.copy_from_slice(&segment.page.bytes()[range]);
            for chunk in out.chunks_exact_mut(size as usize) {
                match self.layout.place(at) {
                    Place::Moved(to) => sectors.encrypt(to, chunk),
                    Place::Encrypted => sectors.encrypt(at, chunk),
                    // Its filesystem does not write there: Windows' files
                    // hold the metadata's places.
                    Place::Hidden => return Err(BlockError::Io),
                    // A volume part encrypted is only read.
                    Place::Plain => return Err(BlockError::ReadOnly),
                }
                at += size;
            }
            // Room was made above.
            let _ = pages.push_within_capacity(page);
        }
        let start = sector * SECTOR;
        if start >= self.layout.header_size {
            let mut encrypted = mem::vec_with_capacity(segments.len())?;
            for (segment, page) in segments.iter().zip(&pages) {
                let page: &Page = page;
                let _ = encrypted.push_within_capacity(Segment { page, ..*segment });
            }
            return write(&*self.device, sector, &encrypted, through);
        }
        // Its first sectors go to their copy, a segment at a time.
        let mut at = start;
        for (segment, page) in segments.iter().zip(&pages) {
            let header = self.layout.header_size;
            let moved =
                if at < header { (header - at).min(segment.len as u64) as usize } else { 0 };
            if moved > 0 {
                let part = [Segment { page, offset: segment.offset, len: moved }];
                write(&*self.device, (self.layout.header + at) / SECTOR, &part, through)?;
            }
            if moved < segment.len {
                let (offset, len) = (segment.offset + moved, segment.len - moved);
                let part = [Segment { page, offset, len }];
                write(&*self.device, (at + moved as u64) / SECTOR, &part, through)?;
            }
            at += segment.len as u64;
        }
        Ok(())
    }
}

/// `segments` written to `device` at `sector`, through to the disk if
/// `through`.
fn write(
    device: &dyn BlockDevice,
    sector: u64,
    segments: &[Segment<'_>],
    through: bool,
) -> Result<(), BlockError> {
    if through { device.write_through(sector, segments) } else { device.write(sector, segments) }
}

/// An AES key expanded, on the heap: one of 256 bits takes 960 bytes in
/// software, and its expansion as much again, so each size is expanded
/// in a frame of its own (the kernel's are held to 2 KiB, ADR 0058).
#[inline(never)]
fn expand(key: &[u8]) -> Result<KBox<Aes>, OpenError> {
    match key.len() {
        16 => expand_128(key),
        32 => expand_256(key),
        _ => Err(Error::WrongKey.into()),
    }
}

#[inline(never)]
fn expand_128(key: &[u8]) -> Result<KBox<Aes>, OpenError> {
    Ok(mem::boxed(Aes::new_128(key).ok_or(Error::WrongKey)?)?)
}

#[inline(never)]
fn expand_256(key: &[u8]) -> Result<KBox<Aes>, OpenError> {
    Ok(mem::boxed(Aes::new_256(key).ok_or(Error::WrongKey)?)?)
}

/// The layout of the BitLocker volume on `device`, and its method's
/// number, from the first copy of its metadata that reads.
#[inline(never)]
fn layout_of(device: &DeviceRef) -> Result<(Layout, u16), OpenError> {
    let mut boot = [0u8; 512];
    radix_block::read_sector(&**device, 0, &mut boot)?;
    let (blocks, sector) = match format::volume(&boot) {
        Some(Volume::Locked { metadata, sector }) => (metadata, sector),
        Some(Volume::Vista) => return Err(Error::NotSupported.into()),
        None => return Err(Error::NotBitLocker.into()),
    };
    let size = device.info().sectors * SECTOR;
    let mut last = Error::Damaged;
    for offset in blocks {
        let Ok(block) = read_block(device, offset, size) else { continue };
        match Metadata::parse(&block, sector)
            .and_then(|m| Ok((Layout::new(&m, size, sector)?, m.method)))
        {
            Ok(found) => return Ok(found),
            Err(err) => last = err,
        }
    }
    Err(last.into())
}

/// The metadata block at `offset` of `device`, of `size` bytes.
fn read_block(device: &DeviceRef, offset: u64, size: u64) -> Result<mem::KVec<u8>, OpenError> {
    let head = read_bytes(device, offset, ENTRIES, size)?;
    let len = format::block_size(&head).ok_or(Error::Damaged)?;
    read_bytes(device, offset, len, size)
}

/// `len` bytes at `offset` of `device`, of `size` bytes, read a page at a
/// time.
fn read_bytes(
    device: &DeviceRef,
    offset: u64,
    len: usize,
    size: u64,
) -> Result<mem::KVec<u8>, OpenError> {
    let block = u64::from(device.info().logical_block);
    let start = offset - offset % block;
    let end = (offset + len as u64).next_multiple_of(block);
    if offset.checked_add(len as u64).is_none_or(|e| e > size) || end > size {
        return Err(Error::Damaged.into());
    }
    let mut out = mem::vec_with_capacity(len)?;
    let mut page = CachePage::new()?;
    let mut at = start;
    while at < end {
        let n = (end - at).min(radix_block::PAGE_SIZE as u64) as usize;
        device.read(at / SECTOR, &mut [SegmentMut { page: &mut page, offset: 0, len: n }])?;
        let from = offset.max(at) - at;
        let to = (offset + len as u64).min(at + n as u64) - at;
        if from < to {
            mem::extend(&mut out, &page.bytes()[from as usize..to as usize])?;
        }
        at += n as u64;
    }
    Ok(out)
}

impl BlockDevice for Unlocked {
    fn info(&self) -> Info {
        self.info
    }

    fn read(&self, sector: u64, segments: &mut [SegmentMut<'_>]) -> Result<(), BlockError> {
        let lengths = segments.iter().map(|s| (s.offset, s.len));
        check(self.info.sectors, self.unit, usize::MAX, sector, lengths)?;
        let sectors = self.sectors()?;
        self.device.read(sector, segments)?;
        let size = self.layout.sector;
        let mut at = sector * SECTOR;
        for segment in segments.iter_mut() {
            if at < self.layout.header_size {
                self.read_moved(at, segment)?;
            }
            let range = segment.offset..segment.offset + segment.len;
            for chunk in segment.page.bytes_mut()[range].chunks_exact_mut(size as usize) {
                match self.layout.place(at) {
                    Place::Moved(from) => sectors.decrypt(from, chunk),
                    Place::Hidden => chunk.fill(0),
                    Place::Encrypted => sectors.decrypt(at, chunk),
                    Place::Plain => {}
                }
                at += size;
            }
        }
        Ok(())
    }

    fn write(&self, sector: u64, segments: &[Segment<'_>]) -> Result<(), BlockError> {
        self.put(sector, segments, false)
    }

    fn write_through(&self, sector: u64, segments: &[Segment<'_>]) -> Result<(), BlockError> {
        self.put(sector, segments, true)
    }

    fn flush(&self) -> Result<(), BlockError> {
        self.device.flush()
    }

    fn health(&self, page: &mut Page) -> Result<Health, BlockError> {
        self.device.health(page)
    }
}
