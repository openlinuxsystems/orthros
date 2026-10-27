// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! The block layer on the host: the page index against a model, partition
//! tables built by hand and fuzzed, and the block cache against a disk in
//! memory.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::vec::Vec;

use proptest::prelude::*;

use crate::cache::BlockCache;
use crate::crc32::crc32;
use crate::mem::CachePage;
use crate::pagemap::PageMap;
use crate::partition::{self, Kind, Scheme};
use crate::ram::RamDisk;
use crate::{BlockDevice, DeviceRef, Error, SECTOR, SegmentMut, Slice, check};

fn shared(disk: RamDisk) -> (Arc<RamDisk>, DeviceRef) {
    let disk = Arc::new(disk);
    let device: DeviceRef = disk.clone();
    (disk, device)
}

#[test]
fn page_map_basics() {
    let mut map = PageMap::new();
    assert!(map.is_empty());
    assert_eq!(map.insert(5, 'a').unwrap(), None);
    assert_eq!(map.insert(5, 'b').unwrap(), Some('a'));
    assert_eq!(map.insert(1 << 40, 'c').unwrap(), None);
    assert_eq!(map.insert(u64::MAX, 'd').unwrap(), None);
    assert_eq!(map.len(), 3);
    assert_eq!(map.get(5), Some(&'b'));
    assert_eq!(map.get(6), None);
    assert_eq!(map.next(6), Some((1 << 40, &'c')));
    assert_eq!(map.next((1 << 40) + 1), Some((u64::MAX, &'d')));
    assert_eq!(map.remove(1 << 40), Some('c'));
    assert_eq!(map.next(6), Some((u64::MAX, &'d')));
    map.retain(|index, _| index != 5, drop);
    assert_eq!(map.len(), 1);
    assert_eq!(map.remove(u64::MAX), Some('d'));
    assert!(map.is_empty());
    assert_eq!(map.next(0), None);
}

#[derive(Clone, Debug)]
enum MapOp {
    Insert(u64, u32),
    Remove(u64),
    Next(u64),
    RetainOdd,
}

fn index() -> impl Strategy<Value = u64> {
    prop_oneof![0..200u64, 0..100_000u64, any::<u64>()]
}

fn map_op() -> impl Strategy<Value = MapOp> {
    prop_oneof![
        4 => (index(), any::<u32>()).prop_map(|(i, v)| MapOp::Insert(i, v)),
        2 => index().prop_map(MapOp::Remove),
        2 => index().prop_map(MapOp::Next),
        1 => Just(MapOp::RetainOdd),
    ]
}

proptest! {
    #[test]
    fn page_map_matches_a_model(ops in proptest::collection::vec(map_op(), 1..200)) {
        let mut map = PageMap::new();
        let mut model = BTreeMap::new();
        for op in ops {
            match op {
                MapOp::Insert(i, v) => {
                    prop_assert_eq!(map.insert(i, v).unwrap(), model.insert(i, v));
                }
                MapOp::Remove(i) => prop_assert_eq!(map.remove(i), model.remove(&i)),
                MapOp::Next(i) => {
                    let expected = model.range(i..).next().map(|(&k, v)| (k, v));
                    prop_assert_eq!(map.next(i), expected);
                }
                MapOp::RetainOdd => {
                    let mut taken = 0;
                    map.retain(|_, v| *v % 2 == 1, |_| taken += 1);
                    let before = model.len();
                    model.retain(|_, v| *v % 2 == 1);
                    prop_assert_eq!(taken, before - model.len());
                }
            }
            prop_assert_eq!(map.len(), model.len());
        }
        let mut seen = Vec::new();
        map.for_each_from(0, |i, v| { seen.push((i, *v)); true });
        let expected: Vec<_> = model.iter().map(|(&k, &v)| (k, v)).collect();
        prop_assert_eq!(seen, expected);
    }
}

