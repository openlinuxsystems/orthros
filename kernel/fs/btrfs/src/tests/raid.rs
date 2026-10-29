// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Host tests of Btrfs on several disks (ADR 0153): filesystems that
//! `mkfs.btrfs` makes across several images, in each profile, read,
//! written, and checked by `btrfs check`, which finds the other images
//! through a blkid cache file that names them; mounts with a disk
//! missing; a disk failing under a mount; a disk that missed commits
//! brought up to date by a scrub; and a log of fsyncs replayed from
//! either disk.

use std::collections::BTreeSet;
use std::format;
use std::path::PathBuf;
use std::process::Command;
use std::string::String;
use std::vec::Vec;

use radix_block::DeviceRef;
use radix_block::ram::RamDisk;
use radix_vfs::mem::KArc;
use radix_vfs::{Error, FileSystem, Kind};

use super::files::noise;
use super::rw::{Model, Thing, put, snapshot};
use super::write::{image_of, writable, write_sparse};
use super::{scratch, tool};
use crate::BtrfsFs;
use crate::chunk::kind;

/// The size of each disk.
const DISK: u64 = 128 << 20;

/// A filesystem `mkfs.btrfs args` makes across `n` disks of [`DISK`]
/// bytes: each disk's image.
pub(super) fn mkfs_disks(n: usize, args: &[&str]) -> Vec<Vec<u8>> {
    let paths: Vec<PathBuf> = (0..n).map(|_| scratch("disk")).collect();
    for path in &paths {
        std::fs::File::create(path).unwrap().set_len(DISK).unwrap();
    }
    let out = Command::new(tool("mkfs.btrfs"))
        .args(["-q", "-f"])
        .args(args)
        .args(&paths)
        .stdin(std::process::Stdio::null())
        .output()
        .unwrap();
    assert!(out.status.success(), "mkfs.btrfs: {}", String::from_utf8_lossy(&out.stderr));
    paths
        .iter()
        .map(|path| {
            let image = std::fs::read(path).unwrap();
            let _ = std::fs::remove_file(path);
            image
        })
        .collect()
}

/// A disk that can be written for each image.
pub(super) fn disks_of(images: &[Vec<u8>]) -> Vec<KArc<RamDisk>> {
    images.iter().map(|image| writable(image)).collect()
}

/// What each disk holds.
pub(super) fn images_of(disks: &[KArc<RamDisk>]) -> Vec<Vec<u8>> {
    disks.iter().map(|disk| image_of(disk, DISK as usize)).collect()
}

/// The filesystem on `disks`, the first named to the mount.
pub(super) fn mount_all(
    disks: &[KArc<RamDisk>],
    writable: bool,
    degraded: bool,
) -> Result<KArc<BtrfsFs>, Error> {
    let devices: Vec<DeviceRef> = disks.iter().map(|disk| disk.clone() as DeviceRef).collect();
    BtrfsFs::mount_disks(&devices, writable, degraded)
}

/// The filesystem's UUID, as text, from a disk's superblock.
fn fsid_of(image: &[u8]) -> String {
    let id = &image[0x10020..0x10030];
    let hex: Vec<String> = id.iter().map(|b| format!("{b:02x}")).collect();
    let hex = hex.concat();
    format!("{}-{}-{}-{}-{}", &hex[..8], &hex[8..12], &hex[12..16], &hex[16..20], &hex[20..])
}

