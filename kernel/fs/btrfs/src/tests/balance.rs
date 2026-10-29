// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Host tests of balancing and of adding and removing disks (ADR 0159):
//! every chunk moved, with snapshots, clones and compressed extents
//! among what moves; a filesystem of one disk given a second and
//! converted to RAID1; a disk's chunks moved off it and the disk taken
//! out; each read back after a sync, and checked by Linux's `btrfs
//! check`, which follows every reference of every block and extent.

use std::format;
use std::vec::Vec;

use radix_vfs::{BALANCE_SINGLE, Balance, BalanceArgs, Error, FileSystem, Kind};

use super::compressed::compressible_tree;
use super::files::noise;
use super::raid::{assert_checks, disks_of, images_of, mkfs_disks, mount_all, write_file};
use super::rw::{Model, mkfs_with, mount_rw, put, snapshot as model_of, synced};
use super::write::{image_of, writable};
use crate::BtrfsFs;
use crate::chunk::kind;

/// Every kind of chunk, each moved and converted to `convert`.
fn every(convert: u64) -> Balance {
    let args = BalanceArgs { on: true, convert, soft: false };
    Balance { data: args, metadata: args, system: args }
}

/// The start and flags of each chunk of `fs`.
fn chunks(fs: &BtrfsFs) -> Vec<(u64, u64)> {
    fs.volume().with_chunks(|map| map.chunks().iter().map(|c| (c.start, c.kind)).collect())
}

/// Checks a balance of every chunk moved each of them to a new one, and
/// left what `fs` reads as it was.
fn balanced_all(fs: &BtrfsFs, plan: &Balance, what: &str) {
    let before = model_of(&fs.root());
    let old = chunks(fs);
    let done = fs.balance(plan).unwrap_or_else(|e| panic!("{what}: {e:?}"));
    assert_eq!((done.considered, done.moved), (old.len() as u64, old.len() as u64), "{what}");
    let last = old.iter().map(|c| c.0).max().unwrap();
    assert!(chunks(fs).iter().all(|c| c.0 > last), "{what}: a chunk was left");
    let after = model_of(&fs.root());
    assert_eq!(after.keys().collect::<Vec<_>>(), before.keys().collect::<Vec<_>>(), "{what}");
    for (path, thing) in &before {
        assert!(after[path] == *thing, "{what}: {path} differs");
    }
}

#[test]
fn a_balance_moves_every_chunk_with_snapshots_and_clones() {
    let disks = disks_of(&mkfs_disks(1, &["-d", "single", "-m", "dup"]));
    let fs = mount_all(&disks, true, false).unwrap();
    let mut model = Model::new();
    write_file(&fs, &mut model, "small", 1, 5000);
    write_file(&fs, &mut model, "big", 2, 3 << 20);
    // Enough files for a tree of two levels.
    fs.root().create("many", Kind::Directory).unwrap();
    let many = fs.root().lookup("many").unwrap();
    for n in 0..300 {
        put(&many.create(&format!("f{n}"), Kind::File).unwrap(), 0, &noise(10 + n, 3000));
    }
    // Part of `big` twice more: a clone in another file and in itself.
    let big = fs.root().lookup("big").unwrap();
    let copy = fs.root().create("copy", Kind::File).unwrap();
    copy.clone_range(&big, 1 << 20, 1 << 20, 0).unwrap();
    big.clone_range(&big, 0, 256 << 10, 2 << 20).unwrap();
    fs.sync().unwrap();
    // A snapshot, then changes to the original: the two share part of
    // their trees, and part of their data.
    let root = fs.root();
    root.snapshot("snap", &root, false).unwrap();
    put(&big, 4096, &noise(3, 8192));
    write_file(&fs, &mut model, "later", 4, 200_000);
    root.snapshot("ro", &root, true).unwrap();
    fs.sync().unwrap();
    balanced_all(&fs, &every(0), "every chunk");
    // It writes on, and the disk holds it all.
    write_file(&fs, &mut Model::new(), "after", 5, 300_000);
    let expected = model_of(&fs.root());
    fs.sync().unwrap();
    drop(fs);
    let images = images_of(&disks);
    assert_checks(&images, "after a balance");
    let fs = mount_all(&disks_of(&images), false, false).unwrap();
    assert!(model_of(&fs.root()) == expected, "read back after a balance");
}

