// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Host tests of the exFAT driver, against the reference tools: volumes
//! are made by exfatprogs' `mkfs.exfat`, what Radix writes is checked by
//! its `fsck.exfat` and read back by The Sleuth Kit's `fls` and `icat`, an
//! exFAT reader of their own (`apt install exfatprogs sleuthkit`; `ob
//! doctor` checks for them), and a volume exfat-fuse wrote is read
//! ([`FUSE_MADE`]). Volumes of 4 KiB sectors, which `mkfs.exfat` makes
//! only on disks that have them, are made by [`format_4k`] from the
//! specification, and checked by `fsck.exfat` too.

use std::collections::BTreeMap;
use std::collections::btree_map::Entry;
use std::path::PathBuf;
use std::process::Command;
use std::string::String;
use std::sync::atomic::{AtomicU32, Ordering};
use std::vec::Vec;
use std::{format, vec};

use proptest::prelude::*;
use radix_block::ram::RamDisk;
use radix_block::{BlockDevice, DeviceRef};
use radix_vfs::mem::{self, KArc};
use radix_vfs::{Error, FileSystem, Kind, NodeRef, Slice, SliceMut};

use crate::ExfatFs;
use crate::boot::{self, REGION_SECTORS};
use crate::entry::{self, ENTRY, File, Stream, kind};

/// 2026-10-22 00:42:22 UTC, for files' times.
fn clock() -> u64 {
    1_792_629_742_000_000_000
}

/// A tool of exfatprogs.
fn tool(name: &str) -> PathBuf {
    for dir in ["/usr/sbin", "/sbin", "/usr/bin", "/bin", "/usr/local/bin", "/usr/local/sbin"] {
        let path = PathBuf::from(dir).join(name);
        if path.exists() {
            return path;
        }
    }
    panic!("{name} is missing: install exfatprogs (apt install exfatprogs)");
}

/// A file name for a scratch image.
fn scratch(what: &str) -> PathBuf {
    static NEXT: AtomicU32 = AtomicU32::new(0);
    let n = NEXT.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!("radix-exfat-{}-{what}-{n}.img", std::process::id()))
}

/// The kinds of volume the tests use: (MiB, cluster bytes), with clusters
/// smaller than a sector's page, a page, and larger.
const VOLUMES: [(u32, u32); 4] = [(8, 512), (16, 4096), (64, 32768), (48, 131_072)];

/// A fresh volume from `mkfs.exfat`, labelled `RADIXTEST`.
fn mkfs(mib: u32, cluster: u32) -> Vec<u8> {
    let path = scratch("mkfs");
    let file = std::fs::File::create(&path).unwrap();
    file.set_len(u64::from(mib) << 20).unwrap();
    drop(file);
    let out = Command::new(tool("mkfs.exfat"))
        .args(["-q", "-L", "RADIXTEST", "-c", &cluster.to_string()])
        .arg(&path)
        .output()
        .unwrap();
    assert!(out.status.success(), "mkfs.exfat: {}", String::from_utf8_lossy(&out.stderr));
    let image = std::fs::read(&path).unwrap();
    let _ = std::fs::remove_file(&path);
    image
}

/// What `fsck.exfat` says of `image`, and whether it found it clean.
fn fsck_report(image: &[u8]) -> (bool, String) {
    let path = scratch("fsck");
    std::fs::write(&path, image).unwrap();
    let out = Command::new(tool("fsck.exfat")).arg("-n").arg(&path).output().unwrap();
    let _ = std::fs::remove_file(&path);
    let text =
        format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
    (out.status.success(), text)
}

/// Checks `image` with `fsck.exfat`, which must find nothing wrong.
fn fsck(image: &[u8]) {
    let (clean, text) = fsck_report(image);
    assert!(clean, "fsck.exfat found problems:\n{text}");
}

/// Every file of `image` but the volume's own, by path, as The Sleuth
/// Kit reads it: `fls` lists them and `icat` copies each out.
fn sleuthkit_files(image: &[u8]) -> BTreeMap<String, Vec<u8>> {
    let path = scratch("tsk");
    std::fs::write(&path, image).unwrap();
    let out = Command::new(tool("fls"))
        .args(["-r", "-p", "-u", "-F", "-f", "exfat"])
        .arg(&path)
        .output()
        .unwrap();
    assert!(out.status.success(), "fls: {}", String::from_utf8_lossy(&out.stderr));
    let mut files = BTreeMap::new();
    for line in String::from_utf8(out.stdout).unwrap().lines() {
        // `r/r 323:	dir/name`
        let (number, name) = line.split_once(":\t").unwrap();
        let number = number.rsplit(' ').next().unwrap();
        if name.starts_with('$') || name.ends_with("(Volume Label Entry)") {
            continue;
        }
        let data = Command::new(tool("icat"))
            .args(["-f", "exfat"])
            .arg(&path)
            .arg(number)
            .output()
            .unwrap();
        assert!(data.status.success(), "icat {name}: {}", String::from_utf8_lossy(&data.stderr));
        files.insert(String::from(name), data.stdout);
    }
    let _ = std::fs::remove_file(&path);
    files
}

/// Bytes that differ from offset to offset and file to file.
fn pattern(seed: u32, len: usize) -> Vec<u8> {
    let mut x = seed.wrapping_mul(2_654_435_761) | 1;
    (0..len)
        .map(|_| {
            x ^= x << 13;
            x ^= x >> 17;
            x ^= x << 5;
            x as u8
        })
        .collect()
}

/// A volume on a RAM disk.
struct Volume {
    disk: KArc<RamDisk>,
    fs: KArc<ExfatFs>,
}

impl Volume {
    fn new(image: &[u8]) -> Volume {
        Volume::new_in(512, image)
    }

    /// The volume in `image` on a disk of logical blocks of `block` bytes.
    fn new_in(block: u32, image: &[u8]) -> Volume {
        Volume::mount_in(block, image, false).unwrap()
    }

    fn mount_in(block: u32, image: &[u8], read_only: bool) -> Result<Volume, Error> {
        radix_vfs::set_clock(clock);
        let sectors = image.len() as u64 / 512;
        let disk = mem::arc(RamDisk::with_blocks(sectors, false, block)).unwrap();
        disk.load(0, image).unwrap();
        let device: DeviceRef = disk.clone();
        let fs = ExfatFs::mount(device, read_only)?;
        Ok(Volume { disk, fs })
    }

    fn root(&self) -> NodeRef {
        self.fs.root()
    }

    /// The disk's bytes as they are.
    fn raw(&self) -> Vec<u8> {
        let mut image = vec![0u8; (self.disk.info().sectors * 512) as usize];
        self.disk.dump(0, &mut image);
        image
    }