/// What `btrfs check` says of the filesystem on `images`: whether it
/// found it whole, and what it printed. It is shown every image by a
/// blkid cache file, as libblkid would list the disks.
fn check_disks(images: &[Vec<u8>]) -> (bool, String) {
    let dir = scratch("check");
    std::fs::create_dir_all(&dir).unwrap();
    let fsid = fsid_of(&images[0]);
    let mut cache = String::new();
    let mut paths = Vec::new();
    for (n, image) in images.iter().enumerate() {
        let path = dir.join(format!("disk{n}"));
        write_sparse(&path, image);
        cache += &format!(
            "<device DEVNO=\"0x07{n:02x}\" TIME=\"1.1\" TYPE=\"btrfs\" UUID=\"{fsid}\">{}</device>\n",
            path.display()
        );
        paths.push(path);
    }
    let tab = dir.join("blkid.tab");
    std::fs::write(&tab, cache).unwrap();
    let out = Command::new(tool("btrfs"))
        .args(["check", "--readonly", "--check-data-csum"])
        .arg(&paths[0])
        .env("BLKID_FILE", &tab)
        .stdin(std::process::Stdio::null())
        .output()
        .unwrap();
    let _ = std::fs::remove_dir_all(&dir);
    let text =
        format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
    let whole = out.status.success() && !text.contains("ERROR") && !text.contains("missing");
    (whole, text)
}

pub(super) fn assert_checks(images: &[Vec<u8>], what: &str) {
    let (whole, text) = check_disks(images);
    assert!(whole, "{what}: btrfs check failed:\n{text}");
}

/// Checks the filesystem `fs` reads `model`.
pub(super) fn assert_reads(fs: &BtrfsFs, model: &Model, what: &str) {
    let read = snapshot(&fs.root());
    assert_eq!(read.keys().collect::<Vec<_>>(), model.keys().collect::<Vec<_>>(), "{what}");
    for (path, thing) in model {
        assert!(read[path] == *thing, "{what}: {path} differs");
    }
}

/// Writes a file of `len` bytes of noise from `seed` at `name` in the top
/// directory of `fs`, and notes it in `model`.
pub(super) fn write_file(fs: &BtrfsFs, model: &mut Model, name: &str, seed: u64, len: usize) {
    let data = noise(seed, len);
    let file = fs.root().create(name, Kind::File).unwrap();
    put(&file, 0, &data);
    model.insert(format!("/{name}"), Thing::File(data));
}

#[test]
fn each_profile_across_disks_is_read_written_and_passes_btrfs_check() {
    let cases = [
        ("raid1", "raid1", 2),
        ("raid10", "raid10", 4),
        ("raid0", "raid1", 3),
        ("raid1c3", "raid1c3", 3),
        ("raid1c4", "raid1c4", 4),
        ("single", "raid1", 2),
    ];
    for (data, metadata, n) in cases {
        let what = format!("-d {data} -m {metadata} on {n} disks");
        let disks = disks_of(&mkfs_disks(n, &["-d", data, "-m", metadata]));
        let fs = mount_all(&disks, true, false).unwrap();
        assert_eq!(fs.volume().disks().len(), n, "{what}");
        let mut model = Model::new();
        write_file(&fs, &mut model, "small", 1, 5000);
        write_file(&fs, &mut model, "big", 2, 3 << 20);
        fs.root().create("dir", Kind::Directory).unwrap();
        model.insert("/dir".into(), Thing::Dir);
        fs.sync().unwrap();
        drop(fs);
        let images = images_of(&disks);
        assert_checks(&images, &what);
        // The disks in another order: the mount finds which is which.
        let mut again = disks_of(&images);
        again.reverse();
        let fs = mount_all(&again, false, false).unwrap();
        assert_reads(&fs, &model, &what);
    }
}