#[test]
fn requests_are_checked() {
    assert_eq!(check(16, 4, 0, [(0, 4096)].into_iter()), Ok(8));
    assert_eq!(check(16, 4, 8, [(0, 4096)].into_iter()), Ok(8));
    assert_eq!(check(16, 4, 9, [(0, 4096)].into_iter()), Err(Error::OutOfRange));
    assert_eq!(check(16, 4, 0, [(512, 3584), (0, 512)].into_iter()), Ok(8));
    assert_eq!(check(16, 4, 0, [(100, 512)].into_iter()), Err(Error::OutOfRange));
    assert_eq!(check(16, 4, 0, [(3584, 1024)].into_iter()), Err(Error::OutOfRange));
    assert_eq!(check(16, 4, 0, [(0, 0)].into_iter()), Err(Error::OutOfRange));
    assert_eq!(check(16, 4, 0, [].into_iter()), Err(Error::OutOfRange));
    assert_eq!(check(16, 1, 0, [(0, 512), (0, 512)].into_iter()), Err(Error::OutOfRange));
    assert_eq!(check(u64::MAX, 4, u64::MAX, [(0, 512)].into_iter()), Err(Error::OutOfRange));
}

#[test]
fn ram_disk_and_slices() {
    let (disk, device) = shared(RamDisk::new(64, false));
    disk.load(512 * 10, b"tenth sector").unwrap();
    let slice = Slice::new(device.clone(), 8, 16).unwrap();
    assert!(Slice::new(device.clone(), 60, 5).is_none());
    assert!(Slice::new(device.clone(), 0, 0).is_none());
    assert_eq!(slice.info().sectors, 16);
    let mut sector = [0u8; 512];
    crate::read_sector(&slice, 2, &mut sector).unwrap();
    assert_eq!(&sector[..12], b"tenth sector");
    let mut page = CachePage::new().unwrap();
    let mut seg = [SegmentMut { page: &mut page, offset: 0, len: 512 }];
    assert_eq!(slice.read(16, &mut seg), Err(Error::OutOfRange));
    let ro = RamDisk::new(8, true);
    let page = CachePage::new().unwrap();
    let seg = [crate::Segment { page: &page, offset: 0, len: 512 }];
    assert_eq!(ro.write(0, &seg), Err(Error::ReadOnly));
}

#[test]
fn discards_and_writes_through() {
    let (disk, device) = shared(RamDisk::new(64, false));
    disk.load(0, &std::vec![0xaa; 64 * 512]).unwrap();
    assert_eq!(crate::check_discard(64, 0, 64), Ok(()));
    assert_eq!(crate::check_discard(64, 0, 0), Err(Error::OutOfRange));
    assert_eq!(crate::check_discard(64, 60, 5), Err(Error::OutOfRange));
    assert_eq!(crate::check_discard(64, u64::MAX, 2), Err(Error::OutOfRange));
    // Through a partition: sectors 8 to 23 of the disk. Its sectors 3 to
    // 12 (disk 11 to 20) go: part of the disk's second page, the third
    // whole, and part of the fourth.
    let slice = Slice::new(device.clone(), 8, 16).unwrap();
    assert_eq!(slice.discard(3, 14), Err(Error::OutOfRange));
    slice.discard(3, 10).unwrap();
    assert_eq!(disk.discarded(), 10);
    let mut raw = std::vec![0u8; 64 * 512];
    disk.dump(0, &mut raw);
    for (sector, bytes) in raw.chunks(512).enumerate() {
        let gone = (11..21).contains(&sector);
        assert!(bytes.iter().all(|&b| b == if gone { 0 } else { 0xaa }), "sector {sector}");
    }
    let flushes = disk.flushes();
    let mut page = CachePage::new().unwrap();
    page.bytes_mut()[..512].fill(0x5a);
    slice.write_through(0, &[crate::Segment { page: &page, offset: 0, len: 512 }]).unwrap();
    assert_eq!(disk.flushes(), flushes + 1);
    disk.dump(8 * 512, &mut raw[..512]);
    assert!(raw[..512].iter().all(|&b| b == 0x5a));
    let ro = RamDisk::new(8, true);
    assert_eq!(ro.discard(0, 8), Err(Error::ReadOnly));
}

/// A disk of 64 sectors that only notes the lent memory it is asked to
/// move.
#[derive(Default)]
struct Lending {
    asked: std::sync::Mutex<Vec<Asked>>,
}

/// Whether [`Lending`] was asked to write, the sector, the frames.
type Asked = (bool, u64, Vec<(u64, u32)>);

impl BlockDevice for Lending {
    fn info(&self) -> crate::Info {
        crate::Info {
            sectors: 64,
            block_size: 512,
            read_only: false,
            max_segments: 16,
            discard_sectors: 0,
            rotational: false,
        }
    }