    /// The disk's bytes, after a sync.
    fn image(&self) -> Vec<u8> {
        self.fs.sync().unwrap();
        self.raw()
    }

    /// The volume, synced and mounted again from its bytes.
    fn remount(self) -> Volume {
        let block = self.disk.info().logical_block;
        let image = self.image();
        drop(self);
        Volume::new_in(block, &image)
    }
}

/// The node at `path` (names separated by `/`) under `dir`.
fn walk(dir: &NodeRef, path: &str) -> Result<NodeRef, Error> {
    let mut node = dir.clone();
    for name in path.split('/').filter(|n| !n.is_empty()) {
        node = node.lookup(name)?;
    }
    Ok(node)
}

fn write_all(node: &NodeRef, offset: u64, data: &[u8]) {
    assert_eq!(node.write(offset, &Slice(data)).unwrap(), data.len());
}

fn read_all(node: &NodeRef) -> Vec<u8> {
    let size = node.stat().size as usize;
    let mut out = vec![0u8; size + 100];
    let n = node.read(0, &mut SliceMut(&mut out)).unwrap();
    assert_eq!(n, size);
    out.truncate(n);
    out
}

/// A file `name` in `dir` holding `data`.
fn make_file(dir: &NodeRef, name: &str, data: &[u8]) -> NodeRef {
    let node = dir.create(name, Kind::File).unwrap();
    if !data.is_empty() {
        write_all(&node, 0, data);
    }
    node
}

fn names(dir: &NodeRef) -> Vec<(String, Kind)> {
    let mut out = Vec::new();
    dir.list(0, &mut |_, name, kind| {
        out.push((String::from(name), kind));
        true
    })
    .unwrap();
    out.sort_by(|a, b| a.0.cmp(&b.0));
    out
}

/// Names of every length class: short, of exactly 15 units (a name
/// entry's), longer, non-ASCII, and astral (two units a character).
const NAMES: [&str; 9] = [
    "README.TXT",
    "notes.txt",
    "fifteen units!!",
    "A long file name with spaces.markdown",
    ".profile",
    "Ünïcödé ✓.txt",
    "x",
    "emoji \u{1f600}\u{1f680}.bin",
    "very_long_name_without_an_extension_that_goes_on_and_on_and_on",
];

/// File sizes around sector, page and cluster edges.
const SIZES: [usize; 9] = [0, 1, 511, 512, 4095, 4096, 4097, 33_000, 300_001];

/// The volume flags on the disk.
fn flags(image: &[u8]) -> u16 {
    u16::from_le_bytes([image[106], image[107]])
}

#[test]
fn volumes_mount() {
    for (mib, cluster) in VOLUMES {
        let volume = Volume::new(&mkfs(mib, cluster));
        let geo = volume.fs.geometry();
        assert_eq!(geo.cluster_size, cluster);
        assert_eq!(geo.sector_size, 512);
        let mut label = String::new();
        volume.fs.label(&mut label).unwrap();
        assert_eq!(label, "RADIXTEST");
        let info = volume.fs.info();
        assert_eq!(info.blocks, u64::from(geo.clusters));
        // The bitmap, the up-case table and the root take clusters.
        assert!(info.free < info.blocks && info.blocks - info.free <= 32, "{info:?}");
        assert!(names(&volume.root()).is_empty());
        assert_eq!(volume.fs.kind().name(), "exfat");
    }
}

#[test]
fn files_written_are_read_back_and_fsck_passes() {
    for (mib, cluster) in VOLUMES {
        let volume = Volume::new(&mkfs(mib, cluster));
        let root = volume.root();
        let dir = root.create("Documents and Settings", Kind::Directory).unwrap();
        let sub = dir.create("sub", Kind::Directory).unwrap();
        let mut expected = Vec::new();
        for (i, name) in NAMES.iter().enumerate() {
            let data = pattern(i as u32, SIZES[i % SIZES.len()]);
            let parent = [&root, &dir, &sub][i % 3];
            make_file(parent, name, &data);
            let path = ["", "Documents and Settings/", "Documents and Settings/sub/"][i % 3];
            expected.push((format!("{path}{name}"), data));
        }
        drop((root, dir, sub));
        let image = volume.image();
        fsck(&image);
        // Synced, the volume is clean again.
        assert_eq!(flags(&image) & boot::flags::DIRTY, 0);
        // Another implementation reads the same files.
        let seen = sleuthkit_files(&image);
        assert_eq!(seen.len(), expected.len(), "{:?}", seen.keys());
        for (path, data) in &expected {
            assert_eq!(seen.get(path), Some(data), "{path}, as The Sleuth Kit reads it");
        }
        let volume = volume.remount();
        let root = volume.root();
        for (path, data) in &expected {
            let node = walk(&root, path).unwrap();
            assert_eq!(&read_all(&node), data, "{path} in clusters of {cluster}");
            // Names are found whatever their case, by the up-case table.
            assert_eq!(read_all(&walk(&root, &path.to_uppercase()).unwrap()), *data);
            assert_eq!(read_all(&walk(&root, &path.to_lowercase()).unwrap()), *data);
        }
        let mut shown = names(&root);
        shown.retain(|(name, _)| name != "Documents and Settings");
        let mut want: Vec<_> =
            NAMES.iter().step_by(3).map(|n| (String::from(*n), Kind::File)).collect();
        want.sort_by(|a, b| a.0.cmp(&b.0));
        assert_eq!(shown, want);
    }
}

/// A volume of 2 MiB that `mkfs.exfat -b 4096 -c 512 -L FUSEMADE` made
/// and exfat-fuse 1.4.0 filled, mounted on Linux through a loop device:
/// the files [`what_exfat_fuse_wrote_reads_back`] checks, `a.bin` and
/// `b.bin` written in turn and synced so that each is in pieces, and
/// `gone.txt` written and removed. Its sectors that are not all zeros: a
/// count of sectors, then each such sector's number and bytes, the
/// numbers as little-endian `u32`s.
const FUSE_MADE: &[u8] = include_bytes!("fixtures/fuse-made.sectors");

/// [`FUSE_MADE`]'s volume.
fn fuse_made() -> Vec<u8> {
    let word = |at: &[u8]| u32::from_le_bytes(at[..4].try_into().unwrap()) as usize;
    let mut image = vec![0u8; word(FUSE_MADE) * 512];
    for record in FUSE_MADE[4..].as_chunks::<{ 4 + 512 }>().0 {
        let at = word(record) * 512;
        image[at..at + 512].copy_from_slice(&record[4..]);
    }
    image
}

