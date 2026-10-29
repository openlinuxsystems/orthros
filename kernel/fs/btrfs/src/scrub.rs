// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Scrub (ADR 0150): every block the last commit holds is read from the
//! disk, every copy of it, and checked, as Linux's `btrfs scrub` checks
//! it: a tree block as a read checks one (its checksum, then its header
//! and items), a data sector against the checksum tree, and the copies
//! of the superblock. A good copy is written over a bad one if asked.
//! What it finds is counted as Linux's `struct btrfs_scrub_progress`
//! counts it, a sector at a time: a tree block of 16 KiB with a bad
//! checksum is four checksum errors.
//!
//! The walk is of the extent tree of the last commit, whose blocks no
//! change takes for itself again before the next commit. On a writable
//! mount it holds the transaction for a run of extents at a time
//! ([`RUN_BYTES`]), so no commit comes between finding an extent and
//! reading it, and a change waits no longer than a run takes.
//!
//! On a filesystem of several disks it reads the copies on each, and the
//! superblocks of each (ADR 0153). A disk that missed commits while it
//! was away has old copies: a checksum or a header finds them, as any
//! other bad copy, and a scrub that writes the good copies over them
//! brings it up to date; data without checksums is written over from a
//! disk that did not miss them.

use radix_block::mem::CachePage;
use radix_block::{PAGE_SIZE, SECTOR, Segment, SegmentMut};
use radix_vfs::mem::{self, KVec};
use radix_vfs::{Error, Result, Scrub};

use crate::chunk::{MAX_COPIES, Site};
use crate::cow::Writer;
use crate::csum::CSUM_SIZE;
use crate::extent;
use crate::format::{Key, RootItem, item, tree, u64_at, uuid_at};
use crate::fs::Inner;
use crate::superblock::{self, Superblock};
use crate::tree::{self as trees, Cursor, Fault, Root};
use crate::volume::{Disk, Volume};

/// The bytes of extents a run reads while it holds the transaction.
const RUN_BYTES: u64 = 16 << 20;

/// The data sectors read at once.
const SECTORS: usize = 32;

/// A page, as a `u64`.
const PAGE: u64 = PAGE_SIZE as u64;

/// The flag of an extent item of a tree block
/// (`BTRFS_EXTENT_FLAG_TREE_BLOCK`).
const TREE_BLOCK: u64 = 1 << 1;

/// Where the superblock's copies are on a device.
const SUPERS: [u64; 3] = [superblock::OFFSET, 64 << 20, 256 << 30];

/// Scrubs the filesystem `inner` is, writing a good copy over a bad one
/// if `repair`, which a read-only mount refuses.
pub(crate) fn scrub(inner: &Inner, repair: bool) -> Result<Scrub> {
    let volume = &*inner.volume;
    if repair && !inner.writable() {
        return Err(Error::ReadOnly);
    }
    if u64::from(volume.superblock().sectorsize) != PAGE {
        return Err(Error::NotSupported);
    }
    let mut counts = Scrub { fsid: volume.superblock().fsid, ..Scrub::default() };
    let mut pages = mem::vec();
    {
        let _txn = inner.reading();
        supers(volume, &mut counts)?;
    }
    let mut next = 0;
    loop {
        let _txn = inner.reading();
        if run(volume, &mut next, repair, &mut pages, &mut counts)? {
            break;
        }
    }
    if repair {
        for device in volume.disks().iter().filter_map(|d| d.device()) {
            device.flush()?;
        }
        if counts.uncorrectable_errors == 0 && volume.disks().iter().any(Disk::behind) {
            // Their superblocks still name the commits they missed, as
            // commits leave a disk behind alone: caught up, they are
            // written. A log of fsyncs is in no superblock the commit
            // wrote, so a commit writes them then, and drops the log (ADR
            // 0154).
            let mut txn = inner.reading();
            volume.caught_up();
            match txn.as_deref_mut() {
                Some(txn) if txn.writer.logging.written() => inner.commit_locked(txn)?,
                _ => Writer::write_superblocks(volume, &volume.superblock_bytes()?, true)?,
            }
        }
    }
    Ok(counts)
}