#[test]
fn new_chunks_spread_over_the_disks_as_linux_spreads_them() {
    let disks = disks_of(&mkfs_disks(4, &["-d", "raid10", "-m", "raid1c3"]));
    let fs = mount_all(&disks, true, false).unwrap();
    let chunks = |fs: &BtrfsFs| fs.volume().with_chunks(|map| map.chunks().len());
    let before = chunks(&fs);
    let mut model = Model::new();
    let mut n = 0;
    // Past the 128 MiB data chunk mkfs made.
    while chunks(&fs) == before && n < 44 {
        write_file(&fs, &mut model, &format!("f{n}"), 10 + n, 4 << 20);
        fs.sync().unwrap();
        n += 1;
    }
    assert!(chunks(&fs) > before, "no new chunk after {n} files");
    fs.volume().with_chunks(|map| {
        for chunk in map.chunks() {
            let devids: BTreeSet<u64> = chunk.stripes.iter().map(|s| s.devid).collect();
            assert_eq!(devids.len(), chunk.stripes.len(), "{chunk:?}: two stripes on one disk");
            match chunk.kind & kind::PROFILE {
                kind::RAID10 => {
                    assert_eq!(chunk.stripes.len(), 4, "{chunk:?}");
                    assert_eq!(chunk.sub_stripes, 2, "{chunk:?}");
                    assert_eq!(chunk.kind & kind::DATA, kind::DATA, "{chunk:?}");
                }
                kind::RAID1C3 => assert_eq!(chunk.stripes.len(), 3, "{chunk:?}"),
                _ => panic!("{chunk:?}: a profile mkfs did not ask for"),
            }
        }
    });
    drop(fs);
    let images = images_of(&disks);
    assert_checks(&images, "new chunks");
    let fs = mount_all(&disks_of(&images), false, false).unwrap();
    assert_reads(&fs, &model, "new chunks");
}

#[test]
fn a_missing_disk_mounts_only_degraded_and_only_with_every_chunk() {
    let disks = disks_of(&mkfs_disks(2, &["-d", "raid1", "-m", "raid1"]));
    let fs = mount_all(&disks, true, false).unwrap();
    let mut model = Model::new();
    write_file(&fs, &mut model, "first", 1, 300_000);
    fs.sync().unwrap();
    drop(fs);
    let first = &disks[..1];
    assert_eq!(mount_all(first, false, false).err(), Some(Error::NoDevice));
    let fs = mount_all(first, true, true).unwrap();
    assert_eq!(fs.volume().missing(), 1);
    assert!(fs.volume().readable());
    assert_reads(&fs, &model, "degraded");
    // Written while the second disk is away: it misses this commit.
    write_file(&fs, &mut model, "second", 2, 200_000);
    fs.sync().unwrap();
    drop(fs);
    let fs = mount_all(&disks, true, false).unwrap();
    assert_eq!(fs.volume().missing(), 0);
    assert!(fs.volume().disks()[1].behind(), "the second disk missed a commit");
    assert_reads(&fs, &model, "back, before the scrub");
    // A commit before the scrub leaves the second disk's superblock as it
    // was, so the next mount finds it behind too.
    write_file(&fs, &mut model, "third", 3, 100_000);
    fs.sync().unwrap();
    drop(fs);
    let fs = mount_all(&disks, true, false).unwrap();
    assert!(fs.volume().disks()[1].behind(), "a commit caught the second disk up");
    assert_reads(&fs, &model, "back, mounted again before the scrub");
    let counts = fs.scrub(true).unwrap();
    assert_eq!(counts.uncorrectable_errors, 0, "{counts:?}");
    assert!(counts.corrected_errors > 0, "{counts:?}");
    assert!(!fs.volume().disks()[1].behind());
    fs.sync().unwrap();
    drop(fs);
    let images = images_of(&disks);
    assert_checks(&images, "after the scrub");
    // The second disk alone now holds everything.
    let fs = mount_all(&disks_of(&images[1..]), false, true).unwrap();
    assert_reads(&fs, &model, "the second disk alone");
    // RAID0 data has no other copy: no mount without every disk.
    let disks = disks_of(&mkfs_disks(2, &["-d", "raid0", "-m", "raid1"]));
    assert_eq!(mount_all(&disks[1..], false, true).err(), Some(Error::NoDevice));
}