#[test]
fn what_exfat_fuse_wrote_reads_back() {
    let image = fuse_made();
    fsck(&image);
    let volume = Volume::new(&image);
    let mut label = String::new();
    volume.fs.label(&mut label).unwrap();
    assert_eq!(label, "FUSEMADE");
    let root = volume.root();
    let long = format!("{}.txt", "n".repeat(200));
    let a: Vec<u8> = (0..4).flat_map(|r| pattern(10 + r, 700)).collect();
    let b: Vec<u8> = (0..4).flat_map(|r| pattern(20 + r, 600)).collect();
    let mut expected = vec![
        (String::from("hello.txt"), b"Written by exfat-fuse.\n".to_vec()),
        (String::from("Some Dir/Ünïcödé ✓.txt"), pattern(3, 1000)),
        (String::from("big.bin"), pattern(1, 20_001)),
        (long.clone(), b"long\n".to_vec()),
        (String::from("empty"), Vec::new()),
        (String::from("a.bin"), a.clone()),
        (String::from("b.bin"), b),
    ];
    for i in 0..40 {
        expected.push((format!("Some Dir/file {i:02}.txt"), format!("file {i}\n").into_bytes()));
    }
    for (path, data) in &expected {
        assert_eq!(&read_all(&walk(&root, path).unwrap()), data, "{path}");
    }
    assert_eq!(read_all(&walk(&root, "some dir/ÜNÏCÖDÉ ✓.TXT").unwrap()), pattern(3, 1000));
    assert!(names(&walk(&root, "empty dir").unwrap()).is_empty());
    assert_eq!(root.lookup("gone.txt").err(), Some(Error::NotFound));
    assert_eq!(names(&root).len(), 8);
    assert_eq!(names(&walk(&root, "Some Dir").unwrap()).len(), 41);
    // Changed by Radix, it is still whole, as Linux's tools read it.
    let file = walk(&root, "a.bin").unwrap();
    let more = pattern(30, 5000);
    write_all(&file, a.len() as u64, &more);
    let dir = walk(&root, "Some Dir").unwrap();
    dir.rename("file 00.txt", &*root, "moved out.txt").unwrap();
    root.remove("b.bin").unwrap();
    make_file(&dir, "new.txt", b"new\n");
    drop((file, dir, root));
    let image = volume.image();
    fsck(&image);
    let seen = sleuthkit_files(&image);
    let mut a = a;
    a.extend(&more);
    assert_eq!(seen.get("a.bin"), Some(&a));
    assert_eq!(seen.get("moved out.txt").map(Vec::as_slice), Some(&b"file 0\n"[..]));
    assert_eq!(seen.get("Some Dir/new.txt").map(Vec::as_slice), Some(&b"new\n"[..]));
    assert!(!seen.contains_key("b.bin") && !seen.contains_key("Some Dir/file 00.txt"));
    assert_eq!(seen.get(&long).map(Vec::as_slice), Some(&b"long\n"[..]));
}

#[test]
fn the_dirty_bit_is_set_before_a_change_and_cleared_by_a_sync() {
    let volume = Volume::new(&mkfs(16, 4096));
    assert_eq!(flags(&volume.raw()) & boot::flags::DIRTY, 0);
    let file = make_file(&volume.root(), "a.txt", b"hello");
    // The change is in memory; the disk says the volume is not clean.
    assert_ne!(flags(&volume.raw()) & boot::flags::DIRTY, 0);
    let image = volume.image();
    assert_eq!(flags(&image) & boot::flags::DIRTY, 0);
    fsck(&image);
    // A write that needs a cluster marks it again, as it takes one.
    let more = pattern(7, 5000);
    write_all(&file, 5, &more);
    assert_ne!(flags(&volume.raw()) & boot::flags::DIRTY, 0);
    drop(file);
    let volume = volume.remount();
    let mut want = b"hello".to_vec();
    want.extend(&more);
    assert_eq!(read_all(&walk(&volume.root(), "a.txt").unwrap()), want);
    // A volume left dirty mounts, and stays dirty until synced.
    let mut image = volume.image();
    image[106] |= boot::flags::DIRTY as u8;
    let volume = Volume::new(&image);
    assert_ne!(flags(&volume.raw()) & boot::flags::DIRTY, 0);
    assert_eq!(flags(&volume.image()) & boot::flags::DIRTY, 0);
}

#[test]
fn remove_rename_and_replace() {
    for (mib, cluster) in [VOLUMES[0], VOLUMES[2]] {
        let volume = Volume::new(&mkfs(mib, cluster));
        let root = volume.root();
        let a = root.create("dir a", Kind::Directory).unwrap();
        let b = root.create("dir b", Kind::Directory).unwrap();
        make_file(&a, "one.txt", b"one");
        make_file(&a, "two.txt", &pattern(2, 9000));
        make_file(&b, "three.txt", b"three");
        // Within a directory, to a longer name.
        a.rename("one.txt", &*a, "the first file, renamed to a much longer name.txt").unwrap();
        // Only the case.
        a.rename("two.txt", &*a, "TWO.TXT").unwrap();
        // Across directories, replacing a file.
        a.rename("TWO.TXT", &*b, "three.txt").unwrap();
        // A directory, into another.
        root.rename("dir a", &*b, "moved").unwrap();
        // Never into itself.
        let moved = walk(&root, "dir b/moved").unwrap();
        assert_eq!(b.rename("moved", &*moved, "inside"), Err(Error::BadName));
        assert_eq!(root.rename("dir b", &*moved, "inside"), Err(Error::BadName));
        // A non-empty directory is not removed, nor replaced.
        assert_eq!(root.remove("dir b"), Err(Error::NotEmpty));
        let c = root.create("c", Kind::Directory).unwrap();
        assert_eq!(root.rename("c", &*b, "moved"), Err(Error::NotEmpty));
        assert_eq!(root.rename("c", &*b, "three.txt"), Err(Error::NotDirectory));
        b.remove("moved/the first file, renamed to a much longer name.txt").unwrap_err();
        moved.remove("the first file, renamed to a much longer name.txt").unwrap();
        // An empty directory replaces an empty one.
        root.rename("c", &*b, "moved").unwrap();
        drop((a, b, c, moved));
        let image = volume.image();
        fsck(&image);
        let volume = volume.remount();
        let root = volume.root();
        assert_eq!(read_all(&walk(&root, "dir b/three.txt").unwrap()), pattern(2, 9000));
        assert_eq!(names(&walk(&root, "dir b").unwrap()).len(), 2);
        assert!(names(&walk(&root, "dir b/moved").unwrap()).is_empty());
        assert_eq!(names(&root), vec![(String::from("dir b"), Kind::Directory)]);
        // Every cluster but those of "dir b", "moved" and "three.txt" is
        // free again.
        let info = volume.fs.info();
        let fresh = Volume::new(&mkfs(mib, cluster)).fs.info();
        assert_eq!(info.free + 2 + 9000u64.div_ceil(u64::from(cluster)), fresh.free);
    }
}