    fn read(&self, _: u64, _: &mut [SegmentMut<'_>]) -> Result<(), Error> {
        Err(Error::Io)
    }

    fn write(&self, _: u64, _: &[crate::Segment<'_>]) -> Result<(), Error> {
        Err(Error::Io)
    }

    fn flush(&self) -> Result<(), Error> {
        Ok(())
    }

    fn read_frames(&self, sector: u64, frames: &[(u64, u32)]) -> Result<(), Error> {
        self.asked.lock().unwrap().push((false, sector, frames.to_vec()));
        Ok(())
    }

    fn write_frames(&self, sector: u64, frames: &[(u64, u32)]) -> Result<(), Error> {
        self.asked.lock().unwrap().push((true, sector, frames.to_vec()));
        Ok(())
    }
}

#[test]
fn lent_memory_goes_through_partitions() {
    let disk = Arc::new(Lending::default());
    let device: DeviceRef = disk.clone();
    // Sectors 8 to 23 of the disk.
    let slice = Slice::new(device, 8, 16).unwrap();
    let frames = [(0x1_0e00, 512), (0x2_0000, 4096)];
    slice.read_frames(2, &frames).unwrap();
    slice.write_frames(7, &frames[1..]).unwrap();
    assert_eq!(
        *disk.asked.lock().unwrap(),
        [(false, 10, frames.to_vec()), (true, 15, frames[1..].to_vec())]
    );
    // Past the partition's end, parts of sectors, a frame across a page's
    // end, none, or more than a mebibyte's worth: refused before the disk
    // sees them.
    assert_eq!(slice.read_frames(8, &[(0x2_0000, 4096), (0x3_0000, 512)]), Err(Error::OutOfRange));
    assert_eq!(slice.read_frames(0, &[(0x2_0000, 100)]), Err(Error::OutOfRange));
    assert_eq!(slice.read_frames(0, &[(0x2_0100, 512)]), Err(Error::OutOfRange));
    assert_eq!(slice.write_frames(0, &[(0x2_0e00, 1024)]), Err(Error::OutOfRange));
    assert_eq!(slice.write_frames(0, &[]), Err(Error::OutOfRange));
    let many = std::vec![(0x2_0000, 512); crate::MAX_FRAMES + 1];
    assert_eq!(crate::check_frames(1 << 20, 0, &many), Err(Error::OutOfRange));
    assert_eq!(crate::check_frames(1 << 20, 0, &many[1..]), Ok(crate::MAX_FRAMES as u64));
    assert_eq!(disk.asked.lock().unwrap().len(), 2);
    // A device that cannot reach lent memory says so, and the caller
    // copies.
    let (_, ram) = shared(RamDisk::new(8, false));
    assert_eq!(ram.read_frames(0, &frames[..1]), Err(Error::Unsupported));
}

/// An MBR with `entries` (status, type, first, sectors).
fn mbr(entries: &[(u8, u8, u32, u32)]) -> [u8; 512] {
    let mut s = [0u8; 512];
    for (i, &(status, kind, first, sectors)) in entries.iter().enumerate() {
        let at = 446 + 16 * i;
        s[at] = status;
        s[at + 4] = kind;
        s[at + 8..at + 12].copy_from_slice(&first.to_le_bytes());
        s[at + 12..at + 16].copy_from_slice(&sectors.to_le_bytes());
    }
    s[510] = 0x55;
    s[511] = 0xaa;
    s
}

fn table(disk: &RamDisk) -> Result<(Scheme, Vec<partition::Partition>), partition::Error> {
    let sectors = disk.info().sectors;
    let (scheme, found) = partition::read(sectors, &mut |at, out| {
        crate::read_sector(disk, at, out).map_err(|_| partition::Error::Io)
    })?;
    Ok((scheme, found.clone()))
}

#[test]
fn mbr_primary_and_logical_partitions() {
    let disk = RamDisk::new(4096, false);
    disk.load(0, &mbr(&[(0x80, 0x0c, 2048, 1024), (0, 0x05, 3072, 1000)])).unwrap();
    // Two logical partitions in the extended one.
    disk.load(3072 * 512, &mbr(&[(0, 0x83, 63, 100), (0, 0x05, 200, 300)])).unwrap();
    disk.load((3072 + 200) * 512, &mbr(&[(0, 0x83, 63, 200)])).unwrap();
    let (scheme, found) = table(&disk).unwrap();
    assert_eq!(scheme, Scheme::Mbr);
    let numbers: Vec<_> = found.iter().map(|p| (p.number, p.first, p.sectors)).collect();
    // The extended one shows its first two sectors only.
    assert_eq!(numbers, [(1, 2048, 1024), (2, 3072, 2), (5, 3135, 100), (6, 3335, 200)]);
    assert_eq!(found[0].kind, Kind::Mbr(0x0c));
    assert!(found.iter().all(|p| !p.truncated));
}

#[test]
fn a_broken_chain_of_logical_partitions_keeps_what_came_before() {
    let disk = RamDisk::new(4096, false);
    disk.load(0, &mbr(&[(0x80, 0x0c, 2048, 1024), (0, 0x05, 3072, 1000)])).unwrap();
    // The second record has no signature.
    disk.load(3072 * 512, &mbr(&[(0, 0x83, 63, 100), (0, 0x05, 200, 300)])).unwrap();
    let (_, found) = table(&disk).unwrap();
    let numbers: Vec<_> = found.iter().map(|p| p.number).collect();
    assert_eq!(numbers, [1, 2, 5]);
    // A link backwards ends it after the record that holds it.
    disk.load((3072 + 200) * 512, &mbr(&[(0, 0x83, 63, 50), (0, 0x05, 100, 50)])).unwrap();
    let numbers: Vec<_> = table(&disk).unwrap().1.iter().map(|p| p.number).collect();
    assert_eq!(numbers, [1, 2, 5, 6]);
}

#[test]
fn whole_disk_filesystems_have_no_table() {
    let disk = RamDisk::new(4096, false);
    assert_eq!(table(&disk).unwrap().0, Scheme::None);
    // A FAT boot sector: boot code where the entries would be.
    let mut boot = [0u8; 512];
    boot[0..3].copy_from_slice(&[0xeb, 0x3c, 0x90]);
    boot[446..510].fill(0x4f);
    boot[510] = 0x55;
    boot[511] = 0xaa;
    disk.load(0, &boot).unwrap();
    assert_eq!(table(&disk).unwrap().0, Scheme::None);
}

#[test]
fn mbr_partitions_past_the_end_are_cut_or_skipped() {
    // As on Linux: one that reaches past the end is cut to fit, one that
    // starts past it, or has no size, is skipped, and the others stay.
    let disk = RamDisk::new(4096, false);
    let entries =
        [(0, 0x83, 2048, 4096), (0, 0x83, 5000, 10), (0, 0x0c, 64, 0), (0, 0x0c, 64, 1984)];
    disk.load(0, &mbr(&entries)).unwrap();
    let (scheme, found) = table(&disk).unwrap();
    assert_eq!(scheme, Scheme::Mbr);
    let kept: Vec<_> = found.iter().map(|p| (p.number, p.first, p.sectors, p.truncated)).collect();
    assert_eq!(kept, [(1, 2048, 2048, true), (4, 64, 1984, false)]);
}

/// A GPT disk of `sectors` with `parts` (first, last, name), both headers
/// written; `damage` breaks the primary header.
fn gpt_disk(sectors: u64, parts: &[(u64, u64, &str)], damage: bool) -> RamDisk {
    let disk = RamDisk::new(sectors, false);
    disk.load(0, &mbr(&[(0, 0xee, 1, (sectors - 1) as u32)])).unwrap();
    let mut entries = std::vec![0u8; 128 * 128];
    for (i, &(first, last, name)) in parts.iter().enumerate() {
        let e = &mut entries[i * 128..(i + 1) * 128];
        // The basic data partition type GUID.
        e[0..16].copy_from_slice(&[
            0xa2, 0xa0, 0xd0, 0xeb, 0xe5, 0xb9, 0x33, 0x44, 0x87, 0xc0, 0x68, 0xb6, 0xb7, 0x26,
            0x99, 0xc7,
        ]);
        e[16] = i as u8 + 1;
        e[32..40].copy_from_slice(&first.to_le_bytes());
        e[40..48].copy_from_slice(&last.to_le_bytes());
        for (j, c) in name.encode_utf16().enumerate() {
            e[56 + 2 * j..58 + 2 * j].copy_from_slice(&c.to_le_bytes());
        }
    }
    let entries_crc = crc32(&entries);
    for (at, entries_at, alternate) in [(1, 2, sectors - 1), (sectors - 1, sectors - 33, 1)] {
        let mut h = [0u8; 512];
        h[0..8].copy_from_slice(b"EFI PART");
        h[8..12].copy_from_slice(&0x0001_0000u32.to_le_bytes());
        h[12..16].copy_from_slice(&92u32.to_le_bytes());
        h[24..32].copy_from_slice(&at.to_le_bytes());
        h[32..40].copy_from_slice(&alternate.to_le_bytes());
        h[40..48].copy_from_slice(&34u64.to_le_bytes());
        h[48..56].copy_from_slice(&(sectors - 34).to_le_bytes());
        h[72..80].copy_from_slice(&entries_at.to_le_bytes());
        h[80..84].copy_from_slice(&128u32.to_le_bytes());
        h[84..88].copy_from_slice(&128u32.to_le_bytes());
        h[88..92].copy_from_slice(&entries_crc.to_le_bytes());
        let crc = partition::header_crc(&h[..92]);
        h[16..20].copy_from_slice(&crc.to_le_bytes());
        disk.load(at * 512, &h).unwrap();
        disk.load(entries_at * 512, &entries).unwrap();
    }
    if damage {
        disk.load(512 + 40, &[0xff]).unwrap();
    }
    disk
}

#[test]
fn gpt_partitions_and_the_backup_header() {
    for damage in [false, true] {
        let disk = gpt_disk(8192, &[(2048, 4095, "EFI system"), (4096, 8000, "data")], damage);
        let (scheme, found) = table(&disk).unwrap();
        assert_eq!(scheme, if damage { Scheme::GptBackup } else { Scheme::Gpt });
        assert_eq!(found.len(), 2);
        assert_eq!((found[0].number, found[0].first, found[0].sectors), (1, 2048, 2048));
        assert_eq!(found[1].name().collect::<std::string::String>(), "data");
    }
    // An entry past the disk's end, or ending before it starts, is
    // skipped; one outside the usable sectors but on the disk is shown,
    // as on Linux.
    let parts =
        [(10, 4095, "low"), (4096, 9000, "past"), (6000, 5000, "backwards"), (8100, 8150, "")];
    let disk = gpt_disk(8192, &parts, false);
    let (_, found) = table(&disk).unwrap();
    let kept: Vec<_> = found.iter().map(|p| (p.number, p.first, p.sectors)).collect();
    assert_eq!(kept, [(1, 10, 4086), (4, 8100, 51)]);
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(512))]