#[test]
fn a_balance_moves_compressed_extents_as_they_are() {
    let image = mkfs_with(&["--compress", "zstd"], &compressible_tree());
    let disk = writable(&image);
    let fs = mount_rw(&disk);
    let data = BalanceArgs { on: true, ..BalanceArgs::default() };
    let before = model_of(&fs.root());
    let done = fs.balance(&Balance { data, ..Balance::default() }).unwrap();
    assert!(done.moved >= 1 && done.moved < done.considered, "{done:?}");
    synced(&fs, &disk, &before, "compressed data moved");
}

#[test]
fn one_disk_gets_a_second_and_converts_to_raid1() {
    let mut disks = disks_of(&mkfs_disks(1, &["-d", "single", "-m", "dup"]));
    let fs = mount_all(&disks, true, false).unwrap();
    let mut model = Model::new();
    write_file(&fs, &mut model, "first", 1, 2 << 20);
    write_file(&fs, &mut model, "second", 2, 70_000);
    fs.sync().unwrap();
    // RAID1 needs two disks.
    assert_eq!(fs.balance(&every(kind::RAID1)).err(), Some(Error::InvalidArgument));
    let second = disks_of(&mkfs_disks(1, &[]));
    assert_eq!(fs.add_disk(disks[0].clone()).err(), Some(Error::InUse));
    assert_eq!(fs.add_disk(second[0].clone()), Ok(2));
    disks.push(second[0].clone());
    assert_eq!(fs.disks(), (2, 0));
    balanced_all(&fs, &every(kind::RAID1), "converted to RAID1");
    assert!(chunks(&fs).iter().all(|c| c.1 & kind::RAID1 != 0), "{:?}", chunks(&fs));
    // A soft convert leaves what is RAID1 already.
    let soft = BalanceArgs { on: true, convert: kind::RAID1, soft: true };
    let done = fs.balance(&Balance { data: soft, metadata: soft, system: soft }).unwrap();
    assert_eq!(done.moved, 0, "{done:?}");
    write_file(&fs, &mut model, "after", 3, 100_000);
    fs.sync().unwrap();
    drop(fs);
    let images = images_of(&disks);
    assert_checks(&images, "converted to RAID1");
    // Either disk alone holds it all.
    for i in 0..2 {
        let fs = mount_all(&disks_of(&images[i..=i]), false, true).unwrap();
        let read = model_of(&fs.root());
        for (path, thing) in &model {
            assert!(read[path] == *thing, "disk {i} alone: {path} differs");
        }
    }
    // And back to one copy, with metadata in two on one disk.
    let disks = disks_of(&images);
    let fs = mount_all(&disks, true, false).unwrap();
    let dup = BalanceArgs { on: true, convert: kind::DUP, soft: false };
    let single = BalanceArgs { on: true, convert: BALANCE_SINGLE, soft: false };
    let plan = Balance { data: single, metadata: dup, system: dup };
    balanced_all(&fs, &plan, "converted back");
    assert!(chunks(&fs).iter().all(|c| c.1 & kind::PROFILE & !kind::DUP == 0));
    fs.sync().unwrap();
    drop(fs);
    assert_checks(&images_of(&disks), "converted back");
}

#[test]
fn a_disk_is_removed_once_its_chunks_move_off_it() {
    let disks = disks_of(&mkfs_disks(3, &["-d", "raid1", "-m", "raid1"]));
    let fs = mount_all(&disks, true, false).unwrap();
    let mut model = Model::new();
    write_file(&fs, &mut model, "first", 1, 3 << 20);
    write_file(&fs, &mut model, "second", 2, 400_000);
    fs.sync().unwrap();
    assert_eq!(fs.remove_disk(9).err(), Some(Error::NotFound));
    let before = model_of(&fs.root());
    fs.remove_disk(3).unwrap();
    assert_eq!(fs.disks(), (2, 0));
    let on_third = |fs: &BtrfsFs| {
        fs.volume()
            .with_chunks(|map| map.chunks().iter().any(|c| c.stripes.iter().any(|s| s.devid == 3)))
    };
    assert!(!on_third(&fs), "a chunk is on the disk removed");
    assert!(model_of(&fs.root()) == before, "read after the removal");
    // RAID1 cannot go below two disks.
    assert_eq!(fs.remove_disk(2).err(), Some(Error::InvalidArgument));
    write_file(&fs, &mut model, "after", 3, 100_000);
    fs.sync().unwrap();
    drop(fs);
    let image = image_of(&disks[2], 128 << 20);
    assert!(&image[0x10040..0x10048] != b"_BHRfS_M", "the disk removed was wiped");
    let images = images_of(&disks[..2]);
    assert_checks(&images, "two disks left");
    let fs = mount_all(&disks_of(&images), false, false).unwrap();
    let read = model_of(&fs.root());
    for (path, thing) in &model {
        assert!(read[path] == *thing, "two disks left: {path} differs");
    }
}