#[test]
fn a_removed_file_lives_while_in_use() {
    let volume = Volume::new(&mkfs(16, 4096));
    let root = volume.root();
    let before = volume.fs.info().free;
    let file = make_file(&root, "doomed.bin", &pattern(5, 100_000));
    root.remove("doomed.bin").unwrap();
    assert_eq!(root.lookup("doomed.bin").err(), Some(Error::NotFound));
    assert_eq!(read_all(&file), pattern(5, 100_000));
    write_all(&file, 0, b"still writable");
    volume.fs.sync().unwrap();
    assert!(volume.fs.info().free < before);
    drop(file);
    assert_eq!(volume.fs.info().free, before);
    drop(root);
    fsck(&volume.image());
}

#[test]
fn growth_reads_zeros_and_writes_them_only_when_data_passes() {
    for (mib, cluster) in [VOLUMES[0], VOLUMES[1], VOLUMES[3]] {
        let volume = Volume::new(&mkfs(mib, cluster));
        let root = volume.root();
        let file = make_file(&root, "sparse.bin", &pattern(1, 1000));
        // Grown: the new bytes read as zeros, and the disk is not written
        // for them; exFAT says how much was written.
        file.set_size(200_000).unwrap();
        let mut want = pattern(1, 1000);
        want.resize(200_000, 0);
        assert_eq!(read_all(&file), want);
        drop(file);
        let image = volume.image();
        fsck(&image);
        // What was written runs to the end of the page that held the data,
        // whose bytes after it were zeros.
        let (_, stream) = set_of(&image, "sparse.bin").unwrap();
        assert_eq!(stream.size, 200_000);
        assert!((1000..=4096).contains(&stream.valid), "{}", stream.valid);
        // Stale bytes on the disk after the data must not show: the
        // clusters are filled with noise behind the driver's back.
        let mut image = image;
        let geo = geometry(&image);
        let first = geo.cluster_offset(stream.first) as usize;
        let span = 200_000usize.next_multiple_of(cluster as usize);
        let first_span = if stream.contiguous() { span } else { cluster as usize };
        let valid = stream.valid as usize;
        for (i, byte) in image[first + valid..first + first_span].iter_mut().enumerate() {
            *byte = (i % 251) as u8 | 1;
        }
        let volume = Volume::new(&image);
        let file = walk(&volume.root(), "sparse.bin").unwrap();
        assert_eq!(read_all(&file), want);
        // Data written far on: what lies between reads as zeros, then and
        // after a remount, even with noise on the disk.
        write_all(&file, 150_000, b"far on");
        want[150_000..150_006].copy_from_slice(b"far on");
        assert_eq!(read_all(&file), want);
        drop(file);
        let image = volume.image();
        fsck(&image);
        let (_, stream) = set_of(&image, "sparse.bin").unwrap();
        assert_eq!(stream.size, 200_000);
        assert!((150_006..=151_552).contains(&stream.valid), "{}", stream.valid);
        let volume = Volume::new(&image);
        let file = walk(&volume.root(), "sparse.bin").unwrap();
        assert_eq!(read_all(&file), want);
        // Cut: the clusters past the end are free again.
        let free = volume.fs.info().free;
        file.set_size(10).unwrap();
        assert_eq!(read_all(&file), &want[..10]);
        let freed = 200_000u64.div_ceil(u64::from(cluster)) - 1;
        assert_eq!(volume.fs.info().free, free + freed);
        drop(file);
        fsck(&volume.image());
    }
}

#[test]
fn files_in_pieces_get_the_table() {
    for (mib, cluster) in [VOLUMES[0], VOLUMES[1]] {
        let volume = Volume::new(&mkfs(mib, cluster));
        let root = volume.root();
        // Two files growing in turn take each other's next clusters, so
        // each is in pieces: the first piece moves to the table.
        let a = root.create("a.bin", Kind::File).unwrap();
        let b = root.create("b.bin", Kind::File).unwrap();
        let (mut want_a, mut want_b) = (Vec::new(), Vec::new());
        for round in 0..12u32 {
            let piece_a = pattern(round, cluster as usize * 2 + 7);
            let piece_b = pattern(round + 100, cluster as usize + 3);
            write_all(&a, want_a.len() as u64, &piece_a);
            write_all(&b, want_b.len() as u64, &piece_b);
            want_a.extend(piece_a);
            want_b.extend(piece_b);
            if round % 4 == 3 {
                // Written back now and then, so that the clusters are
                // taken as the data grows.
                volume.fs.sync().unwrap();
            }
        }
        // A directory in pieces too.
        let dir = root.create("many", Kind::Directory).unwrap();
        for i in 0..(cluster as usize / 32 * 3) / 3 + 5 {
            make_file(&dir, &format!("file number {i}"), b"x");
            if i % 7 == 0 {
                make_file(&root, &format!("spacer {i}"), &pattern(i as u32, cluster as usize));
            }
        }
        drop((a, b, dir));
        let image = volume.image();
        fsck(&image);
        let (_, stream) = set_of(&image, "a.bin").unwrap();
        assert!(!stream.contiguous(), "a.bin should be in pieces");
        let volume = volume.remount();
        let root = volume.root();
        assert_eq!(read_all(&walk(&root, "a.bin").unwrap()), want_a);
        assert_eq!(read_all(&walk(&root, "b.bin").unwrap()), want_b);
        let many = walk(&root, "many").unwrap();
        assert_eq!(names(&many).len(), (cluster as usize / 32 * 3) / 3 + 5);
        // Cut and removed, they free every cluster.
        walk(&root, "a.bin").unwrap().set_size(cluster as u64 + 1).unwrap();
        root.remove("b.bin").unwrap();
        drop(many);
        fsck(&volume.image());
    }
}

#[test]
fn full_volumes_and_directories() {
    let volume = Volume::new(&mkfs(8, 512));
    let root = volume.root();
    let file = root.create("filler", Kind::File).unwrap();
    let mut written = 0u64;
    // Big writes until one does not fit, then a cluster at a time.
    let mut chunk = Vec::new();
    for len in [64 * 1024, 512] {
        chunk = pattern(3, len);
        loop {
            match file.write(written, &Slice(&chunk)) {
                Ok(n) if n > 0 => written += n as u64,
                Ok(_) => break,
                Err(err) => {
                    assert_eq!(err, Error::NoSpace);
                    break;
                }
            }
        }
    }
    assert!(written > 5 << 20, "{written}");
    assert_eq!(volume.fs.info().free, 0);
    // No cluster for a new directory either.
    assert_eq!(root.create("dir", Kind::Directory).err(), Some(Error::NoSpace));
    // Data written so far is kept.
    drop(file);
    let image = volume.image();
    fsck(&image);
    let volume = Volume::new(&image);
    let file = walk(&volume.root(), "filler").unwrap();
    assert_eq!(file.stat().size, written);
    let mut buf = vec![0u8; chunk.len()];
    file.read(written - chunk.len() as u64, &mut SliceMut(&mut buf)).unwrap();
    assert_eq!(buf, chunk);
    // Cut, it gives its room back.
    file.set_size(0).unwrap();
    drop(file);
    let fresh = Volume::new(&mkfs(8, 512)).fs.info();
    assert_eq!(volume.fs.info().free, fresh.free);
    fsck(&volume.image());
}

