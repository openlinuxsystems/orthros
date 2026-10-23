// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Host tests of the FAT driver, against the reference tools: volumes are
//! made by `mkfs.fat`, what Radix writes is checked by `fsck.fat` and read
//! back by mtools, and what mtools writes is read by Radix (dosfstools
//! and mtools, `apt install dosfstools mtools`; `ob doctor` checks for
//! them).

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

use crate::FatFs;
use crate::table::Width;

/// 2026-10-22 00:42:22 UTC, for files' times.
fn clock() -> u64 {
    1_792_629_742_000_000_000
}

/// A tool of dosfstools or mtools.
fn tool(name: &str) -> PathBuf {
    for dir in ["/usr/sbin", "/sbin", "/usr/bin", "/bin", "/usr/local/bin", "/usr/local/sbin"] {
        let path = PathBuf::from(dir).join(name);
        if path.exists() {
            return path;
        }
    }
    panic!("{name} is missing: install dosfstools and mtools (apt install dosfstools mtools)");
}

/// A file name for a scratch image.
fn scratch(what: &str) -> PathBuf {
    static NEXT: AtomicU32 = AtomicU32::new(0);
    let n = NEXT.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!("radix-fat-{}-{what}-{n}.img", std::process::id()))
}

/// The kinds of volume the tests use: (FAT bits, KiB, sectors per
/// cluster), covering each width with clusters smaller and larger than a
/// page.
const VOLUMES: [(u8, u32, u32); 5] =
    [(12, 1440, 1), (12, 8192, 8), (16, 32768, 4), (16, 81920, 32), (32, 40960, 1)];

/// A fresh volume from `mkfs.fat`.
fn mkfs(bits: u8, kib: u32, per_cluster: u32) -> Vec<u8> {
    let path = scratch("mkfs");
    let out = Command::new(tool("mkfs.fat"))
        .args(["-C", "-F", &bits.to_string(), "-s", &per_cluster.to_string(), "-n", "RADIXTEST"])
        .arg(&path)
        .arg(kib.to_string())
        .output()
        .unwrap();
    assert!(out.status.success(), "mkfs.fat: {}", String::from_utf8_lossy(&out.stderr));
    let image = std::fs::read(&path).unwrap();
    let _ = std::fs::remove_file(&path);
    image
}

/// What `fsck.fat` says of `image`, and whether it found it clean.
fn fsck_report(image: &[u8]) -> (bool, String) {
    let path = scratch("fsck");
    std::fs::write(&path, image).unwrap();
    let out = Command::new(tool("fsck.fat")).arg("-n").arg(&path).output().unwrap();
    let _ = std::fs::remove_file(&path);
    let text =
        format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
    (out.status.success(), text)
}

/// Checks `image` with `fsck.fat`, which must find nothing wrong.
fn fsck(image: &[u8]) {
    let (clean, text) = fsck_report(image);
    assert!(clean, "fsck.fat found problems:\n{text}");
}

/// Runs an mtools command on `image`; returns what it printed.
fn mtools(image: &mut Vec<u8>, command: &str, args: &[&str]) -> Vec<u8> {
    let path = scratch("mtools");
    std::fs::write(&path, &*image).unwrap();
    let out = Command::new(tool(command))
        .env("MTOOLS_SKIP_CHECK", "1")
        .env("LC_ALL", "C.UTF-8")
        .arg("-i")
        .arg(&path)
        .args(args)
        .output()
        .unwrap();
    *image = std::fs::read(&path).unwrap();
    let _ = std::fs::remove_file(&path);
    assert!(out.status.success(), "{command} {args:?}: {}", String::from_utf8_lossy(&out.stderr));
    out.stdout
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
    fs: KArc<FatFs>,
}

impl Volume {
    fn new(image: &[u8]) -> Volume {
        radix_vfs::set_clock(clock);
        let disk = mem::arc(RamDisk::new(image.len() as u64 / 512, false)).unwrap();
        disk.load(0, image).unwrap();
        let device: DeviceRef = disk.clone();
        let fs = FatFs::mount(device, false).unwrap();
        Volume { disk, fs }
    }

