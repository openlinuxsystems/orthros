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
    assert_eq!(check(16, 1, 4, 0, [(0, 4096)].into_iter()), Ok(8));
    assert_eq!(check(16, 1, 4, 8, [(0, 4096)].into_iter()), Ok(8));
    assert_eq!(check(16, 1, 4, 9, [(0, 4096)].into_iter()), Err(Error::OutOfRange));
    assert_eq!(check(16, 1, 4, 0, [(512, 3584), (0, 512)].into_iter()), Ok(8));
    assert_eq!(check(16, 1, 4, 0, [(100, 512)].into_iter()), Err(Error::OutOfRange));
    assert_eq!(check(16, 1, 4, 0, [(3584, 1024)].into_iter()), Err(Error::OutOfRange));
    assert_eq!(check(16, 1, 4, 0, [(0, 0)].into_iter()), Err(Error::OutOfRange));
    assert_eq!(check(16, 1, 4, 0, [].into_iter()), Err(Error::OutOfRange));
    assert_eq!(check(16, 1, 1, 0, [(0, 512), (0, 512)].into_iter()), Err(Error::OutOfRange));
    assert_eq!(check(u64::MAX, 1, 4, u64::MAX, [(0, 512)].into_iter()), Err(Error::OutOfRange));
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
    assert_eq!(crate::check_discard(64, 1, 0, 64), Ok(()));
    assert_eq!(crate::check_discard(64, 1, 0, 0), Err(Error::OutOfRange));
    assert_eq!(crate::check_discard(64, 1, 60, 5), Err(Error::OutOfRange));
    assert_eq!(crate::check_discard(64, 1, u64::MAX, 2), Err(Error::OutOfRange));
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
            logical_block: 512,
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
    assert_eq!(crate::check_frames(1 << 20, 1, 0, &many), Err(Error::OutOfRange));
    assert_eq!(crate::check_frames(1 << 20, 1, 0, &many[1..]), Ok(crate::MAX_FRAMES as u64));
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
    let info = disk.info();
    let (scheme, found) = partition::read(info.sectors, info.unit(), &mut |at, out| {
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
    gpt_disk_in(512, sectors, parts, damage)
}

/// A disk of `sectors` sectors in logical blocks of `block` bytes, with a
/// GPT of `parts` (first and last block, name) counted in its blocks.
fn gpt_disk_in(block: u64, sectors: u64, parts: &[(u64, u64, &str)], damage: bool) -> RamDisk {
    let disk = RamDisk::with_blocks(sectors, false, block as u32);
    let blocks = sectors * 512 / block;
    disk.load(0, &mbr(&[(0, 0xee, 1, (blocks - 1) as u32)])).unwrap();
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
    let table = entries.len() as u64 / block;
    let last = blocks - 1;
    for (at, entries_at, alternate) in [(1, 2, last), (last, last - table, 1)] {
        let mut h = [0u8; 512];
        h[0..8].copy_from_slice(b"EFI PART");
        h[8..12].copy_from_slice(&0x0001_0000u32.to_le_bytes());
        h[12..16].copy_from_slice(&92u32.to_le_bytes());
        h[24..32].copy_from_slice(&at.to_le_bytes());
        h[32..40].copy_from_slice(&alternate.to_le_bytes());
        h[40..48].copy_from_slice(&(2 + table).to_le_bytes());
        h[48..56].copy_from_slice(&(last - 1 - table).to_le_bytes());
        h[72..80].copy_from_slice(&entries_at.to_le_bytes());
        h[80..84].copy_from_slice(&128u32.to_le_bytes());
        h[84..88].copy_from_slice(&128u32.to_le_bytes());
        h[88..92].copy_from_slice(&entries_crc.to_le_bytes());
        let crc = partition::header_crc(&h[..92]);
        h[16..20].copy_from_slice(&crc.to_le_bytes());
        disk.load(at * block, &h).unwrap();
        disk.load(entries_at * block, &entries).unwrap();
    }
    if damage {
        disk.load(block + 40, &[0xff]).unwrap();
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

#[test]
fn gpt_partitions_in_4k_blocks() {
    // 8 MiB in 2048 blocks of 4 KiB: the table counts blocks, the
    // partitions come out in sectors.
    for damage in [false, true] {
        let disk =
            gpt_disk_in(4096, 16384, &[(256, 511, "EFI system"), (512, 2000, "data")], damage);
        let (scheme, found) = table(&disk).unwrap();
        assert_eq!(scheme, if damage { Scheme::GptBackup } else { Scheme::Gpt });
        let kept: Vec<_> = found.iter().map(|p| (p.number, p.first, p.sectors)).collect();
        assert_eq!(kept, [(1, 2048, 2048), (2, 4096, 1489 * 8)]);
    }
    // Read as if its blocks were sectors, the table is no GPT's.
    let disk = gpt_disk_in(4096, 16384, &[(256, 511, "")], false);
    let read = partition::read(16384, 1, &mut |at, out| {
        crate::read_sector(&disk, at, out).map_err(|_| partition::Error::Io)
    });
    assert!(matches!(read, Err(partition::Error::Damaged(_))));
}

#[test]
fn mbr_partitions_in_4k_blocks() {
    let disk = RamDisk::with_blocks(8192, false, 4096);
    disk.load(0, &mbr(&[(0, 0x0c, 256, 512), (0, 0x05, 768, 100)])).unwrap();
    disk.load(768 * 4096, &mbr(&[(0, 0x83, 1, 10)])).unwrap();
    let (scheme, found) = table(&disk).unwrap();
    assert_eq!(scheme, Scheme::Mbr);
    let numbers: Vec<_> = found.iter().map(|p| (p.number, p.first, p.sectors)).collect();
    // The extended one shows its first block only.
    assert_eq!(numbers, [(1, 2048, 4096), (2, 6144, 8), (5, 6152, 80)]);
}

#[test]
fn disks_with_4k_sectors_take_whole_blocks() {
    assert_eq!(check(16, 8, 4, 0, [(0, 4096)].into_iter()), Ok(8));
    assert_eq!(check(16, 8, 4, 4, [(0, 4096)].into_iter()), Err(Error::OutOfRange));
    assert_eq!(check(16, 8, 4, 0, [(0, 512)].into_iter()), Err(Error::OutOfRange));
    assert_eq!(check(16, 8, 4, 0, [(512, 3584), (0, 512)].into_iter()), Err(Error::OutOfRange));
    assert_eq!(crate::check_discard(64, 8, 8, 16), Ok(()));
    assert_eq!(crate::check_discard(64, 8, 8, 4), Err(Error::OutOfRange));
    assert_eq!(crate::check_discard(64, 8, 4, 8), Err(Error::OutOfRange));
    let (disk, device) = shared(RamDisk::with_blocks(64, false, 4096));
    assert_eq!((device.info().logical_block, device.info().unit()), (4096, 8));
    disk.load(512 * 10, b"tenth sector").unwrap();
    // A slice is whole blocks; a sector is read in the block that holds it.
    assert!(Slice::new(device.clone(), 4, 16).is_none());
    assert!(Slice::new(device.clone(), 8, 12).is_none());
    let slice = Slice::new(device.clone(), 8, 16).unwrap();
    let mut sector = [0u8; 512];
    crate::read_sector(&slice, 2, &mut sector).unwrap();
    assert_eq!(&sector[..12], b"tenth sector");
    let mut page = CachePage::new().unwrap();
    let mut seg = [SegmentMut { page: &mut page, offset: 0, len: 512 }];
    assert_eq!(slice.read(0, &mut seg), Err(Error::OutOfRange));
    // The cache reads and writes whole blocks: a write of three bytes
    // reads its block first, and syncs it whole.
    let cache = BlockCache::new(device);
    cache.write(5130, b"abc").unwrap();
    let mut out = [0u8; 15];
    cache.read(5120, &mut out).unwrap();
    assert_eq!(&out, b"tenth sectabc\0\0");
    cache.fill(8192, 4096, 1).unwrap();
    let requests = disk.requests();
    cache.sync().unwrap();
    // Two blocks written, side by side in one request, and the flush.
    assert_eq!(disk.requests(), requests + 2);
    let mut raw = [0u8; 15];
    disk.dump(5120, &mut raw);
    assert_eq!(&raw, b"tenth sectabc\0\0");
    // What is discarded goes in whole blocks only.
    cache.write(0, b"zz").unwrap();
    cache.discard(0, 512);
    assert!(cache.is_dirty());
    cache.discard(0, 4096);
    assert!(!cache.is_dirty());
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
fn sync_writes_runs_that_follow_one_another_in_one_request() {
    let (disk, device) = shared(RamDisk::new(1024, false));
    let cache = BlockCache::new(device);
    // 20 pages side by side, ending in half a page, then a gap, then a
    // sector of a page alone, and two runs in one page, with a gap.
    cache.fill(0, 20 * 4096 + 2048, 1).unwrap();
    cache.fill(40 * 4096, 512, 2).unwrap();
    cache.fill(60 * 4096, 1024, 3).unwrap();
    cache.fill(60 * 4096 + 2048, 1024, 4).unwrap();
    let requests = disk.requests();
    cache.sync().unwrap();
    // 21 runs side by side: 16 in one request, 5 in the next; then the
    // lone sector, and the two runs of one page; and the flush.
    assert_eq!(disk.requests(), requests + 2 + 1 + 2 + 1);
    assert!(!cache.is_dirty());
    let mut raw = [0u8; 4096];
    disk.dump(19 * 4096, &mut raw);
    assert!(raw.iter().all(|&b| b == 1));
    disk.dump(20 * 4096, &mut raw);
    assert!(raw[..2048].iter().all(|&b| b == 1) && raw[2048..].iter().all(|&b| b == 0));
    disk.dump(60 * 4096, &mut raw);
    assert!(raw[..1024].iter().all(|&b| b == 3) && raw[1024..2048].iter().all(|&b| b == 0));
    assert!(raw[2048..3072].iter().all(|&b| b == 4));
    // A failed request leaves its runs dirty, and the next sync writes
    // them.
    cache.fill(4096, 8192, 5).unwrap();
    disk.fail_after(0);
    assert_eq!(cache.sync(), Err(Error::Io));
    assert!(cache.is_dirty());
    disk.fail_after(u32::MAX);
    cache.sync().unwrap();
    assert!(!cache.is_dirty());
    disk.dump(8192, &mut raw);
    assert!(raw.iter().all(|&b| b == 5));
    // A page written out alone, then changed again, is written once.
    cache.fill(4096, 512, 6).unwrap();
    cache.write_out(4096, &[7u8; 512]).unwrap();
    cache.fill(4096, 512, 8).unwrap();
    let requests = disk.requests();
    cache.sync().unwrap();
    assert_eq!(disk.requests(), requests + 2);
    disk.dump(4096, &mut raw);
    assert!(raw[..512].iter().all(|&b| b == 8));
}

/// A disk that notes how many writes each [`BlockDevice::write_all`] it is
/// asked for has, and which are to be durable.
struct Batches {
    disk: Arc<RamDisk>,
    calls: std::sync::Mutex<Vec<Vec<bool>>>,
}

impl BlockDevice for Batches {
    fn info(&self) -> crate::Info {
        self.disk.info()
    }

    fn read(&self, sector: u64, segments: &mut [SegmentMut<'_>]) -> Result<(), Error> {
        self.disk.read(sector, segments)
    }

    fn write(&self, sector: u64, segments: &[crate::Segment<'_>]) -> Result<(), Error> {
        self.disk.write(sector, segments)
    }

    fn flush(&self) -> Result<(), Error> {
        self.disk.flush()
    }

    fn write_all(&self, writes: &[crate::Write<'_>]) -> Result<(), Error> {
        self.calls.lock().unwrap().push(writes.iter().map(|w| w.through).collect());
        self.disk.write_all(writes)
    }
}

#[test]
fn a_sync_and_writes_now_go_to_the_disk_at_once() {
    let disk = Arc::new(RamDisk::new(1024, false));
    let batches = Arc::new(Batches { disk: disk.clone(), calls: std::sync::Mutex::default() });
    let device: DeviceRef = batches.clone();
    let cache = BlockCache::new(device);
    // Three runs apart, and two side by side: four requests, given at once.
    cache.fill(0, 512, 1).unwrap();
    cache.fill(40 * 4096, 4096, 2).unwrap();
    cache.fill(41 * 4096, 4096, 3).unwrap();
    cache.fill(80 * 4096, 1024, 4).unwrap();
    cache.fill(90 * 4096, 512, 5).unwrap();
    cache.sync().unwrap();
    assert_eq!(batches.calls.lock().unwrap().as_slice(), [vec![false; 4]]);
    // A superblock and its copies: the first durable, all at once.
    let flushes = disk.flushes();
    cache.write(8 * 4096, &[6u8; 512]).unwrap();
    let copy = [7u8; 4096];
    let pieces = [(16 * 4096, &copy[..], true), (50 * 4096, &copy[..], false)];
    cache.write_now_all(&pieces).unwrap();
    assert_eq!(batches.calls.lock().unwrap()[1], [true, false]);
    assert_eq!(disk.flushes(), flushes + 1);
    let mut raw = [0u8; 4096];
    disk.dump(50 * 4096, &mut raw);
    assert_eq!(raw, copy);
    // What else was dirty stays dirty.
    assert!(cache.is_dirty());
}

#[test]
fn writes_through_and_out_reach_the_disk_alone() {
    let (disk, device) = shared(RamDisk::new(20, false));
    let cache = BlockCache::new(device);
    cache.write(0, &[1u8; 512]).unwrap();
    let flushes = disk.flushes();
    // A write through reaches the disk at once (a RAM disk has no forced
    // unit access, so it writes and flushes); the sector written before
    // stays in the cache, dirty.
    cache.write_through(4096, &[2u8; 1024]).unwrap();
    cache.write_out(8192, &[3u8; 512]).unwrap();
    let mut raw = [0u8; 512];
    disk.dump(4096 + 512, &mut raw);
    assert_eq!(raw, [2u8; 512]);
    disk.dump(8192, &mut raw);
    assert_eq!(raw, [3u8; 512]);
    disk.dump(0, &mut raw);
    assert_eq!(raw, [0u8; 512]);
    assert_eq!(disk.flushes(), flushes + 1);
    assert!(cache.is_dirty());
    // The cache keeps what it wrote.
    let mut out = [0u8; 512];
    cache.read(4096, &mut out).unwrap();
    assert_eq!(out, [2u8; 512]);
    cache.sync().unwrap();
    disk.dump(0, &mut raw);
    assert_eq!(raw, [1u8; 512]);
    assert!(!cache.is_dirty());
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

/// A swap area's first page as util-linux's `mkswap` writes it for a
/// device of `pages` pages, with `bad` listed and the label `label`.
fn swap_page(pages: u32, bad: &[u32], label: &[u8]) -> std::vec::Vec<u8> {
    let mut page = std::vec![0u8; 4096];
    let mut put = |at: usize, word: u32| page[at..at + 4].copy_from_slice(&word.to_le_bytes());
    put(1024, 1);
    put(1028, pages - 1);
    put(1032, bad.len() as u32);
    for (i, &number) in bad.iter().enumerate() {
        put(1536 + 4 * i, number);
    }
    page[1036..1052].copy_from_slice(&[0x5a; 16]);
    page[1052..1052 + label.len()].copy_from_slice(label);
    page[4086..].copy_from_slice(b"SWAPSPACE2");
    page
}

#[test]
fn swap_headers_read_as_linux_reads_them() {
    use crate::swap::{Header, HeaderError, MAX_BAD};
    let page = swap_page(16384, &[7, 9, 20000, 0], b"SWAPTEST");
    let header = Header::read(&page, 16384).unwrap();
    assert_eq!(header.last_page, 16383);
    assert_eq!(header.bad_pages, 4);
    assert_eq!(header.uuid, [0x5a; 16]);
    assert_eq!(header.label(), Some("SWAPTEST"));
    // A bad page past the end, or the header's own, is no page to skip.
    assert_eq!(header.bad(&page).collect::<std::vec::Vec<_>>(), [7, 9]);
    // A device smaller than the header says ends the area.
    assert_eq!(Header::read(&page, 100).unwrap().last_page, 99);
    assert_eq!(Header::read(&page, 1), Err(HeaderError::Corrupt));
    let mut other = page.clone();
    other[4086..].copy_from_slice(b"SWAP-SPACE");
    assert_eq!(Header::read(&other, 16384), Err(HeaderError::NotSwap));
    let mut other = page.clone();
    other[1024..1028].copy_from_slice(&1u32.to_be_bytes());
    assert_eq!(Header::read(&other, 16384), Err(HeaderError::Version));
    let mut other = page.clone();
    other[1032..1036].copy_from_slice(&(MAX_BAD + 1).to_le_bytes());
    assert_eq!(Header::read(&other, 16384), Err(HeaderError::Corrupt));
    assert_eq!(MAX_BAD, 637, "Linux's MAX_SWAP_BADPAGES for 4 KiB pages");
    assert_eq!(Header::read(&page[..4000], 16384), Err(HeaderError::NotSwap));
}

/// What util-linux's own `mkswap` writes, where the host has it.
#[test]
fn swap_headers_that_mkswap_makes_read() {
    use crate::swap::Header;
    let Some(tool) = ["/usr/sbin/mkswap", "/sbin/mkswap"]
        .into_iter()
        .find(|tool| std::path::Path::new(tool).exists())
    else {
        return;
    };
    let path = std::env::temp_dir().join(std::format!("radix-swap-{}.img", std::process::id()));
    std::fs::File::create(&path).unwrap().set_len(8 << 20).unwrap();
    let made = std::process::Command::new(tool)
        .args(["-L", "RADIXSWAP", "-U", "5a5a5a5a-5a5a-5a5a-5a5a-5a5a5a5a5a5a"])
        .arg(&path)
        .stdin(std::process::Stdio::null())
        .output()
        .unwrap();
    let image = std::fs::read(&path).unwrap();
    std::fs::remove_file(&path).unwrap();
    assert!(made.status.success(), "{}", std::string::String::from_utf8_lossy(&made.stderr));
    let header = Header::read(&image[..4096], 2048).unwrap();
    assert_eq!((header.last_page, header.bad_pages), (2047, 0));
    assert_eq!(header.uuid, [0x5a; 16]);
    assert_eq!(header.label(), Some("RADIXSWAP"));
}