#[test]
fn an_fsync_on_several_disks_replays_from_any_that_hold_it_all() {
    let disks = disks_of(&mkfs_disks(2, &["-d", "raid1", "-m", "raid1"]));
    let fs = mount_all(&disks, true, false).unwrap();
    let mut model = Model::new();
    write_file(&fs, &mut model, "file", 1, 300_000);
    fs.sync().unwrap();
    // Overwritten and made durable by the log of fsyncs alone (ADR 0154),
    // and the power cut before the commit.
    let file = fs.root().lookup("file").unwrap();
    let data = noise(2, 4096);
    put(&file, 8192, &data);
    let Some(Thing::File(bytes)) = model.get_mut("/file") else { unreachable!() };
    bytes[8192..8192 + 4096].copy_from_slice(&data);
    file.sync().unwrap();
    let images = images_of(&disks);
    drop(file);
    drop(fs);
    for (i, image) in images.iter().enumerate() {
        let log_root = u64::from_le_bytes(image[0x10060..0x10068].try_into().unwrap());
        assert_ne!(log_root, 0, "disk {i}'s superblock names the log");
    }
    for (what, set) in [
        ("both disks", &images[..]),
        ("the first alone", &images[..1]),
        ("the second alone", &images[1..]),
    ] {
        let fs = mount_all(&disks_of(set), true, set.len() < 2).unwrap();
        assert_reads(&fs, &model, what);
    }
    let disks = disks_of(&images);
    drop(mount_all(&disks, true, false).unwrap());
    assert_checks(&images_of(&disks), "after the replay");
}

#[test]
fn a_disk_that_fails_under_a_mount_is_left_out() {
    let disks = disks_of(&mkfs_disks(2, &["-d", "raid1", "-m", "raid1"]));
    let fs = mount_all(&disks, true, false).unwrap();
    let mut model = Model::new();
    write_file(&fs, &mut model, "before", 1, 300_000);
    fs.sync().unwrap();
    disks[1].fail_after(0);
    write_file(&fs, &mut model, "after", 2, 300_000);
    fs.sync().unwrap();
    assert_eq!(fs.volume().missing(), 1, "the failed disk is left out");
    assert_reads(&fs, &model, "after the failure");
    drop(fs);
    disks[1].fail_after(u32::MAX);
    let images = images_of(&disks);
    let fs = mount_all(&disks_of(&images[..1]), false, true).unwrap();
    assert_reads(&fs, &model, "the disk that stayed");
    // With RAID0 data, a failed disk loses data: the sync fails.
    let disks = disks_of(&mkfs_disks(2, &["-d", "raid0", "-m", "raid1"]));
    let fs = mount_all(&disks, true, false).unwrap();
    let mut model = Model::new();
    disks[1].fail_after(0);
    write_file(&fs, &mut model, "lost", 3, 1 << 20);
    assert!(fs.sync().is_err());
}

/// A disk of `len` bytes that holds nothing.
fn blank(len: u64) -> KArc<RamDisk> {
    writable(&vec![0; len as usize])
}

/// Whether `image` holds a Btrfs superblock where the first one goes.
fn has_superblock(image: &[u8]) -> bool {
    &image[0x10040..0x10048] == b"_BHRfS_M"
}

#[test]
fn a_missing_disk_is_replaced_and_rebuilt_from_the_copies() {
    let disks = disks_of(&mkfs_disks(2, &["-d", "raid1", "-m", "raid1"]));
    let fs = mount_all(&disks, true, false).unwrap();
    let mut model = Model::new();
    write_file(&fs, &mut model, "first", 1, 600_000);
    fs.sync().unwrap();
    drop(fs);
    // The second disk is lost; the first goes on alone, and is written.
    let fs = mount_all(&disks[..1], true, true).unwrap();
    write_file(&fs, &mut model, "alone", 2, 300_000);
    let new = blank(DISK);
    let counts = fs.replace_disk(2, new.clone()).unwrap();
    assert_eq!(counts.uncorrectable_errors, 0, "{counts:?}");
    assert_eq!(fs.disks(), (2, 0), "the new disk is one of them");
    assert!(!fs.volume().disks()[1].behind(), "the rebuild caught it up");
    assert_reads(&fs, &model, "rebuilt");
    write_file(&fs, &mut model, "after", 3, 200_000);
    fs.sync().unwrap();
    drop(fs);
    let images = images_of(&[disks[0].clone(), new]);
    assert_checks(&images, "after the replace");
    // Either disk alone holds everything now.
    for (i, what) in [(0, "the first disk alone"), (1, "the new disk alone")] {
        let fs = mount_all(&disks_of(&images[i..=i]), false, true).unwrap();
        assert_reads(&fs, &model, what);
    }
}