    /// Garbage tables never panic, and every partition found is on the
    /// disk.
    #[test]
    fn garbage_tables_are_safe(
        sectors in 2u64..256,
        first in proptest::collection::vec(any::<u8>(), 512),
        rest in proptest::collection::vec((0u64..256, proptest::collection::vec(any::<u8>(), 512)), 0..8),
        signature in any::<bool>(),
    ) {
        let disk = RamDisk::new(sectors, false);
        let mut first = first;
        if signature {
            first[510] = 0x55;
            first[511] = 0xaa;
        }
        disk.load(0, &first).unwrap();
        for (at, bytes) in rest {
            if at > 0 && at < sectors {
                disk.load(at * 512, &bytes).unwrap();
            }
        }
        if let Ok((_, found)) = table(&disk) {
            for p in found {
                prop_assert!(partition::within(p.first, p.sectors, sectors));
            }
        }
    }
}

#[test]
fn block_cache_reads_writes_and_syncs() {
    let (disk, device) = shared(RamDisk::new(20, false));
    disk.load(0, &[7u8; 20 * 512]).unwrap();
    let cache = BlockCache::new(device);
    let mut out = [0u8; 700];
    cache.read(4000, &mut out).unwrap();
    assert!(out.iter().all(|&b| b == 7));
    // A partial-sector write reads the sector first; the device sees it
    // only after sync.
    cache.write(4090, b"across pages").unwrap();
    let mut raw = [0u8; 12];
    disk.dump(4090, &mut raw);
    assert_eq!(raw, [7u8; 12]);
    assert!(cache.is_dirty());
    let flushes = disk.flushes();
    cache.sync().unwrap();
    assert!(!cache.is_dirty());
    assert_eq!(disk.flushes(), flushes + 1);
    disk.dump(4090, &mut raw);
    assert_eq!(&raw, b"across pages");
    // Whole sectors are written without being read.
    let requests = disk.requests();
    cache.fill(8192, 1024, 1).unwrap();
    assert_eq!(disk.requests(), requests);
    // The last page is partial: 20 sectors are 2.5 pages.
    let mut tail = [0u8; 512];
    cache.read(19 * 512, &mut tail).unwrap();
    assert_eq!(cache.read(20 * 512, &mut tail), Err(Error::OutOfRange));
    assert_eq!(cache.write(20 * 512 - 1, b"xy"), Err(Error::OutOfRange));
}