/// Checks each copy of the superblock that fits on each disk: its
/// checksum, its address, and that it is the last commit's.
#[inline(never)]
fn supers(volume: &Volume, counts: &mut Scrub) -> Result<()> {
    let generation = Superblock::read(&volume.superblock_bytes()?)?.generation;
    let (csum, fsid) = (volume.superblock().csum_type, volume.superblock().fsid);
    let mut page = CachePage::new()?;
    let disks = volume.disks().iter().enumerate().filter(|(_, d)| d.present());
    for (at, disk) in
        disks.flat_map(|(i, d)| SUPERS.iter().map(move |&at| (Site { disk: i, at }, d)))
    {
        if !disk.holds(at.at, superblock::SIZE as u64) {
            continue;
        }
        let read = read_pages(volume, at, core::slice::from_mut(&mut page));
        // As Linux's: its checksum, magic, address, filesystem and
        // generation.
        let good = read.is_ok() && {
            let bytes = &page.bytes()[..superblock::SIZE];
            csum.matches(&bytes[CSUM_SIZE..], &bytes[..CSUM_SIZE])
                && u64_at(bytes, 0x40) == superblock::MAGIC
                && u64_at(bytes, 0x30) == at.at
                && uuid_at(bytes, 0x20) == fsid
                && u64_at(bytes, 0x48) == generation
        };
        if !good {
            counts.super_errors += 1;
        }
    }
    Ok(())
}

/// The roots of the extent tree and the checksum tree of the last
/// commit.
fn committed(volume: &Volume) -> Result<(Root, Option<Root>)> {
    let sb = Superblock::read(&volume.superblock_bytes()?)?;
    let roots = Root { bytenr: sb.root, level: sb.root_level, generation: sb.generation };
    let root_of = |id: u64| -> Result<Option<Root>> {
        let cursor = Cursor::seek(volume, &roots, &Key::first(id, item::ROOT_ITEM))?;
        let Some((key, data)) = cursor.get() else { return Ok(None) };
        if !key.is(id, item::ROOT_ITEM) {
            return Ok(None);
        }
        let item = RootItem::read(data).ok_or(Error::Corrupt)?;
        Ok(Some(Root { bytenr: item.bytenr, level: item.level, generation: item.generation }))
    };
    Ok((root_of(tree::EXTENT)?.ok_or(Error::Corrupt)?, root_of(tree::CSUM)?))
}

/// Scrubs the extents of the last commit from address `next` on, up to
/// [`RUN_BYTES`] of them, and moves `next` past them; says whether none
/// is left.
#[inline(never)]
fn run(
    volume: &Volume,
    next: &mut u64,
    repair: bool,
    pages: &mut KVec<CachePage>,
    counts: &mut Scrub,
) -> Result<bool> {
    let (extents, csums) = committed(volume)?;
    let nodesize = u64::from(volume.superblock().nodesize);
    let mut cursor = Cursor::seek(volume, &extents, &Key::first(*next, 0))?;
    let mut read = 0;
    while read < RUN_BYTES {
        let Some((key, data)) = cursor.get() else { return Ok(true) };
        let bytenr = key.objectid;
        // A tree block's level is in its key (a skinny one) or after its
        // item's header and the first key it holds.
        let block = |level: u8| Root { bytenr, level, generation: u64_at(data, 8) };
        let found = match key.kind {
            item::METADATA_ITEM if data.len() >= 24 => {
                Some(Ok(block(u8::try_from(key.offset).map_err(|_| Error::Corrupt)?)))
            }
            item::EXTENT_ITEM if data.len() >= 24 && u64_at(data, 16) & TREE_BLOCK != 0 => {
                Some(Ok(block(*data.get(24 + Key::SIZE).ok_or(Error::Corrupt)?)))
            }
            item::EXTENT_ITEM if data.len() >= 24 => Some(Err(key.offset)),
            item::METADATA_ITEM | item::EXTENT_ITEM => return Err(Error::Corrupt),
            _ => None,
        };
        match found {
            Some(Ok(root)) => {
                tree_block(volume, &root, repair, counts)?;
                read += nodesize;
            }
            Some(Err(len)) => {
                data_extent(volume, csums.as_ref(), (bytenr, len), repair, pages, counts)?;
                read += len;
            }
            None => {}
        }
        *next = bytenr + 1;
        cursor.advance()?;
    }
    Ok(false)
}

