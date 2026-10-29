// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Replacing a disk (ADR 0155), as Linux's `btrfs replace start -B`
//! does: a new disk takes the place of one of the filesystem's, missing
//! or there, with its device ID and UUID, and is rebuilt from the copies
//! on the others. It is a disk that missed every commit: read last, and
//! caught up by a scrub (ADR 0153), which then writes it its superblock.
//! Until then it has none, so a mount after a power cut finds the disk
//! it replaced missing still, and the replace is run again.

use radix_block::DeviceRef;
use radix_block::cache::BlockCache;
use radix_vfs::{Error, Result, Scrub};

use crate::format::{Key, item, u64_at};
use crate::fs::Inner;
use crate::scrub::SUPERS;
use crate::superblock;
use crate::tree::Cursor;
use crate::volume::{DEV_ITEMS, Volume};

/// Replaces the disk of device ID `devid` with `device` and rebuilds onto
/// it, returning what the rebuild's scrub read and found. The disk it
/// replaced, if it was there, has its superblocks wiped once the new one
/// holds every copy, so that it is not taken for one of the filesystem's
/// again.
///
/// Fails with [`Error::ReadOnly`] on a read-only mount, [`Error::NotFound`]
/// when no disk has that ID, [`Error::InUse`] when `device` is one of the
/// filesystem's disks or the disk was replaced already since the mount,
/// [`Error::NoSpace`] when `device` is smaller than the disk's device
/// item says, and [`Error::NotSupported`] when the other disks lack a
/// copy of a chunk on it (Linux copies those from the disk itself, which
/// may be missing) or `device`'s blocks are larger than the
/// filesystem's sectors.
pub(crate) fn replace(inner: &Inner, devid: u64, device: &DeviceRef) -> Result<Scrub> {
    let volume = &*inner.volume;
    if !inner.writable() {
        return Err(Error::ReadOnly);
    }
    let index = volume.disks().iter().position(|d| d.devid == devid).ok_or(Error::NotFound)?;
    if volume.disks().iter().filter_map(|d| d.device()).any(|d| DeviceRef::ptr_eq(d, device)) {
        return Err(Error::InUse);
    }
    let info = device.info();
    if info.logical_block > volume.superblock().sectorsize {
        return Err(Error::NotSupported);
    }
    if info.sectors * radix_block::SECTOR < total_bytes(volume, devid)? {
        return Err(Error::NoSpace);
    }
    let present = |other: u64| {
        other != devid && volume.disks().iter().any(|d| d.devid == other && d.present())
    };
    if !volume.with_chunks(|map| map.chunks().iter().all(|c| c.readable(present))) {
        return Err(Error::NotSupported);
    }
    // A superblock the device held before, of another filesystem or of
    // this one, would be taken for its own after a power cut.
    wipe(&BlockCache::new(device.clone()))?;
    {
        // What the running transaction wrote is on the disks it had: its
        // commit comes first, and changes from then on reach the new disk.
        let mut txn = inner.reading().ok_or(Error::ReadOnly)?;
        inner.commit_locked(&mut txn)?;
        volume.replace(index, device)?;
    }
    let disk = volume.disk(index)?;
    let counts = match crate::scrub::scrub(inner, true) {
        Ok(counts) => counts,
        // A new disk that fails, or that the rebuild could not fill, is
        // left out. The disk it replaced, if it was there, has kept its
        // superblock, and comes back at the next mount, behind.
        Err(e) => {
            if disk.behind() {
                let _ = volume.write_failed(index, e);
            }
            return Err(e);
        }
    };
    // Wiped once the new disk has it all, as Linux's replace does; a
    // disk failing may refuse it.
    if disk.present()
        && !disk.behind()
        && let Some(old) = disk.replaced()
    {
        let _ = wipe(old);
    }
    Ok(counts)
}

/// The size the device item of disk `devid` gives it.
fn total_bytes(volume: &Volume, devid: u64) -> Result<u64> {
    let key = Key::new(DEV_ITEMS, item::DEV_ITEM, devid);
    let cursor = Cursor::seek(volume, &volume.chunk_root(), &key)?;
    match cursor.get() {
        Some((found, data)) if found == key && data.len() >= 16 => Ok(u64_at(data, 8)),
        _ => Err(Error::Corrupt),
    }
}

/// Writes zeros over each copy of a superblock that fits on `cache`'s
/// disk, and has the disk keep them.
fn wipe(cache: &BlockCache) -> Result<()> {
    let size = cache.device().info().sectors * radix_block::SECTOR;
    for at in SUPERS.into_iter().filter(|&at| at + superblock::SIZE as u64 <= size) {
        cache.fill(at, superblock::SIZE, 0)?;
    }
    cache.sync()?;
    cache.device().flush()?;
    Ok(())
}