#[test]
fn discarded_sectors_never_reach_the_disk() {
    let (disk, device) = shared(RamDisk::new(16, false));
    let cache = BlockCache::new(device);
    cache.fill(0, 4096, 0xaa).unwrap();
    // Forget sectors 2 and 3 only; the others in the page stay dirty.
    cache.discard(1024, 1024);
    disk.load(1024, &[0x55; 1024]).unwrap();
    cache.sync().unwrap();
    let mut raw = [0u8; 4096];
    disk.dump(0, &mut raw);
    assert!(raw[..1024].iter().all(|&b| b == 0xaa));
    assert!(raw[1024..2048].iter().all(|&b| b == 0x55));
    assert!(raw[2048..].iter().all(|&b| b == 0xaa));
    // Read again, the discarded sectors come from the disk.
    let mut back = [0u8; 4];
    cache.read(1024, &mut back).unwrap();
    assert_eq!(back, [0x55; 4]);
}

#[test]
fn failed_writes_stay_dirty_and_shrink_keeps_them() {
    let (disk, device) = shared(RamDisk::new(64, false));
    let cache = BlockCache::new(device);
    cache.write(0, b"keep me").unwrap();
    let mut out = [0u8; 4];
    cache.read(16384, &mut out).unwrap();
    assert_eq!(cache.pages(), 2);
    disk.fail_after(0);
    assert_eq!(cache.sync(), Err(Error::Io));
    assert!(cache.is_dirty());
    assert_eq!(cache.shrink(10), 1, "only the clean page goes");
    disk.fail_after(u32::MAX);
    cache.sync().unwrap();
    let mut raw = [0u8; 7];
    disk.dump(0, &mut raw);
    assert_eq!(&raw, b"keep me");
    assert_eq!(cache.shrink(10), 1);
    assert_eq!(cache.pages(), 0);
}