#[test]
fn read_only_volumes_refuse_changes() {
    let mut volume = Volume::new(&mkfs(16, 4096));
    make_file(&volume.root(), "kept.txt", b"kept");
    let image = volume.image();
    volume = Volume::mount_in(512, &image, true).unwrap();
    let root = volume.root();
    assert_eq!(root.create("new", Kind::File).err(), Some(Error::ReadOnly));
    assert_eq!(root.remove("kept.txt"), Err(Error::ReadOnly));
    let kept = walk(&root, "kept.txt").unwrap();
    assert_eq!(kept.write(0, &Slice(b"x")), Err(Error::ReadOnly));
    assert_eq!(read_all(&kept), b"kept");
    assert!(volume.fs.read_only());
    drop((root, kept));
    assert_eq!(volume.image(), image);
}

#[test]
fn times_are_kept_as_exfat_keeps_them() {
    let volume = Volume::new(&mkfs(16, 4096));
    let root = volume.root();
    let file = make_file(&root, "timed.txt", b"t");
    // 2024-02-29 12:34:56.789 UTC: hundredths for the change, two
    // seconds for the read.
    let t = 1_709_210_096_789_000_000;
    file.set_times(Some(t), Some(t)).unwrap();
    let stat = file.stat();
    assert_eq!(stat.modified, 1_709_210_096_780_000_000);
    assert_eq!(stat.accessed, 1_709_210_096_000_000_000);
    assert_eq!(stat.created, clock());
    drop((root, file));
    let volume = volume.remount();
    let stat = walk(&volume.root(), "timed.txt").unwrap().stat();
    assert_eq!(stat.modified, 1_709_210_096_780_000_000);
    assert_eq!(stat.accessed, 1_709_210_096_000_000_000);
    assert_eq!(stat.created, clock());
    let (file, _) = set_of(&volume.image(), "timed.txt").unwrap();
    assert_eq!(file.modified.utc, 0x80);
}

#[test]
fn names_exfat_forbids_are_refused() {
    let volume = Volume::new(&mkfs(16, 4096));
    let root = volume.root();
    for name in ["a:b", "q?", "star*", "trailing.", "trailing ", "pipe|", "back\\slash"] {
        assert_eq!(root.create(name, Kind::File).err(), Some(Error::BadName), "{name}");
    }
    assert_eq!(root.create(&"n".repeat(256), Kind::File).err(), Some(Error::NameTooLong));
    make_file(&root, &"n".repeat(255), b"longest");
    make_file(&root, "Straße", b"1");
    // The same but for case, by the volume's table.
    assert_eq!(root.create("STRASSE", Kind::File).map(|_| ()), Ok(()));
    assert_eq!(root.create("straße", Kind::File).err(), Some(Error::Exists));
    assert_eq!(root.create("ÜBER", Kind::File).map(|_| ()), Ok(()));
    assert_eq!(root.create("über", Kind::File).err(), Some(Error::Exists));
    drop(root);
    fsck(&volume.image());
}

#[test]
fn damaged_volumes_are_refused() {
    let image = mkfs(16, 4096);
    let mount = |image: &[u8]| Volume::mount_in(512, image, false).map(|_| ());
    // Not exFAT at all.
    let mut other = image.clone();
    other[3..11].copy_from_slice(b"NTFS    ");
    assert_eq!(mount(&other), Err(Error::UnknownFilesystem));
    // A byte the checksum covers.
    let mut changed = image.clone();
    changed[100] ^= 1;
    assert_eq!(mount(&changed), Err(Error::Corrupt));
    // The flags the checksum leaves out.
    let mut flagged = image.clone();
    flagged[112] = 37;
    assert_eq!(mount(&flagged), Ok(()));
    // A later major revision.
    let mut later = image.clone();
    later[105] = 2;
    reseal(&mut later);
    assert_eq!(mount(&later), Err(Error::NotSupported));
    // A volume bigger than its disk.
    assert_eq!(mount(&image[..image.len() / 2]), Err(Error::Corrupt));
    // A disk of 4 KiB blocks cannot write its 512-byte sectors alone.
    assert_eq!(Volume::mount_in(4096, &image, false).err(), Some(Error::NotSupported));
}

#[test]
fn other_filesystems_are_told_apart() {
    // FAT and Btrfs take exFAT for none of theirs, and exFAT FAT for
    // none of its, so that Spark can try each in turn.
    let image = mkfs(16, 4096);
    let disk = |image: &[u8]| {
        let disk = mem::arc(RamDisk::new(image.len() as u64 / 512, false)).unwrap();
        disk.load(0, image).unwrap();
        let device: DeviceRef = disk;
        device
    };
    assert_eq!(radix_fat::FatFs::mount(disk(&image), false).err(), Some(Error::UnknownFilesystem));
    let path = scratch("fat");
    let out = Command::new(tool("mkfs.fat")).arg("-C").arg(&path).arg("16384").output().unwrap();
    assert!(out.status.success(), "mkfs.fat: {}", String::from_utf8_lossy(&out.stderr));
    let fat = std::fs::read(&path).unwrap();
    let _ = std::fs::remove_file(&path);
    assert_eq!(ExfatFs::mount(disk(&fat), false).err(), Some(Error::UnknownFilesystem));
    // What Radix reads but does not write mounts only read-only.
    let mut texfat = image.clone();
    texfat[110] = 2;
    reseal(&mut texfat);
    assert_eq!(ExfatFs::mount(disk(&texfat), false).err(), Some(Error::NotSupported));
    let mut failed = image.clone();
    failed[106] |= boot::flags::MEDIA_FAILURE as u8;
    assert_eq!(ExfatFs::mount(disk(&failed), false).err(), Some(Error::NotSupported));
    let volume = Volume::mount_in(512, &failed, true).unwrap();
    assert!(volume.fs.read_only());
}

/// Writes the main boot region's checksum sector again, after a change,
/// by the sector size the boot sector gives, or 512 bytes if it gives
/// none of exFAT's.
fn reseal(image: &mut [u8]) {
    let ss = if (9..=12).contains(&image[108]) { 1usize << image[108] } else { 512 };
    let sum = boot::checksum(0, 0, &image[..(REGION_SECTORS as usize - 1) * ss]);
    let at = (REGION_SECTORS as usize - 1) * ss;
    for word in image[at..at + ss].as_chunks_mut::<4>().0 {
        word.copy_from_slice(&sum.to_le_bytes());
    }
}