#[test]
fn a_missing_disk_is_removed_from_the_copies_on_the_others() {
    let disks = disks_of(&mkfs_disks(3, &["-d", "raid1", "-m", "raid1"]));
    let fs = mount_all(&disks, true, false).unwrap();
    let mut model = Model::new();
    write_file(&fs, &mut model, "first", 1, 1 << 20);
    fs.sync().unwrap();
    drop(fs);
    let fs = mount_all(&disks[..2], true, true).unwrap();
    assert_eq!(fs.disks(), (3, 1));
    fs.remove_disk(3).unwrap();
    assert_eq!(fs.disks(), (2, 0));
    fs.sync().unwrap();
    drop(fs);
    let images = images_of(&disks[..2]);
    assert_checks(&images, "the missing disk removed");
    let fs = mount_all(&disks_of(&images), false, false).unwrap();
    assert_eq!(model_of(&fs.root())["/first"], model["/first"]);
}

#[test]
fn a_disk_put_in_the_place_of_a_missing_one_takes_chunks() {
    let disks = disks_of(&mkfs_disks(2, &["-d", "raid1", "-m", "raid1"]));
    let fs = mount_all(&disks, true, false).unwrap();
    let mut model = Model::new();
    write_file(&fs, &mut model, "first", 1, 1 << 20);
    fs.sync().unwrap();
    drop(fs);
    // The second disk is lost, and the first is written alone, so the
    // disks are counted while the second is missing.
    let fs = mount_all(&disks[..1], true, true).unwrap();
    write_file(&fs, &mut model, "alone", 2, 300_000);
    fs.sync().unwrap();
    let replaced = writable(&std::vec![0; 128 << 20]);
    fs.replace_disk(2, replaced.clone()).unwrap();
    let added = writable(&std::vec![0; 128 << 20]);
    assert_eq!(fs.add_disk(added.clone()), Ok(3));
    // The first disk's chunks go on the one put in its place and the
    // one added.
    fs.remove_disk(1).unwrap();
    assert_eq!(fs.disks(), (2, 0));
    write_file(&fs, &mut model, "after", 3, 100_000);
    fs.sync().unwrap();
    drop(fs);
    let images = images_of(&[replaced, added]);
    assert_checks(&images, "the first disk removed");
    let fs = mount_all(&disks_of(&images), false, false).unwrap();
    let read = model_of(&fs.root());
    for (path, thing) in &model {
        assert!(read[path] == *thing, "the first disk removed: {path} differs");
    }
}

#[test]
fn a_removal_the_other_disks_have_no_room_for_fails_and_leaves_them_writable() {
    let disks = disks_of(&mkfs_disks(3, &["-d", "raid1", "-m", "raid1"]));
    let fs = mount_all(&disks, true, false).unwrap();
    let mut model = Model::new();
    // As much as three disks hold in RAID1, more than two do.
    for n in 0..10 {
        write_file(&fs, &mut model, &format!("full{n}"), n, 10 << 20);
        fs.sync().unwrap();
    }
    assert_eq!(fs.remove_disk(3).err(), Some(Error::NoSpace));
    assert_eq!(fs.disks(), (3, 0));
    // Still written: a file removed makes room for another.
    fs.root().remove("full0").unwrap();
    model.remove("/full0");
    fs.sync().unwrap();
    write_file(&fs, &mut model, "after", 10, 100_000);
    fs.sync().unwrap();
    let read = model_of(&fs.root());
    for (path, thing) in &model {
        assert!(read[path] == *thing, "after the removal failed: {path} differs");
    }
    drop(fs);
    assert_checks(&images_of(&disks), "a removal that failed");
}

#[test]
fn a_read_only_mount_neither_balances_nor_adds_nor_removes() {
    let disks = disks_of(&mkfs_disks(2, &["-d", "raid1", "-m", "raid1"]));
    let fs = mount_all(&disks, false, false).unwrap();
    assert_eq!(fs.balance(&every(0)).err(), Some(Error::ReadOnly));
    assert_eq!(fs.remove_disk(2).err(), Some(Error::ReadOnly));
    let blank = writable(&std::vec![0; 128 << 20]);
    assert_eq!(fs.add_disk(blank).err(), Some(Error::ReadOnly));
}