#[derive(Clone, Debug)]
enum CacheOp {
    Write(u64, Vec<u8>),
    Read(u64, usize),
    Discard(u64, u64),
    Sync,
    Shrink,
}

proptest! {
    /// The cache against a model disk: after a sync, the disk holds what
    /// was written, and reads always see the latest writes.
    #[test]
    fn block_cache_matches_a_model(ops in proptest::collection::vec(prop_oneof![
        4 => (0u64..12_000, proptest::collection::vec(any::<u8>(), 1..3000))
            .prop_map(|(o, d)| CacheOp::Write(o, d)),
        3 => (0u64..12_000, 1usize..5000).prop_map(|(o, l)| CacheOp::Read(o, l)),
        1 => (0u64..24, 1u64..8).prop_map(|(s, n)| CacheOp::Discard(s, n)),
        1 => Just(CacheOp::Sync),
        1 => Just(CacheOp::Shrink),
    ], 1..60)) {
        let size = 24 * SECTOR as usize;
        let (disk, device) = shared(RamDisk::new(24, false));
        let cache = BlockCache::new(device);
        let mut model = std::vec![0u8; size];
        for op in ops {
            match op {
                CacheOp::Write(offset, data) => {
                    let fits = offset as usize + data.len() <= size;
                    prop_assert_eq!(cache.write(offset, &data).is_ok(), fits);
                    if fits {
                        model[offset as usize..offset as usize + data.len()].copy_from_slice(&data);
                    }
                }
                CacheOp::Read(offset, len) => {
                    let mut out = std::vec![0u8; len];
                    let fits = offset as usize + len <= size;
                    prop_assert_eq!(cache.read(offset, &mut out).is_ok(), fits);
                    if fits {
                        prop_assert_eq!(&out[..], &model[offset as usize..offset as usize + len]);
                    }
                }
                CacheOp::Discard(sector, count) => {
                    let count = count.min(24 - sector);
                    cache.discard(sector * SECTOR, (count * SECTOR) as usize);
                    // The model forgets too: those sectors are what the
                    // disk has.
                    let (a, b) = ((sector * SECTOR) as usize, ((sector + count) * SECTOR) as usize);
                    disk.dump(a as u64, &mut model[a..b]);
                }
                CacheOp::Sync => {
                    cache.sync().unwrap();
                    let mut raw = std::vec![0u8; size];
                    disk.dump(0, &mut raw);
                    prop_assert_eq!(&raw, &model);
                }
                CacheOp::Shrink => {
                    cache.shrink(usize::MAX);
                }
            }
        }
    }
}