/// The geometry of `image`.
fn geometry(image: &[u8]) -> boot::Geometry {
    boot::Geometry::parse(image[..512].try_into().unwrap(), image.len() as u64).unwrap()
}

/// The file entry and stream extension of the file `name` in the root
/// directory of `image`, read here rather than by the driver.
fn set_of(image: &[u8], name: &str) -> Option<(File, Stream)> {
    let geo = geometry(image);
    let cs = geo.cluster_size as usize;
    // The root directory's clusters, by the table.
    let mut clusters = vec![geo.root_cluster];
    loop {
        let at = geo.fat_start as usize + 4 * *clusters.last().unwrap() as usize;
        let next = u32::from_le_bytes(image[at..at + 4].try_into().unwrap());
        if next == 0xffff_ffff {
            break;
        }
        clusters.push(next);
    }
    let bytes: Vec<u8> = clusters
        .iter()
        .flat_map(|&c| image[geo.cluster_offset(c) as usize..][..cs].iter().copied())
        .collect();
    let entries: Vec<[u8; ENTRY]> = bytes.as_chunks::<ENTRY>().0.to_vec();
    let wanted: Vec<u16> = name.encode_utf16().collect();
    for (i, e) in entries.iter().enumerate() {
        if e[0] != kind::FILE {
            continue;
        }
        let file = File::decode(e);
        let stream = Stream::decode(&entries[i + 1]);
        let units: Vec<u16> = entries[i + 2..i + 1 + usize::from(file.secondaries)]
            .iter()
            .flat_map(entry::name_units)
            .take(usize::from(stream.name_len))
            .collect();
        if units == wanted {
            return Some((file, stream));
        }
    }
    None
}

/// A volume of 4 KiB sectors of `mib` MiB, with clusters of `cluster`
/// bytes, made as the specification lays one out: the boot regions,
/// the table, then the bitmap, the up-case table (ASCII's letters only)
/// and the root directory in the heap.
fn format_4k(mib: u32, cluster: u32) -> Vec<u8> {
    let ss = 4096usize;
    let mut image = vec![0u8; (mib as usize) << 20];
    let sectors = image.len() / ss;
    let per_cluster = cluster as usize / ss;
    let fat_offset = 24usize;
    // Room for the table: the heap starts after it, on a cluster.
    let estimate = (sectors - fat_offset) / per_cluster;
    let fat_length = ((estimate + 2) * 4).div_ceil(ss);
    let heap = (fat_offset + fat_length).next_multiple_of(per_cluster);
    let clusters = (sectors - heap) / per_cluster;
    let b = &mut image[..512];
    b[0..3].copy_from_slice(&[0xeb, 0x76, 0x90]);
    b[3..11].copy_from_slice(b"EXFAT   ");
    b[72..80].copy_from_slice(&(sectors as u64).to_le_bytes());
    b[80..84].copy_from_slice(&(fat_offset as u32).to_le_bytes());
    b[84..88].copy_from_slice(&(fat_length as u32).to_le_bytes());
    b[88..92].copy_from_slice(&(heap as u32).to_le_bytes());
    b[92..96].copy_from_slice(&(clusters as u32).to_le_bytes());
    // Clusters 2, 3 and 4: the bitmap, the up-case table, the root.
    b[96..100].copy_from_slice(&4u32.to_le_bytes());
    b[100..104].copy_from_slice(&0x1234_5678u32.to_le_bytes());
    b[104..106].copy_from_slice(&0x0100u16.to_le_bytes());
    b[108] = 12;
    b[109] = per_cluster.trailing_zeros() as u8;
    b[110] = 1;
    b[111] = 0x80;
    b[112] = 0xff;
    b[510] = 0x55;
    b[511] = 0xaa;
    // The extended boot sectors end in their signature.
    for s in 1..9 {
        image[s * ss + ss - 4..(s + 1) * ss].copy_from_slice(&[0, 0, 0x55, 0xaa]);
    }
    reseal(&mut image);
    let backup = REGION_SECTORS as usize * ss;
    image.copy_within(0..backup, backup);
    // The table: media and reserved entries, then the three clusters.
    let fat = fat_offset * ss;
    for (i, value) in
        [0xffff_fff8u32, 0xffff_ffff, 0xffff_ffff, 0xffff_ffff, 0xffff_ffff].iter().enumerate()
    {
        image[fat + 4 * i..fat + 4 * i + 4].copy_from_slice(&value.to_le_bytes());
    }
    let offset = |c: usize| heap * ss + (c - 2) * cluster as usize;
    image[offset(2)] = 0b111;
    let mut table: Vec<u16> = vec![0xffff, 0x61];
    table.extend(0x41..=0x5a);
    table.extend([0xffff, 0xffff - 0x7b]);
    let table: Vec<u8> = table.iter().flat_map(|u| u.to_le_bytes()).collect();
    image[offset(3)..offset(3) + table.len()].copy_from_slice(&table);
    let sum = table.iter().fold(0u32, |s, &b| s.rotate_right(1).wrapping_add(u32::from(b)));
    let root = offset(4);
    let mut bitmap = [0u8; ENTRY];
    bitmap[0] = kind::BITMAP;
    bitmap[20..24].copy_from_slice(&2u32.to_le_bytes());
    bitmap[24..32].copy_from_slice(&(clusters.div_ceil(8) as u64).to_le_bytes());
    let mut upcase = [0u8; ENTRY];
    upcase[0] = kind::UPCASE;
    upcase[4..8].copy_from_slice(&sum.to_le_bytes());
    upcase[20..24].copy_from_slice(&3u32.to_le_bytes());
    upcase[24..32].copy_from_slice(&(table.len() as u64).to_le_bytes());
    let mut label = [0u8; ENTRY];
    label[0] = kind::LABEL;
    label[1] = 4;
    for (i, unit) in "FOUR".encode_utf16().enumerate() {
        label[2 + 2 * i..4 + 2 * i].copy_from_slice(&unit.to_le_bytes());
    }
    for (i, e) in [label, bitmap, upcase].iter().enumerate() {
        image[root + i * ENTRY..root + (i + 1) * ENTRY].copy_from_slice(e);
    }
    image
}

