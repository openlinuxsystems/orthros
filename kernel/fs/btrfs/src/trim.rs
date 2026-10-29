// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Trimming (ADR 0158), as Linux's FITRIM does for fstrim(8): each disk
//! is told, by a discard, which of its bytes the filesystem does not use,
//! so that a solid state disk can erase them ahead of time.
//!
//! Two kinds of bytes are unused: the free space of each block group, as
//! the transaction may take it now (not what a commit still pins, nor the
//! blocks of a log), on every copy of it; and each disk's bytes no chunk
//! takes. Each batch of them is found and discarded under the running
//! transaction, so that no change takes them meanwhile, and the
//! transaction is let go between batches, so that changes wait for one
//! batch at most. The start of every disk and its superblock's copies are
//! never discarded, whatever the free space says.

use radix_vfs::{Error, Result};

use crate::fs::Inner;
use crate::scrub::SUPERS;
use crate::volume::Volume;

/// The most bytes found to discard under the transaction at once.
const BATCH: u64 = 128 << 20;

/// What is never discarded on a disk around the copy of the superblock
/// at `at`: the disk's first MiB for the first, which no chunk takes
/// (room for a boot loader and the superblock), and 64 KiB at each other
/// copy, as free space keeps them from use.
fn kept(at: u64) -> (u64, u64) {
    const START: u64 = 1 << 20;
    const GUARD: u64 = 64 << 10;
    if at < START { (0, START) } else { (at, at + GUARD) }
}

/// Discards the unused bytes of the filesystem `inner` is, in ranges of
/// at least `minimum` bytes: returns how many bytes it discarded on all
/// the disks, each copy counted, as Linux counts them.
///
/// Fails with [`Error::ReadOnly`] on a read-only mount, and with
/// [`Error::NotSupported`] when no disk that is there discards. A disk
/// that fails a discard is passed over, and the trim goes on: it fails
/// with the first such error at the end.
pub(crate) fn trim(inner: &Inner, minimum: u64) -> Result<u64> {
    let volume = &*inner.volume;
    if !inner.writable() {
        return Err(Error::ReadOnly);
    }
    let discards =
        volume.disks().iter().filter_map(|d| d.device()).any(|d| d.info().discard_sectors > 0);
    if !discards {
        return Err(Error::NotSupported);
    }
    let mut trim = Trim { volume, trimmed: 0, error: None };
    // The block groups' free space, from the first logical address on.
    let mut from = Some(0);
    while let Some(at) = from {
        let txn = inner.reading().ok_or(Error::ReadOnly)?;
        let (ranges, next) = txn.writer.space.free_from(at, minimum, BATCH)?;
        for &(start, end) in &ranges {
            trim.logical(start, end)?;
        }
        drop(txn);
        from = next;
    }
    // The disks' bytes no chunk takes, a disk at a time from its start.
    let mut from = Some((0, 0));
    while let Some((devid, at)) = from {
        let mut txn = inner.reading().ok_or(Error::ReadOnly)?;
        let mut holes = txn.writer.unallocated(volume)?;
        holes.sort_unstable();
        let mut bytes = 0;
        from = None;
        for &(id, start, end) in holes.iter().filter(|h| (h.0, h.2) > (devid, at)) {
            // What a batch cut short of the limit goes on with.
            let going_on = id == devid && start < at;
            let start = if going_on { at } else { start };
            if !going_on && end - start < minimum {
                continue;
            }
            let end = end.min(start + (BATCH - bytes));
            trim.physical(id, start, end);
            bytes += end - start;
            if bytes >= BATCH {
                from = Some((id, end));
                break;
            }
        }
        drop(txn);
    }
    match trim.error {
        Some(error) => Err(error),
        None => Ok(trim.trimmed),
    }
}

/// A trim under way: what it discarded, and the first discard that failed.
struct Trim<'a> {
    volume: &'a Volume,
    trimmed: u64,
    error: Option<Error>,
}

impl Trim<'_> {
    /// Discards every copy of the logical range `[start, end)`, on the
    /// disks that are there.
    fn logical(&mut self, start: u64, end: u64) -> Result<()> {
        let mut at = start;
        while at < end {
            let place = self.volume.place(at)?;
            let len = place.len.min(end - at);
            if len == 0 {
                return Err(Error::Corrupt);
            }
            for site in place.copies() {
                let devid = self.volume.disk(site.disk)?.devid;
                self.physical(devid, site.at, site.at + len);
            }
            at += len;
        }
        Ok(())
    }

    /// Discards `[start, end)` on disk `devid`, but for what [`kept`]
    /// keeps, in the disk's whole logical blocks.
    fn physical(&mut self, devid: u64, start: u64, end: u64) {
        let Some(disk) = self.volume.disks().iter().find(|d| d.devid == devid) else { return };
        let Some(device) = disk.device() else { return };
        let info = device.info();
        if info.discard_sectors == 0 || info.read_only {
            return;
        }
        let end = end.min(info.sectors * radix_block::SECTOR);
        let mut parts = [(start, end); SUPERS.len() + 1];
        let mut count = 1;
        for (s, e) in SUPERS.into_iter().map(kept) {
            // Each part left once [s, e) is cut out of it.
            let mut next = [(0, 0); SUPERS.len() + 1];
            let mut n = 0;
            for &(a, b) in &parts[..count] {
                for (x, y) in [(a, b.min(s)), (a.max(e), b)] {
                    if x < y && n < next.len() {
                        next[n] = (x, y);
                        n += 1;
                    }
                }
            }
            (parts, count) = (next, n);
        }
        let block = u64::from(info.logical_block).max(radix_block::SECTOR);
        for &(a, b) in &parts[..count] {
            let (a, b) = (a.next_multiple_of(block), b / block * block);
            if a >= b {
                continue;
            }
            let sector = radix_block::SECTOR;
            match device.discard(a / sector, (b - a) / sector) {
                Ok(()) => self.trimmed += b - a,
                // A disk that discards nothing, after all.
                Err(radix_block::Error::Unsupported) => {}
                Err(e) => {
                    self.error.get_or_insert(e.into());
                }
            }
        }
    }
}