#[test]
fn a_disk_there_is_replaced_and_its_superblocks_wiped() {
    let disks = disks_of(&mkfs_disks(2, &["-d", "raid1", "-m", "raid1"]));
    let fs = mount_all(&disks, true, false).unwrap();
    let mut model = Model::new();
    write_file(&fs, &mut model, "first", 1, 400_000);
    fs.sync().unwrap();
    // The disk the mount named, larger than the one it replaces.
    let new = blank(DISK + (16 << 20));
    let counts = fs.replace_disk(1, new.clone()).unwrap();
    assert_eq!(counts.uncorrectable_errors, 0, "{counts:?}");
    write_file(&fs, &mut model, "after", 2, 100_000);
    fs.sync().unwrap();
    drop(fs);
    assert!(!has_superblock(&image_of(&disks[0], DISK as usize)), "the old disk was wiped");
    let images = [image_of(&new, (DISK + (16 << 20)) as usize), image_of(&disks[1], DISK as usize)];
    assert_checks(&images, "the new disk and the second");
    let fs = mount_all(&disks_of(&images), false, false).unwrap();
    assert_reads(&fs, &model, "the new disk and the second");
}

#[test]
fn a_replace_that_cannot_be_done_changes_nothing() {
    let disks = disks_of(&mkfs_disks(2, &["-d", "raid1", "-m", "raid1"]));
    let fs = mount_all(&disks, true, false).unwrap();
    assert_eq!(fs.replace_disk(9, blank(DISK)).err(), Some(Error::NotFound));
    assert_eq!(fs.replace_disk(2, disks[0].clone()).err(), Some(Error::InUse));
    assert_eq!(fs.replace_disk(2, blank(DISK / 2)).err(), Some(Error::NoSpace));
    assert_eq!(fs.disks(), (2, 0));
    drop(fs);
    let fs = mount_all(&disks, false, false).unwrap();
    assert_eq!(fs.replace_disk(2, blank(DISK)).err(), Some(Error::ReadOnly));
    drop(fs);
    // RAID0 data has no copy elsewhere to rebuild it from.
    let disks = disks_of(&mkfs_disks(2, &["-d", "raid0", "-m", "raid1"]));
    let fs = mount_all(&disks, true, false).unwrap();
    assert_eq!(fs.replace_disk(2, blank(DISK)).err(), Some(Error::NotSupported));
    // A rebuild onto a disk that fails leaves the one it replaced.
    let disks = disks_of(&mkfs_disks(2, &["-d", "raid1", "-m", "raid1"]));
    let fs = mount_all(&disks, true, false).unwrap();
    let mut model = Model::new();
    write_file(&fs, &mut model, "first", 1, 400_000);
    fs.sync().unwrap();
    let new = blank(DISK);
    new.fail_after(8);
    assert_eq!(fs.replace_disk(2, new).err(), Some(Error::Io));
    assert_eq!(fs.disks(), (2, 1), "the failed new disk is left out");
    drop(fs);
    assert!(has_superblock(&image_of(&disks[1], DISK as usize)), "the old disk was kept");
    let images = images_of(&disks);
    let fs = mount_all(&disks_of(&images), false, false).unwrap();
    assert_reads(&fs, &model, "both old disks");
}
