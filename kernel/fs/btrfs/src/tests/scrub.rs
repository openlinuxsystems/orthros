// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Host tests of scrub (ADR 0150): copies of tree blocks, data sectors
//! and the superblock damaged on the disk beneath a mount, found and
//! counted as Linux counts them, and mended from a good copy.

use radix_block::BlockDevice;
use radix_vfs::{Error, FileSystem, Kind, Scrub};

use super::files::{mount, noise};
use super::rw::{Model, Thing, mount_rw, put, synced};
use super::write::{btrfs_check, image_of, mkfs_empty, mkfs_sized, writable};
use crate::tree::HEADER_SIZE;

/// The errors a scrub found.
fn errors(counts: &Scrub) -> u64 {
    counts.read_errors
        + counts.csum_errors
        + counts.verify_errors
        + counts.super_errors
        + counts.uncorrectable_errors
}

/// Where on the disk `image` holds the 4 KiB `block`, which is unique
/// but for its copies.
fn places_of(image: &[u8], block: &[u8]) -> Vec<usize> {
    (0..image.len() / 4096)
        .map(|at| at * 4096)
        .filter(|&at| image[at..at + 4096] == *block)
        .collect()
}

#[test]
fn scrub_finds_damaged_copies_and_mends_them_from_good_ones() {
    let disk = writable(&mkfs_sized(256 << 20, &["-m", "dup", "-d", "dup"]));
    let fs = mount_rw(&disk);
    let mut model = Model::new();
    let data = noise(7, 300_000);
    let file = fs.root().create("file", Kind::File).unwrap();
    put(&file, 0, &data);
    model.insert("/file".into(), Thing::File(data.clone()));
    synced(&fs, &disk, &model, "written");
    let clean = fs.scrub(false).unwrap();
    assert_eq!(errors(&clean), 0, "{clean:?}");
    assert_eq!(clean.corrected_errors, 0);
    assert!(clean.tree_extents >= 2 * 6, "{clean:?}");
    // Each block of the file, twice.
    assert!(clean.data_bytes >= 2 * 300_000, "{clean:?}");
    assert_eq!(clean.fsid, fs.volume().superblock().fsid);
    // The second copy of the root tree's block, and the first of a
    // block of the file's data.
    let len = (disk.info().sectors * radix_block::SECTOR) as usize;
    let image = image_of(&disk, len);
    let root = fs.volume().root_tree();
    let tree_copy = fs.volume().place(root.bytenr).unwrap().copies()[1];
    let data_copies = places_of(&image, &data[8192..12288]);
    assert_eq!(data_copies.len(), 2);
    let damage = |at: u64| {
        let mut byte = [0];
        disk.dump(at, &mut byte);
        disk.load(at, &[byte[0] ^ 0x40]).unwrap();
    };
    damage(tree_copy + HEADER_SIZE as u64 + 3);
    damage(data_copies[0] as u64 + 100);
    // A scrub that only reads finds them, and that a good copy of each
    // is there, and leaves the disk as it is.
    let found = fs.scrub(false).unwrap();
    assert_eq!(found.csum_errors, 4 + 1, "{found:?}");
    assert_eq!(found.corrected_errors, 4 + 1, "{found:?}");
    assert_eq!(found.uncorrectable_errors, 0, "{found:?}");
    assert_eq!(found.tree_extents, clean.tree_extents);
    assert_ne!(image_of(&disk, len), image);
    // One that repairs writes the good copies over the bad ones.
    let mended = fs.scrub(true).unwrap();
    assert_eq!((mended.csum_errors, mended.corrected_errors), (5, 5), "{mended:?}");
    assert_eq!(image_of(&disk, len), image);
    let again = fs.scrub(true).unwrap();
    assert_eq!(errors(&again) + again.corrected_errors, 0, "{again:?}");
    synced(&fs, &disk, &model, "mended");
}

#[test]
fn scrub_counts_what_no_copy_mends() {
    let disk = writable(&mkfs_empty(&[]));
    let fs = mount_rw(&disk);
    let mut model = Model::new();
    let data = noise(8, 100_000);
    let file = fs.root().create("file", Kind::File).unwrap();
    put(&file, 0, &data);
    model.insert("/file".into(), Thing::File(data.clone()));
    let dir = fs.root().create("dir", Kind::Directory).unwrap();
    model.insert("/dir".into(), Thing::Dir);
    for i in 0..50 {
        let name = format!("a file with a long name, {i}");
        dir.create(&name, Kind::File).unwrap();
        model.insert(format!("/dir/{name}"), Thing::File(Vec::new()));
    }
    synced(&fs, &disk, &model, "written");
    let len = (disk.info().sectors * radix_block::SECTOR) as usize;
    let image = image_of(&disk, len);
    // Data has one copy; both copies of the top subvolume's block are
    // damaged, and the superblock's copy at 64 MiB.
    let data_copies = places_of(&image, &data[4096..8192]);
    assert_eq!(data_copies.len(), 1);
    let top = fs.volume().tree(5).unwrap().unwrap();
    let tree_copies = fs.volume().place(top.bytenr).unwrap().copies().to_vec();
    assert_eq!(tree_copies.len(), 2);
    let damage = |at: u64| {
        let mut byte = [0];
        disk.dump(at, &mut byte);
        disk.load(at, &[byte[0] ^ 0x01]).unwrap();
    };
    damage(data_copies[0] as u64 + 4000);
    for &copy in &tree_copies {
        damage(copy + HEADER_SIZE as u64 + 3);
    }
    damage((64 << 20) + 0x100);
    let found = fs.scrub(true).unwrap();
    assert_eq!(found.csum_errors, 1 + 2 * 4, "{found:?}");
    assert_eq!(found.uncorrectable_errors, 1 + 2 * 4, "{found:?}");
    assert_eq!(found.corrected_errors, 0, "{found:?}");
    assert_eq!(found.super_errors, 1, "{found:?}");
    // The next commit writes the superblock's copies again, and the top
    // subvolume's block anew; the file's sector stays bad.
    fs.root().create("after", Kind::File).unwrap();
    fs.sync().unwrap();
    let after = fs.scrub(false).unwrap();
    assert_eq!(after.super_errors, 0, "{after:?}");
    assert_eq!(after.uncorrectable_errors, 1, "{after:?}");
}

#[test]
fn a_read_only_mount_scrubs_without_mending() {
    let disk = writable(&mkfs_empty(&["-m", "dup"]));
    let fs = mount_rw(&disk);
    let file = fs.root().create("file", Kind::File).unwrap();
    put(&file, 0, &noise(9, 50_000));
    fs.sync().unwrap();
    let image = image_of(&disk, (disk.info().sectors * radix_block::SECTOR) as usize);
    assert!(btrfs_check(&image).0);
    let ro = mount(&image).unwrap();
    assert_eq!(ro.scrub(true), Err(Error::ReadOnly));
    let counts = ro.scrub(false).unwrap();
    assert_eq!(errors(&counts), 0, "{counts:?}");
    assert!(counts.data_bytes >= 50_000, "{counts:?}");
}