/// Reads and checks each copy of the tree block `want` names, and writes
/// a good one over the bad ones if `repair`.
#[inline(never)]
fn tree_block(volume: &Volume, want: &Root, repair: bool, counts: &mut Scrub) -> Result<()> {
    let sb = volume.superblock();
    let nodesize = sb.nodesize as usize;
    let sectors = u64::from(sb.nodesize / sb.sectorsize);
    let place = volume.place(want.bytenr)?;
    if place.len < nodesize as u64 {
        return Err(Error::Corrupt);
    }
    let mut good = None;
    let mut bad = [false; MAX_COPIES];
    for (copy, &site) in place.copies().iter().enumerate() {
        counts.tree_extents += 1;
        counts.tree_bytes += nodesize as u64;
        counts.last_physical = counts.last_physical.max(site.at + nodesize as u64);
        let mut bytes = crate::zeroed(nodesize)?;
        if let Err(err) = read_bytes(volume, site, &mut bytes) {
            if err == Error::NoMemory {
                return Err(err);
            }
            counts.read_errors += sectors;
            bad[copy] = true;
            continue;
        }
        match trees::check_copy(volume, want, bytes) {
            (None, bytes) => {
                good.get_or_insert(bytes);
            }
            (Some(fault), _) => {
                match fault {
                    Fault::Sum => counts.csum_errors += sectors,
                    Fault::Header => counts.verify_errors += sectors,
                }
                bad[copy] = true;
            }
        }
    }
    for (copy, &site) in place.copies().iter().enumerate() {
        if !bad[copy] {
            continue;
        }
        match &good {
            Some(bytes) => {
                if repair {
                    let cache = volume.disk(site.disk)?.cache().ok_or(Error::NoDevice)?;
                    cache.write_out(site.at, bytes)?;
                }
                counts.corrected_errors += sectors;
            }
            None => counts.uncorrectable_errors += sectors,
        }
    }
    Ok(())
}