#[test]
fn volumes_of_4k_sectors_on_disks_of_4k_sectors() {
    for cluster in [4096, 65536] {
        let image = format_4k(32, cluster);
        fsck(&image);
        let volume = Volume::new_in(4096, &image);
        assert_eq!(volume.fs.geometry().sector_size, 4096);
        let mut label = String::new();
        volume.fs.label(&mut label).unwrap();
        assert_eq!(label, "FOUR");
        let root = volume.root();
        let dir = root.create("Sub directory", Kind::Directory).unwrap();
        let mut expected = Vec::new();
        for (i, &size) in SIZES.iter().enumerate() {
            let name = format!("file {i}.bin");
            let data = pattern(i as u32 + 40, size);
            make_file([&root, &dir][i % 2], &name, &data);
            let path = ["", "Sub directory/"][i % 2];
            expected.push((format!("{path}{name}"), data));
        }
        drop((root, dir));
        let image = volume.image();
        fsck(&image);
        let volume = volume.remount();
        for (path, data) in &expected {
            assert_eq!(&read_all(&walk(&volume.root(), path).unwrap()), data, "{path}");
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum Model {
    File(Vec<u8>),
    Dir,
}

#[derive(Clone, Debug)]
enum Op {
    Create { dir: usize, name: usize, kind: bool },
    Write { path: usize, offset: u32, len: u16, seed: u8 },
    Resize { path: usize, size: u32 },
    Remove { path: usize },
    Rename { from: usize, to_dir: usize, to: usize },
    Sync,
    Remount,
}

/// The names the model uses: two directories' worth, some long, some
/// differing only in case.
const MODEL_NAMES: [&str; 6] =
    ["alpha", "Beta.TXT", "a much longer name, past fifteen units.data", "ALPHA2", "d", "é é"];
const MODEL_DIRS: [&str; 3] = ["", "d", "é é"];

fn op() -> impl Strategy<Value = Op> {
    prop_oneof![
        3 => (0..3usize, 0..6usize, any::<bool>()).prop_map(|(dir, name, kind)| Op::Create { dir, name, kind }),
        4 => (0..18usize, 0..40_000u32, 0..9000u16, any::<u8>())
            .prop_map(|(path, offset, len, seed)| Op::Write { path, offset, len, seed }),
        1 => (0..18usize, 0..50_000u32).prop_map(|(path, size)| Op::Resize { path, size }),
        1 => (0..18usize).prop_map(|path| Op::Remove { path }),
        1 => (0..18usize, 0..3usize, 0..6usize).prop_map(|(from, to_dir, to)| Op::Rename { from, to_dir, to }),
        1 => Just(Op::Sync),
        1 => Just(Op::Remount),
    ]
}

fn join(dir: &str, name: &str) -> String {
    if dir.is_empty() { String::from(name) } else { format!("{dir}/{name}") }
}

/// Path number `n`: a name in one of the directories.
fn model_path(n: usize) -> (String, &'static str, &'static str) {
    let dir = MODEL_DIRS[n / 6];
    let name = MODEL_NAMES[n % 6];
    (join(dir, name), dir, name)
}

/// The volumes the model runs on, made once.
fn fresh(kind: usize) -> Vec<u8> {
    static IMAGES: std::sync::OnceLock<Vec<Vec<u8>>> = std::sync::OnceLock::new();
    IMAGES.get_or_init(|| VOLUMES.iter().map(|&(mib, cluster)| mkfs(mib, cluster)).collect())[kind]
        .clone()
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 24, ..ProptestConfig::default() })]

    #[test]
    fn the_driver_matches_a_model(kind in 0..4usize, ops in proptest::collection::vec(op(), 1..40)) {
        let mut volume = Volume::new(&fresh(kind));
        let mut model: BTreeMap<String, Model> = BTreeMap::new();
        for op in ops {
            let root = volume.root();
            match op {
                Op::Create { dir, name, kind } => {
                    let (dir, name) = (MODEL_DIRS[dir], MODEL_NAMES[name]);
                    let path = join(dir, name);
                    let parent_ok = dir.is_empty() || model.get(dir) == Some(&Model::Dir);
                    let kind = if kind { Kind::Directory } else { Kind::File };
                    let got = walk(&root, dir).and_then(|d| d.create(name, kind).map(|_| ()));
                    if !parent_ok {
                        prop_assert!(got.is_err());
                    } else {
                        match model.entry(path) {
                            Entry::Occupied(_) => prop_assert_eq!(got, Err(Error::Exists)),
                            Entry::Vacant(slot) => {
                                prop_assert_eq!(got, Ok(()));
                                slot.insert(if kind == Kind::Directory { Model::Dir } else { Model::File(Vec::new()) });
                            }
                        }
                    }
                }
                Op::Write { path, offset, len, seed } => {
                    let (path, _, _) = model_path(path);
                    let data = pattern(u32::from(seed), usize::from(len));
                    let got = walk(&root, &path).and_then(|node| node.write(u64::from(offset), &Slice(&data)));
                    match model.get_mut(&path) {
                        Some(Model::File(bytes)) => {
                            prop_assert_eq!(got, Ok(data.len()));
                            let (offset, end) = (offset as usize, offset as usize + data.len());
                            if !data.is_empty() {
                                if bytes.len() < end {
                                    bytes.resize(end, 0);
                                }
                                bytes[offset..end].copy_from_slice(&data);
                            }
                        }
                        Some(Model::Dir) => prop_assert_eq!(got, Err(Error::IsDirectory)),
                        None => prop_assert!(got.is_err()),
                    }
                }
                Op::Resize { path, size } => {
                    let (path, _, _) = model_path(path);
                    let got = walk(&root, &path).and_then(|node| node.set_size(u64::from(size)));
                    match model.get_mut(&path) {
                        Some(Model::File(bytes)) => {
                            prop_assert_eq!(got, Ok(()));
                            bytes.resize(size as usize, 0);
                        }
                        Some(Model::Dir) => prop_assert_eq!(got, Err(Error::IsDirectory)),
                        None => prop_assert!(got.is_err()),
                    }
                }
                Op::Remove { path } => {
                    let (path, dir, name) = model_path(path);
                    let got = walk(&root, dir).and_then(|d| d.remove(name));
                    let has_children = model.keys().any(|k| k.starts_with(&format!("{path}/")));
                    match model.get(&path) {
                        Some(Model::Dir) if has_children => prop_assert_eq!(got, Err(Error::NotEmpty)),
                        Some(_) => {
                            prop_assert_eq!(got, Ok(()));
                            model.remove(&path);
                        }
                        None => prop_assert!(got.is_err()),
                    }
                }
                Op::Rename { from, to_dir, to } => {
                    let (from_path, from_dir, from_name) = model_path(from);
                    let (to_dir, to_name) = (MODEL_DIRS[to_dir], MODEL_NAMES[to]);
                    let to_path = join(to_dir, to_name);
                    let got = walk(&root, from_dir).and_then(|source| {
                        let target = walk(&root, to_dir)?;
                        source.rename(from_name, &*target, to_name)
                    });
                    let source = model.get(&from_path).cloned();
                    let target_dir_ok = to_dir.is_empty() || model.get(to_dir) == Some(&Model::Dir);
                    let into_itself = to_path == from_path
                        || to_path.starts_with(&format!("{from_path}/"))
                        || to_dir == from_path;
                    match source {
                        None => prop_assert!(got.is_err()),
                        Some(_) if !target_dir_ok => prop_assert!(got.is_err()),
                        Some(_) if to_path == from_path => prop_assert_eq!(got, Ok(())),
                        Some(Model::Dir) if into_itself => prop_assert_eq!(got, Err(Error::BadName)),
                        Some(moving) => {
                            let replaced = model.get(&to_path).cloned();
                            let target_full = model.keys().any(|k| k.starts_with(&format!("{to_path}/")));
                            match (&moving, replaced) {
                                (_, Some(Model::Dir)) if target_full => prop_assert_eq!(got, Err(Error::NotEmpty)),
                                (Model::Dir, Some(Model::File(_))) => prop_assert_eq!(got, Err(Error::NotDirectory)),
                                (Model::File(_), Some(Model::Dir)) => prop_assert_eq!(got, Err(Error::IsDirectory)),
                                _ => {
                                    prop_assert_eq!(got, Ok(()));
                                    let moved: Vec<(String, Model)> = model
                                        .iter()
                                        .filter(|(k, _)| *k == &from_path || k.starts_with(&format!("{from_path}/")))
                                        .map(|(k, v)| (format!("{to_path}{}", &k[from_path.len()..]), v.clone()))
                                        .collect();
                                    model.retain(|k, _| k != &from_path && !k.starts_with(&format!("{from_path}/")));
                                    model.remove(&to_path);
                                    model.extend(moved);
                                }
                            }
                        }
                    }
                }
                Op::Sync => prop_assert_eq!(volume.fs.sync(), Ok(())),
                Op::Remount => {
                    drop(root);
                    volume = volume.remount();
                }
            }
        }
        // The tree matches the model, before and after a remount.
        for _ in 0..2 {
            let root = volume.root();
            for (path, entry) in &model {
                let node = walk(&root, path);
                prop_assert!(node.is_ok(), "{path} is missing");
                let node = node.unwrap();
                match entry {
                    Model::File(bytes) => prop_assert_eq!(&read_all(&node), bytes, "{}", path),
                    Model::Dir => prop_assert_eq!(node.stat().kind, Kind::Directory),
                }
            }
            for dir in MODEL_DIRS {
                if !dir.is_empty() && model.get(dir) != Some(&Model::Dir) {
                    continue;
                }
                let want = model.keys().filter(|k| {
                    let parent = k.rsplit_once('/').map_or("", |(p, _)| p);
                    parent == dir
                }).count();
                prop_assert_eq!(names(&walk(&root, dir).unwrap()).len(), want);
            }
            drop(root);
            volume = volume.remount();
        }
        fsck(&volume.image());
    }
}