    fn root(&self) -> NodeRef {
        self.fs.root()
    }

    /// The disk's bytes, after a sync.
    fn image(&self) -> Vec<u8> {
        self.fs.sync().unwrap();
        let mut image = vec![0u8; (self.disk.info().sectors * 512) as usize];
        self.disk.dump(0, &mut image);
        image
    }

    /// The volume, synced and mounted again from its bytes.
    fn remount(self) -> Volume {
        let image = self.image();
        drop(self);
        Volume::new(&image)
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

/// Names that exercise the short and long forms.
const NAMES: [&str; 9] = [
    "README.TXT",
    "notes.txt",
    "Makefile",
    "A long file name with spaces.markdown",
    ".profile",
    "Ünïcödé ✓.txt",
    "x",
    "archive.tar.gz",
    "very_long_name_without_an_extension_that_goes_on",
];

/// File sizes around sector, page and cluster edges.
const SIZES: [usize; 9] = [0, 1, 511, 512, 4095, 4096, 4097, 33_000, 140_001];

#[test]
fn each_width_mounts() {
    for (bits, kib, per_cluster) in VOLUMES {
        let volume = Volume::new(&mkfs(bits, kib, per_cluster));
        let geo = volume.fs.geometry();
        let width = match bits {
            12 => Width::Fat12,
            16 => Width::Fat16,
            _ => Width::Fat32,
        };
        assert_eq!(geo.width, width);
        assert_eq!(geo.cluster_size, 512 * per_cluster);
        let mut label = String::new();
        volume.fs.label(&mut label).unwrap();
        assert_eq!(label, "RADIXTEST");
        let info = volume.fs.info();
        assert_eq!(info.blocks, u64::from(geo.clusters));
        // FAT32's root directory takes a cluster.
        assert_eq!(info.free, info.blocks - u64::from(bits == 32));
        assert!(names(&volume.root()).is_empty());
    }
}

#[test]
fn files_written_are_read_back_by_radix_fsck_and_mtools() {
    for (bits, kib, per_cluster) in VOLUMES {
        let volume = Volume::new(&mkfs(bits, kib, per_cluster));
        let root = volume.root();
        let dir = root.create("Documents and Settings", Kind::Directory).unwrap();
        let sub = dir.create("sub", Kind::Directory).unwrap();
        let mut expected = Vec::new();
        for (i, name) in NAMES.iter().enumerate() {
            let size = SIZES[i % SIZES.len()];
            // A big file only where there is room for it.
            let size = if kib < 2000 { size.min(40_000) } else { size };
            let data = pattern(i as u32, size);
            let parent = [&root, &dir, &sub][i % 3];
            make_file(parent, name, &data);
            let path = ["", "Documents and Settings/", "Documents and Settings/sub/"][i % 3];
            expected.push((format!("{path}{name}"), data));
        }
        let mut image = volume.image();
        fsck(&image);
        for (path, data) in &expected {
            let shown = mtools(&mut image, "mtype", &[&format!("::/{path}")]);
            assert_eq!(&shown, data, "mtype {path} on FAT{bits}");
        }
        let listing = String::from_utf8(mtools(&mut image, "mdir", &["-/", "-b", "::/"])).unwrap();
        for (path, _) in &expected {
            assert!(
                listing.lines().any(|line| line == format!("::/{path}")),
                "{path} not in\n{listing}"
            );
        }
        drop((root, dir, sub));
        let volume = volume.remount();
        let root = volume.root();
        for (path, data) in &expected {
            let node = walk(&root, path).unwrap();
            assert_eq!(&read_all(&node), data, "{path} on FAT{bits}");
            // Names are found whatever their case.
            let upper: String = path.to_uppercase();
            assert_eq!(read_all(&walk(&root, &upper).unwrap()), *data);
        }
        let mut shown = names(&root);
        shown.retain(|(name, _)| name != "Documents and Settings");
        let mut want: Vec<_> =
            NAMES.iter().step_by(3).map(|n| (String::from(*n), Kind::File)).collect();
        want.sort_by(|a, b| a.0.cmp(&b.0));
        assert_eq!(shown, want);
    }
}

#[test]
fn files_mtools_writes_are_read() {
    for (bits, kib, per_cluster) in [VOLUMES[0], VOLUMES[2], VOLUMES[4]] {
        let mut image = mkfs(bits, kib, per_cluster);
        let source = scratch("source");
        let data = pattern(7, 20_000);
        std::fs::write(&source, &data).unwrap();
        let source = source.to_str().unwrap();
        mtools(&mut image, "mmd", &["::/Program Files"]);
        mtools(&mut image, "mcopy", &[source, "::/Program Files/Some Long Name.dat"]);
        mtools(&mut image, "mcopy", &[source, "::/SHORT.DAT"]);
        mtools(&mut image, "mcopy", &[source, "::/lower.dat"]);
        let _ = std::fs::remove_file(source);
        let volume = Volume::new(&image);
        let root = volume.root();
        for path in ["Program Files/Some Long Name.dat", "SHORT.DAT", "lower.dat"] {
            assert_eq!(read_all(&walk(&root, path).unwrap()), data, "{path}");
        }
        let shown = names(&root);
        assert!(shown.contains(&(String::from("Program Files"), Kind::Directory)), "{shown:?}");
        // mtools keeps case in long names or case bits.
        assert!(shown.iter().any(|(n, _)| n == "lower.dat"), "{shown:?}");
        // Radix's changes to mtools' files are clean too.
        let file = walk(&root, "Program Files/Some Long Name.dat").unwrap();
        write_all(&file, 19_990, &pattern(8, 5000));
        file.set_size(10_000).unwrap();
        drop(file);
        walk(&root, "Program Files").unwrap().remove("Some Long Name.dat").unwrap();
        root.remove("Program Files").unwrap();
        fsck(&volume.image());
    }
}

#[test]
fn remove_rename_and_replace() {
    for (bits, kib, per_cluster) in VOLUMES {
        let volume = Volume::new(&mkfs(bits, kib, per_cluster));
        let root = volume.root();
        let a = root.create("a", Kind::Directory).unwrap();
        let b = a.create("b", Kind::Directory).unwrap();
        make_file(&root, "one.txt", &pattern(1, 3000));
        make_file(&root, "two.txt", &pattern(2, 9000));
        make_file(&b, "deep file.txt", &pattern(3, 100));
        // A case change in place.
        root.rename("one.txt", &*root, "ONE.txt").unwrap();
        assert!(names(&root).contains(&(String::from("ONE.txt"), Kind::File)));
        // Into a directory, under a long name.
        root.rename("ONE.txt", &*a, "the first file.txt").unwrap();
        assert_eq!(read_all(&walk(&root, "a/the first file.txt").unwrap()), pattern(1, 3000));
        // Replacing a file.
        root.rename("two.txt", &*a, "the first file.txt").unwrap();
        assert_eq!(read_all(&walk(&root, "a/the first file.txt").unwrap()), pattern(2, 9000));
        assert_eq!(root.lookup("two.txt").err(), Some(Error::NotFound));
        // A directory under itself, and over a non-empty one.
        assert_eq!(root.rename("a", &*b, "a").err(), Some(Error::BadName));
        assert_eq!(root.rename("a", &*a, "x").err(), Some(Error::BadName));
        root.create("full", Kind::Directory).unwrap().create("f", Kind::File).unwrap();
        assert_eq!(a.rename("b", &*root, "full").err(), Some(Error::NotEmpty));
        assert_eq!(root.rename("full", &*a, "the first file.txt").err(), Some(Error::NotDirectory));
        // A directory moves up: its `..` follows (fsck checks it).
        a.rename("b", &*root, "b moved").unwrap();
        assert_eq!(read_all(&walk(&root, "b moved/deep file.txt").unwrap()), pattern(3, 100));
        assert_eq!(root.remove("a").err(), Some(Error::NotEmpty));
        a.remove("the first file.txt").unwrap();
        root.remove("a").unwrap();
        assert_eq!(root.create("full", Kind::File).err(), Some(Error::Exists));
        assert_eq!(root.create("FULL", Kind::Directory).err(), Some(Error::Exists));
        assert_eq!(root.create("bad:name", Kind::File).err(), Some(Error::BadName));
        assert_eq!(root.create("dot.", Kind::File).err(), Some(Error::BadName));
        drop((a, b));
        fsck(&volume.image());
        let volume = volume.remount();
        let mut shown = names(&volume.root());
        shown.sort_by(|a, b| a.0.cmp(&b.0));
        assert_eq!(
            shown,
            vec![
                (String::from("b moved"), Kind::Directory),
                (String::from("full"), Kind::Directory)
            ]
        );
    }
}

#[test]
fn a_removed_file_lives_while_in_use() {
    let volume = Volume::new(&mkfs(16, 32768, 4));
    let root = volume.root();
    let free = volume.fs.info().free;
    let file = make_file(&root, "open.bin", &pattern(5, 50_000));
    root.remove("open.bin").unwrap();
    assert_eq!(root.lookup("open.bin").err(), Some(Error::NotFound));
    // Still readable and writable through the handle.
    assert_eq!(read_all(&file), pattern(5, 50_000));
    write_all(&file, 50_000, &pattern(6, 10_000));
    volume.fs.sync().unwrap();
    assert!(volume.fs.info().free < free);
    drop(file);
    assert_eq!(volume.fs.info().free, free);
    fsck(&volume.image());
}

#[test]
fn growth_reads_zeros_and_cuts_free_clusters() {
    for (bits, kib, per_cluster) in VOLUMES {
        let volume = Volume::new(&mkfs(bits, kib, per_cluster));
        let root = volume.root();
        // Fill the volume's first clusters with junk, then free them, so
        // that new files get clusters that do not read as zeros.
        let junk = make_file(&root, "junk", &[0xee; 64 * 1024]);
        drop(junk);
        root.remove("junk").unwrap();
        let file = root.create("sparse", Kind::File).unwrap();
        write_all(&file, 0, b"start");
        write_all(&file, 20_000, b"end");
        file.set_size(30_000).unwrap();
        let mut want = vec![0u8; 30_000];
        want[..5].copy_from_slice(b"start");
        want[20_000..20_003].copy_from_slice(b"end");
        assert_eq!(read_all(&file), want);
        file.set_size(3).unwrap();
        file.set_size(9000).unwrap();
        let mut want = vec![0u8; 9000];
        want[..3].copy_from_slice(b"sta");
        assert_eq!(read_all(&file), want);
        drop(file);
        let volume = volume.remount();
        assert_eq!(read_all(&walk(&volume.root(), "sparse").unwrap()), want);
        walk(&volume.root(), "sparse").unwrap().set_size(0).unwrap();
        let mut image = volume.image();
        fsck(&image);
        assert!(mtools(&mut image, "mtype", &["::/sparse"]).is_empty());
    }
}

#[test]
fn full_volumes_and_directories() {
    // A floppy fills up; what was written is still whole.
    let volume = Volume::new(&mkfs(12, 1440, 1));
    let root = volume.root();
    let big = root.create("big", Kind::File).unwrap();
    let mut written = 0u64;
    // Big writes until one does not fit, then a cluster at a time.
    for len in [64 * 1024, 512] {
        let chunk = pattern(9, len);
        loop {
            match big.write(written, &Slice(&chunk)) {
                Ok(n) => written += n as u64,
                Err(err) => {
                    assert_eq!(err, Error::NoSpace);
                    break;
                }
            }
        }
    }
    assert_eq!(volume.fs.info().free, 0);
    assert_eq!(root.create("more", Kind::Directory).err(), Some(Error::NoSpace));
    big.set_size(1000).unwrap();
    drop(big);
    fsck(&volume.image());
    // The fixed root holds 224 entries; a directory grows past a cluster.
    let volume = Volume::new(&mkfs(12, 1440, 1));
    let root = volume.root();
    let mut made = 0;
    let full = loop {
        match root.create(&format!("f{made}"), Kind::File) {
            Ok(_) => made += 1,
            Err(err) => break err,
        }
    };
    assert_eq!(full, Error::NoSpace);
    // 224 entries, one of them the volume label.
    assert_eq!(made, 223);
    root.remove("f0").unwrap();
    let dir = root.create("dir", Kind::Directory).unwrap();
    for i in 0..300 {
        dir.create(&format!("A rather long file name, number {i}"), Kind::File).unwrap();
    }
    assert_eq!(names(&dir).len(), 300);
    // Their short names all differ (fsck checks), past the numeric tails.
    for i in (0..300).step_by(2) {
        dir.remove(&format!("A rather long file name, number {i}")).unwrap();
    }
    drop(dir);
    let mut image = volume.image();
    fsck(&image);
    let listing = String::from_utf8(mtools(&mut image, "mdir", &["-/", "-b", "::/dir"])).unwrap();
    assert_eq!(listing.lines().count(), 150, "{listing}");
}

#[test]
fn read_only_volumes_refuse_changes() {
    let mut image = mkfs(16, 32768, 4);
    let source = scratch("ro");
    std::fs::write(&source, b"hello").unwrap();
    mtools(&mut image, "mcopy", &[source.to_str().unwrap(), "::/hello.txt"]);
    let _ = std::fs::remove_file(&source);
    let disk = mem::arc(RamDisk::new(image.len() as u64 / 512, true)).unwrap();
    disk.load(0, &image).unwrap();
    let device: DeviceRef = disk.clone();
    let fs = FatFs::mount(device, false).unwrap();
    assert!(fs.read_only());
    let root = fs.root();
    let file = root.lookup("hello.txt").unwrap();
    assert_eq!(read_all(&file), b"hello");
    assert_eq!(file.write(0, &Slice(b"x")).err(), Some(Error::ReadOnly));
    assert_eq!(root.create("new", Kind::File).err(), Some(Error::ReadOnly));
    assert_eq!(root.remove("hello.txt").err(), Some(Error::ReadOnly));
    assert_eq!(file.set_times(Some(0), None).err(), Some(Error::ReadOnly));
    fs.sync().unwrap();
}

#[test]
fn times_set_are_kept_as_fat_keeps_them() {
    const S: u64 = 1_000_000_000;
    // 2026-10-22 12:34:57.5 UTC, and 2001-02-03 04:05:06 UTC.
    let (modified, accessed) = (1_792_672_497 * S + S / 2, 981_173_106 * S);
    let volume = Volume::new(&mkfs(16, 32768, 4));
    let root = volume.root();
    let file = make_file(&root, "file", b"data");
    let dir = root.create("dir", Kind::Directory).unwrap();
    let made = file.stat();
    assert_eq!(made.accessed, made.modified - made.modified % (86_400 * S));
    file.set_times(Some(accessed), Some(modified)).unwrap();
    dir.set_times(None, Some(modified)).unwrap();
    // A change time in two-second steps, a read's day; what stat says now
    // is what the disk will say.
    let stat = file.stat();
    assert_eq!(stat.modified, 1_792_672_496 * S);
    assert_eq!(stat.accessed, 981_158_400 * S);
    // Only what was asked for changes.
    file.set_times(None, None).unwrap();
    assert_eq!(file.stat(), stat);
    drop((file, dir, root));
    let volume = volume.remount();
    let root = volume.root();
    let file = walk(&root, "file").unwrap();
    assert_eq!(file.stat().modified, 1_792_672_496 * S);
    assert_eq!(file.stat().accessed, 981_158_400 * S);
    assert_eq!(walk(&root, "dir").unwrap().stat().modified, 1_792_672_496 * S);
    // Times FAT cannot hold are clamped to its years.
    file.set_times(Some(0), Some(0)).unwrap();
    assert_eq!(file.stat().modified, 315_532_800 * S);
    drop((file, root));
    fsck(&volume.image());
}

#[test]
fn damaged_volumes_are_refused() {
    let image = mkfs(16, 32768, 4);
    // Not FAT at all.
    let blank = vec![0u8; 1 << 20];
    let disk = mem::arc(RamDisk::new(blank.len() as u64 / 512, false)).unwrap();
    let device: DeviceRef = disk.clone();
    assert_eq!(FatFs::mount(device, false).err(), Some(Error::UnknownFilesystem));
    // Bigger than its disk.
    let disk = mem::arc(RamDisk::new(1024, false)).unwrap();
    disk.load(0, &image[..1024 * 512]).unwrap();
    let device: DeviceRef = disk.clone();
    assert_eq!(FatFs::mount(device, false).err(), Some(Error::Corrupt));
    // A chain that loops: reading the file fails, nothing hangs.
    let volume = Volume::new(&image);
    make_file(&volume.root(), "loop", &pattern(1, 20_000));
    let mut image = volume.image();
    drop(volume);
    let (geo, first) = {
        let volume = Volume::new(&image);
        let node = volume.root().lookup("loop").unwrap();
        let first = radix_vfs::downcast::<crate::FatNode>(&*node).unwrap().meta.lock().first;
        (*volume.fs.geometry(), first)
    };
    let at = (geo.fat_start + crate::table::offset(geo.width, first + 1)) as usize;
    image[at..at + 2].copy_from_slice(&(first as u16).to_le_bytes());
    // The oracle sees the damage too, so it would see Radix's.
    let (clean, report) = fsck_report(&image);
    assert!(!clean, "fsck.fat missed a loop:\n{report}");
    let volume = Volume::new(&image);
    let node = volume.root().lookup("loop").unwrap();
    let mut out = vec![0u8; 20_000];
    assert_eq!(node.read(0, &mut SliceMut(&mut out)).err(), Some(Error::Corrupt));
}

/// What the model of a directory tree holds.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Model {
    File(Vec<u8>),
    Dir,
}

#[derive(Clone, Debug)]
enum Op {
    Create { dir: usize, name: usize, kind: bool },
    Write { path: usize, offset: u16, len: u16, seed: u8 },
    Resize { path: usize, size: u16 },
    Remove { path: usize },
    Rename { from: usize, to_dir: usize, to: usize },
    Remount,
}

/// The names the model uses: two directories' worth, some long, some
/// differing only in case.
const MODEL_NAMES: [&str; 6] =
    ["alpha", "Beta.TXT", "a much longer name.data", "ALPHA2", "d", "e e"];
const MODEL_DIRS: [&str; 3] = ["", "d", "e e"];

fn op() -> impl Strategy<Value = Op> {
    prop_oneof![
        3 => (0..3usize, 0..6usize, any::<bool>()).prop_map(|(dir, name, kind)| Op::Create { dir, name, kind }),
        4 => (0..18usize, 0..12_000u16, 0..9000u16, any::<u8>())
            .prop_map(|(path, offset, len, seed)| Op::Write { path, offset, len, seed }),
        1 => (0..18usize, 0..14_000u16).prop_map(|(path, size)| Op::Resize { path, size }),
        1 => (0..18usize).prop_map(|path| Op::Remove { path }),
        1 => (0..18usize, 0..3usize, 0..6usize).prop_map(|(from, to_dir, to)| Op::Rename { from, to_dir, to }),
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

proptest! {
    #![proptest_config(ProptestConfig { cases: 24, ..ProptestConfig::default() })]

    #[test]
    fn the_driver_matches_a_model(width in 0..5usize, ops in proptest::collection::vec(op(), 1..40)) {
        let (bits, kib, per_cluster) = VOLUMES[width];
        let mut volume = Volume::new(&mkfs(bits, kib, per_cluster));
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
                            let end = usize::from(offset) + data.len();
                            if !data.is_empty() {
                                if bytes.len() < end {
                                    bytes.resize(end, 0);
                                }
                                bytes[usize::from(offset)..end].copy_from_slice(&data);
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
                            bytes.resize(usize::from(size), 0);
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