/// Reads and checks each copy of the data extent of `len` bytes at
/// `bytenr` against the checksum tree `csums`, a run of sectors at a
/// time, and writes a good copy of a sector over its bad ones if
/// `repair`.
#[inline(never)]
fn data_extent(
    volume: &Volume,
    csums: Option<&Root>,
    (bytenr, len): (u64, u64),
    repair: bool,
    pages: &mut KVec<CachePage>,
    counts: &mut Scrub,
) -> Result<()> {
    let mut at = 0;
    while at < len {
        let logical = bytenr + at;
        let place = volume.place(logical)?;
        let n = ((len - at).min(place.len) / PAGE).min(SECTORS as u64) as usize;
        if n == 0 {
            return Err(Error::Corrupt);
        }
        let copies = place.copies();
        while pages.len() < SECTORS * copies.len() {
            mem::push(pages, CachePage::new()?)?;
        }
        // Each copy's sectors that the disk failed to read, a bit each.
        let mut failed = [0u32; MAX_COPIES];
        for (copy, &site) in copies.iter().enumerate() {
            counts.data_extents += u64::from(at == 0);
            counts.data_bytes += n as u64 * PAGE;
            counts.last_physical = counts.last_physical.max(site.at + n as u64 * PAGE);
            let run = &mut pages[copy * SECTORS..copy * SECTORS + n];
            if read_pages(volume, site, run).is_err() {
                // Each sector alone, to find which fail.
                for (sector, page) in run.iter_mut().enumerate() {
                    let spot = Site { at: site.at + sector as u64 * PAGE, ..site };
                    if read_pages(volume, spot, core::slice::from_mut(page)).is_err() {
                        failed[copy] |= 1 << sector;
                    }
                }
            }
        }
        let csum = volume.superblock().csum_type;
        let mut wrote = Ok(());
        let behind = |copy: usize| volume.disks()[copies[copy].disk].behind();
        let mut check = |sector: usize, sum: Option<&[u8]>| {
            let read = |copy: usize| failed[copy] & (1 << sector) == 0;
            let page = |copy: usize| &pages[copy * SECTORS + sector];
            let Some(sum) = sum else {
                for copy in 0..copies.len() {
                    if read(copy) {
                        counts.no_csum += 1;
                    } else {
                        counts.read_errors += 1;
                    }
                }
                // With no checksum to tell, a disk that missed commits
                // takes the copy of one that did not.
                let good = (0..copies.len()).find(|&copy| read(copy) && !behind(copy));
                let Some(good) = good.filter(|_| repair) else { return };
                for copy in (0..copies.len()).filter(|&copy| behind(copy)) {
                    if wrote.is_ok() && page(copy).bytes() != page(good).bytes() {
                        let spot =
                            Site { at: copies[copy].at + sector as u64 * PAGE, ..copies[copy] };
                        wrote = write_page(volume, spot, page(good));
                    }
                }
                return;
            };
            let ok = |copy: usize| read(copy) && csum.matches(page(copy).bytes(), sum);
            let good = (0..copies.len()).find(|&copy| ok(copy));
            for (copy, &site) in copies.iter().enumerate() {
                if ok(copy) {
                    continue;
                }
                if read(copy) {
                    counts.csum_errors += 1;
                } else {
                    counts.read_errors += 1;
                }
                let Some(good) = good else {
                    counts.uncorrectable_errors += 1;
                    continue;
                };
                if repair && wrote.is_ok() {
                    let spot = Site { at: site.at + sector as u64 * PAGE, ..site };
                    wrote = write_page(volume, spot, page(good));
                }
                counts.corrected_errors += 1;
            }
        };
        match csums {
            Some(csums) => extent::each_sum(volume, csums, logical, n, &mut check)?,
            None => (0..n).for_each(|sector| check(sector, None)),
        }
        wrote?;
        at += n as u64 * PAGE;
    }
    Ok(())
}

/// Reads the disk from `site` on into `pages`, around every cache.
fn read_pages(volume: &Volume, site: Site, pages: &mut [CachePage]) -> Result<()> {
    let disk = volume.disk(site.disk)?;
    let device = disk.device().ok_or(Error::NoDevice)?;
    if !disk.holds(site.at, pages.len() as u64 * PAGE) {
        return Err(Error::Corrupt);
    }
    let mut segments = mem::vec_with_capacity(pages.len())?;
    for page in pages.iter_mut() {
        let segment = SegmentMut { page, offset: 0, len: PAGE_SIZE };
        if segments.push_within_capacity(segment).is_err() {
            return Err(Error::NoMemory);
        }
    }
    device.read(site.at / SECTOR, &mut segments)?;
    drop(segments);
    for page in pages.iter_mut() {
        radix_hw::device_wrote(page.bytes_mut());
    }
    Ok(())
}

/// Reads `out.len()` bytes, whole pages, from `site` on, around every
/// cache.
fn read_bytes(volume: &Volume, site: Site, out: &mut [u8]) -> Result<()> {
    let count = out.len().div_ceil(PAGE_SIZE);
    let mut pages = mem::vec_with_capacity(count)?;
    while pages.len() < count {
        mem::push(&mut pages, CachePage::new()?)?;
    }
    read_pages(volume, site, &mut pages)?;
    for (chunk, page) in out.chunks_mut(PAGE_SIZE).zip(&pages) {
        chunk.copy_from_slice(&page.bytes()[..chunk.len()]);
    }
    Ok(())
}

/// Writes `page` at `site`.
fn write_page(volume: &Volume, site: Site, page: &CachePage) -> Result<()> {
    let device = volume.disk(site.disk)?.device().ok_or(Error::NoDevice)?;
    let segment = Segment { page, offset: 0, len: PAGE_SIZE };
    Ok(device.write(site.at / SECTOR, &[segment])?)
}