/// A volume of each kind holding files and directories, made once.
fn populated() -> &'static [Vec<u8>] {
    static IMAGES: std::sync::OnceLock<Vec<Vec<u8>>> = std::sync::OnceLock::new();
    IMAGES.get_or_init(|| {
        [VOLUMES[0], VOLUMES[1]]
            .into_iter()
            .map(|(mib, cluster)| {
                let volume = Volume::new(&mkfs(mib, cluster));
                let root = volume.root();
                let dir = root.create("A long directory name", Kind::Directory).unwrap();
                let inner = dir.create("inner", Kind::Directory).unwrap();
                for (i, (name, size)) in NAMES.iter().zip(SIZES).enumerate() {
                    let parent = [&root, &dir, &inner][i % 3];
                    make_file(parent, name, &pattern(u32::try_from(i).unwrap(), size));
                }
                drop((root, dir, inner));
                volume.image()
            })
            .collect()
    })
}

/// Everything under `node`, read: at most `budget` nodes, and files only
/// up to 256 KiB, as damage can make them look huge.
fn read_everything(node: &NodeRef, depth: usize, budget: &mut usize) {
    if *budget == 0 || depth > 8 {
        return;
    }
    *budget -= 1;
    let _ = node.stat();
    let mut buffer = vec![0u8; 64 * 1024];
    let mut offset = 0;
    while offset < 256 * 1024 {
        match node.read(offset, &mut SliceMut(&mut buffer)) {
            Ok(n) if n > 0 => offset += n as u64,
            _ => break,
        }
    }
    let mut found = Vec::new();
    let _ = node.list(0, &mut |_, name, _| {
        found.push(String::from(name));
        found.len() < 64
    });
    for name in &found {
        if let Ok(child) = node.lookup(name) {
            read_everything(&child, depth + 1, budget);
        }
    }
}

/// The names in `dir`, or none if it cannot be listed.
fn names_or_none(dir: &NodeRef) -> Vec<String> {
    let mut out = Vec::new();
    let _ = dir.list(0, &mut |_, name, _| {
        out.push(String::from(name));
        out.len() < 64
    });
    out
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 64, ..ProptestConfig::default() })]

    /// A volume with any of its metadata bytes changed is refused, or
    /// works: reads, lists, writes, renames, removes and syncs give
    /// errors, never a panic or a hang. Half the changes go to the boot
    /// region, sealed again so that its checksum passes.
    #[test]
    fn damaged_volumes_never_panic(
        which in 0..2usize,
        changes in proptest::collection::vec((any::<bool>(), any::<u32>(), any::<u8>()), 1..24),
    ) {
        let mut image = populated()[which].clone();
        let geo = geometry(&image);
        let heap = geo.heap_start as usize;
        let mut boot_changed = false;
        for (boot, at, value) in changes {
            if boot {
                image[64 + at as usize % (512 - 64)] = value;
                boot_changed = true;
            } else {
                // The table, the bitmap, the up-case table, directories and
                // the first data clusters.
                let from = geo.fat_start as usize;
                let span = (heap + 64 * geo.cluster_size as usize).min(image.len()) - from;
                image[from + at as usize % span] = value;
            }
        }
        if boot_changed {
            reseal(&mut image);
        }
        let disk = mem::arc(RamDisk::new(image.len() as u64 / 512, false)).unwrap();
        disk.load(0, &image).unwrap();
        let device: DeviceRef = disk.clone();
        if let Ok(fs) = ExfatFs::mount(device, false) {
            let root = fs.root();
            read_everything(&root, 0, &mut 200);
            if let Ok(file) = root.create("new file.txt", Kind::File) {
                let _ = file.write(0, &Slice(&pattern(99, 5000)));
                let _ = file.set_size(100_000);
            }
            let found = names_or_none(&root);
            if let Some(name) = found.first() {
                let _ = root.rename(name, &*root, "renamed");
            }
            for name in found.iter().skip(1).take(3) {
                let _ = root.remove(name);
            }
            let _ = fs.sync();
        }
    }
}
