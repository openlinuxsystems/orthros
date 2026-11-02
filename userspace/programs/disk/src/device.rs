// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! What the tools read and write: a disk, a partition of one, or a
//! file that holds a disk image, as a run of bytes of a known length in
//! logical sectors of a known size.

use std::fs::File;
use std::io;
use std::os::unix::fs::FileExt;

/// The most bytes written at once: zeroing a large FAT goes in pieces.
const CHUNK: usize = 1 << 20;

/// A disk, a partition or an image: `len` bytes of `file` from `base`.
#[derive(Debug)]
pub struct Device {
    file: File,
    base: u64,
    len: u64,
    sector: u32,
}

impl Device {
    /// The whole of `file`, `len` bytes in logical sectors of `sector`
    /// bytes (512 or 4096): a disk, or an image of one.
    pub fn whole(file: File, len: u64, sector: u32) -> Device {
        Device { file, base: 0, len, sector }
    }

    /// `sectors` logical sectors of this device from `first`: a partition
    /// of a disk image, as `./ob image` formats them.
    ///
    /// # Errors
    ///
    /// If the file cannot be opened again, or the part is not within.
    pub fn part(&self, first: u64, sectors: u64) -> io::Result<Device> {
        let sector = u64::from(self.sector);
        let (base, len) = (first.saturating_mul(sector), sectors.saturating_mul(sector));
        if base.checked_add(len).is_none_or(|end| end > self.len) {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "the part is not on the device"));
        }
        Ok(Device { file: self.file.try_clone()?, base: self.base + base, len, sector: self.sector })
    }

    /// Its size in bytes.
    pub fn len(&self) -> u64 {
        self.len
    }

    /// Whether it has no bytes.
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// The size of its logical sectors.
    pub fn sector_size(&self) -> u32 {
        self.sector
    }

    /// How many logical sectors it has.
    pub fn sectors(&self) -> u64 {
        self.len / u64::from(self.sector)
    }

    /// Where it starts on its file, in bytes: 0 for a whole disk.
    pub fn base(&self) -> u64 {
        self.base
    }

    fn check(&self, offset: u64, len: usize) -> io::Result<u64> {
        match offset.checked_add(len as u64) {
            Some(end) if end <= self.len => Ok(self.base + offset),
            _ => Err(io::Error::new(io::ErrorKind::UnexpectedEof, "past the end of the device")),
        }
    }

    /// Reads `out.len()` bytes at `offset`.
    ///
    /// # Errors
    ///
    /// If they are not all on the device, or cannot be read.
    pub fn read_at(&self, offset: u64, out: &mut [u8]) -> io::Result<()> {
        let at = self.check(offset, out.len())?;
        self.file.read_exact_at(out, at)
    }

    /// `count` logical sectors from `lba`.
    ///
    /// # Errors
    ///
    /// As [`Device::read_at`].
    pub fn read_sectors(&self, lba: u64, count: u64) -> io::Result<Vec<u8>> {
        let sector = u64::from(self.sector);
        let len = usize::try_from(count * sector)
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "too many sectors"))?;
        let mut out = vec![0u8; len];
        self.read_at(lba * sector, &mut out)?;
        Ok(out)
    }

    /// Writes `data` at `offset`.
    ///
    /// # Errors
    ///
    /// If it does not fit on the device, or cannot be written.
    pub fn write_at(&self, offset: u64, data: &[u8]) -> io::Result<()> {
        let at = self.check(offset, data.len())?;
        for (i, chunk) in data.chunks(CHUNK).enumerate() {
            self.file.write_all_at(chunk, at + (i * CHUNK) as u64)?;
        }
        Ok(())
    }

    /// Writes `data` at logical sector `lba`.
    ///
    /// # Errors
    ///
    /// As [`Device::write_at`].
    pub fn write_sectors(&self, lba: u64, data: &[u8]) -> io::Result<()> {
        self.write_at(lba * u64::from(self.sector), data)
    }

    /// Writes `len` zeros at `offset`.
    ///
    /// # Errors
    ///
    /// As [`Device::write_at`].
    pub fn zero(&self, offset: u64, len: u64) -> io::Result<()> {
        self.check(offset, 0)?;
        if offset.checked_add(len).is_none_or(|end| end > self.len) {
            return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "past the end of the device"));
        }
        let zeros = vec![0u8; CHUNK.min(usize::try_from(len).unwrap_or(CHUNK))];
        let mut done = 0;
        while done < len {
            let n = (len - done).min(zeros.len() as u64) as usize;
            self.write_at(offset + done, &zeros[..n])?;
            done += n as u64;
        }
        Ok(())
    }

    /// Makes what was written last: the disk empties its write cache.
    ///
    /// # Errors
    ///
    /// If the system reports the writes failed.
    pub fn sync(&self) -> io::Result<()> {
        self.file.sync_all()
    }

    /// The file, for requests of the device's own.
    pub fn file(&self) -> &File {
        &self.file
    }
}
