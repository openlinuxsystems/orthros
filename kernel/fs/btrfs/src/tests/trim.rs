// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Trims (ADR 0158): what is discarded is only what no file and no tree
//! holds, on every copy, never a superblock, and the filesystem reads
//! and checks whole after it, even while files change during it.

use std::format;
use std::vec::Vec;

use radix_vfs::{Error, FileSystem, Kind};

use super::FILE;
use super::files::noise;
use super::raid::{
    assert_checks, assert_reads, disks_of, images_of, mkfs_disks, mount_all, write_file,
};
use super::rw::{Model, Thing, mount_rw, put, synced};
use super::write::{image_of, mkfs_sized, writable};
use crate::scrub::SUPERS;
use crate::superblock;

/// The bytes of each copy of the superblock on `image`, and its first MiB.
fn kept_bytes(image: &[u8]) -> Vec<Vec<u8>> {
    let mut kept = std::vec![image[..1 << 20].to_vec()];
    for at in SUPERS.into_iter().map(|at| at as usize) {
        if at + superblock::SIZE <= image.len() {
            kept.push(image[at..at + superblock::SIZE].to_vec());
        }
    }
    kept
}

#[test]
fn a_trim_discards_free_space_and_every_file_reads_and_checks_after() {
    let size = 256u64 << 20;
    let disk = writable(&mkfs_sized(size, &[]));
    let fs = mount_rw(&disk);
    let mut model = Model::new();
    let root = fs.root();
    // Files that take data chunks' space, and some removed, freeing it.
    for n in 0..12u64 {
        let name = format!("f{n}");
        let data = noise(n + 1, (n as usize + 1) * 300_000);
        let file = root.create(&name, Kind::File, FILE).unwrap();
        put(&file, 0, &data);
        model.insert(format!("/{name}"), Thing::File(data));
    }
    fs.sync().unwrap();
    for n in (0..12u64).step_by(3) {
        root.remove(&format!("f{n}")).unwrap();
        model.remove(&format!("/f{n}"));
    }
    fs.sync().unwrap();
    let before = image_of(&disk, size as usize);
    let trimmed = fs.trim(0).unwrap();
    let after = image_of(&disk, size as usize);
    assert!(trimmed > 64 << 20, "only {trimmed} bytes trimmed");
    assert_eq!(disk.discarded() * 512, trimmed);
    assert_eq!(kept_bytes(&before), kept_bytes(&after), "a superblock or the disk's start changed");
    // Every file reads, and btrfs check finds every block and checksum.
    synced(&fs, &disk, &model, "after a trim");
    // A trim of ranges of at least 1 GiB finds none, and trims nothing.
    assert_eq!(fs.trim(1 << 30).unwrap(), 0);
    // Files written after it read back too.
    write_file(&fs, &mut model, "later", 99, 2 << 20);
    synced(&fs, &disk, &model, "written after a trim");
}

#[test]
fn a_trim_reaches_every_copy_and_passes_over_a_missing_disk() {
    let disks = disks_of(&mkfs_disks(2, &["-d", "raid1", "-m", "raid1"]));
    let fs = mount_all(&disks, true, false).unwrap();
    let mut model = Model::new();
    write_file(&fs, &mut model, "kept", 1, 3 << 20);
    write_file(&fs, &mut model, "gone", 2, 5 << 20);
    fs.sync().unwrap();
    fs.root().remove("gone").unwrap();
    model.remove("/gone");
    fs.sync().unwrap();
    let trimmed = fs.trim(0).unwrap();
    let (first, second) = (disks[0].discarded() * 512, disks[1].discarded() * 512);
    assert!(first > 0 && second > 0, "{first} and {second} bytes discarded");
    assert_eq!(first + second, trimmed);
    fs.sync().unwrap();
    drop(fs);
    let images = images_of(&disks);
    assert_checks(&images, "raid1 after a trim");
    // With the second disk missing, only the first is trimmed.
    let alone = disks_of(&images[..1]);
    let fs = mount_all(&alone, true, true).unwrap();
    assert_reads(&fs, &model, "raid1 with a disk missing");
    let trimmed = fs.trim(0).unwrap();
    assert_eq!(alone[0].discarded() * 512, trimmed);
    assert_reads(&fs, &model, "raid1 with a disk missing, trimmed");
}

#[test]
fn a_trim_while_files_change_takes_nothing_they_hold() {
    let size = 256u64 << 20;
    let disk = writable(&mkfs_sized(size, &[]));
    let fs = mount_rw(&disk);
    let mut model = Model::new();
    std::thread::scope(|scope| {
        let trims = scope.spawn(|| {
            let mut trimmed = 0;
            for _ in 0..20 {
                trimmed += fs.trim(0).unwrap();
            }
            trimmed
        });
        let root = fs.root();
        for round in 0..40u64 {
            let name = format!("r{}", round % 8);
            if model.contains_key(&format!("/{name}")) {
                root.remove(&name).unwrap();
                model.remove(&format!("/{name}"));
            }
            let data = noise(round + 7, 200_000 + (round as usize % 5) * 150_000);
            let file = root.create(&name, Kind::File, FILE).unwrap();
            put(&file, 0, &data);
            model.insert(format!("/{name}"), Thing::File(data));
            if round % 4 == 3 {
                fs.sync().unwrap();
            }
        }
        assert!(trims.join().unwrap() > 0);
    });
    synced(&fs, &disk, &model, "files changed during trims");
}

#[test]
fn a_trim_keeps_what_the_last_commit_holds() {
    let size = 256u64 << 20;
    let disk = writable(&mkfs_sized(size, &[]));
    let fs = mount_rw(&disk);
    let mut committed = Model::new();
    write_file(&fs, &mut committed, "a", 5, 4 << 20);
    write_file(&fs, &mut committed, "b", 6, 6 << 20);
    fs.sync().unwrap();
    // Removed, and rewritten, but not committed: the disk's trees still
    // hold the old blocks, which a power cut now goes back to.
    fs.root().remove("a").unwrap();
    let b = fs.root().lookup("b").unwrap();
    put(&b, 0, &noise(7, 1 << 20));
    let mut model = Model::new();
    write_file(&fs, &mut model, "c", 8, 3 << 20);
    assert!(fs.trim(0).unwrap() > 0);
    let cut = image_of(&disk, size as usize);
    let (whole, text) = super::write::btrfs_check(&cut);
    assert!(whole, "btrfs check failed after a trim and a power cut:\n{text}");
    let after = super::files::mount(&cut).unwrap();
    assert_reads(&after, &committed, "the last commit, after a trim");
}

#[test]
fn a_read_only_mount_is_not_trimmed() {
    let image = mkfs_sized(128 << 20, &[]);
    let fs = super::files::mount(&image).unwrap();
    assert_eq!(fs.trim(0), Err(Error::ReadOnly));
}
