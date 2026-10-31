// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Host tests of writing NTFS (ADR 0237): Radix writes files,
//! directories, names and links into volumes `mkntfs` made, and other
//! implementations check what it wrote. ntfs-3g's `ntfsresize --info`
//! finds each cluster in use held by one file and each file's clusters in
//! use, its `ntfsfix -n` finds `$MFTMirr` matching `$MFT`, its `ntfscat`
//! and `ntfsls` read files and directories by their paths; The Sleuth
//! Kit's `fls` and `icat` read every file back, finding none in no
//! directory.

use std::collections::BTreeMap;
use std::process::Command;
use std::string::String;
use std::vec::Vec;
use std::{format, vec};

use proptest::prelude::*;
use radix_block::ram::RamDisk;
use radix_block::{BlockDevice, DeviceRef};
use radix_vfs::mem::{self, KArc};
use radix_vfs::{Access, Error, FileSystem, Kind, NodeRef, Slice};

use crate::NtfsFs;
use crate::tests::{
    Image, SIZES, VOLUMES, names, pattern, read_all, run, scratch, test_names, tool, walk,
};

const DIR: Access = Access::default_for(Kind::Directory);
const FILE: Access = Access::default_for(Kind::File);

/// 2026-10-22 00:42:22 UTC, for files' times.
fn clock() -> u64 {
    1_792_629_742_000_000_000
}

/// A volume mounted writable, on a RAM disk.
struct Volume {
    disk: KArc<RamDisk>,
    fs: KArc<NtfsFs>,
}

impl Volume {
    fn mount_in(block: u32, image: &[u8], read_only: bool) -> Result<Volume, Error> {
        radix_vfs::set_clock(clock);
        let sectors = image.len() as u64 / 512;
        let disk = mem::arc(RamDisk::with_blocks(sectors, false, block)).unwrap();
        disk.load(0, image).unwrap();
        let device: DeviceRef = disk.clone();
        let fs = NtfsFs::mount(device, read_only)?;
        Ok(Volume { disk, fs })
    }

    fn new_in(block: u32, image: &[u8]) -> Volume {
        Volume::mount_in(block, image, false).unwrap()
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

fn write_all(node: &NodeRef, offset: u64, data: &[u8]) {
    assert_eq!(node.write(offset, &Slice(data)).unwrap(), data.len());
}

/// A file `name` in `dir` holding `data`, written in pieces of `piece`
/// bytes.
fn make_file(dir: &NodeRef, name: &str, data: &[u8], piece: usize) -> NodeRef {
    let node = dir.create(name, Kind::File, FILE).unwrap();
    for (i, part) in data.chunks(piece.max(1)).enumerate() {
        write_all(&node, (i * piece) as u64, part);
    }
    node
}

/// The image in a scratch file, removed when done with.
struct Scratch(std::path::PathBuf);

impl Scratch {
    fn new(image: &[u8]) -> Scratch {
        let path = scratch("check.img");
        std::fs::write(&path, image).unwrap();
        Scratch(path)
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

/// Checks `image` as ntfs-3g does: its clusters counted
/// (`ntfsresize --info`, which fails on a cluster in use that no file
/// holds or a file's cluster not in use) and its `$MFTMirr` the same as
/// `$MFT` (`ntfsfix -n`, which fails if not, or if the volume is dirty).
fn ntfs_3g_check(image: &[u8]) {
    let file = Scratch::new(image);
    let out =
        Command::new(tool("ntfsresize")).args(["-i", "-f", "-n"]).arg(&file.0).output().unwrap();
    let text = String::from_utf8_lossy(&out.stdout);
    if !out.status.success() || !text.contains("Accounting clusters") {
        if let Ok(keep) = std::env::var("NTFS_KEEP") {
            std::fs::write(keep, image).unwrap();
        }
        panic!("ntfsresize:\n{text}{}", String::from_utf8_lossy(&out.stderr));
    }
    let out = Command::new(tool("ntfsfix")).arg("-n").arg(&file.0).output().unwrap();
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(out.status.success() && text.contains("Mounting volume... OK"), "ntfsfix:\n{text}");
}

/// The paths of the files and directories `fls` finds in `image` (not
/// the system's), each with its inode as `icat` takes it. Fails on a file
/// in no directory, which `fls` finds under `$OrphanFiles`.
fn fls(image: &Scratch) -> BTreeMap<String, (Kind, String)> {
    let out = run(Command::new(tool("fls")).args(["-r", "-p", "-u", "-f", "ntfs"]).arg(&image.0));
    let mut found = BTreeMap::new();
    for line in String::from_utf8(out).unwrap().lines() {
        // `r/r 64-128-1:	dir/name`, or `d/d 65-144-2:	dir`.
        let (number, path) = line.split_once(":\t").unwrap();
        assert!(!path.starts_with("$OrphanFiles/"), "a file in no directory: {line}");
        if path.starts_with('$') {
            continue;
        }
        let (kind, number) = number.split_once(' ').unwrap();
        let kind = if kind.starts_with('d') { Kind::Directory } else { Kind::File };
        found.insert(String::from(path), (kind, String::from(number)));
    }
    found
}

/// Every file of `image` as The Sleuth Kit reads it: its path and bytes;
/// a directory's bytes are empty.
fn sleuthkit_files(image: &[u8]) -> BTreeMap<String, Vec<u8>> {
    let file = Scratch::new(image);
    let mut files = BTreeMap::new();
    for (path, (kind, number)) in fls(&file) {
        let data = if kind == Kind::Directory {
            Vec::new()
        } else {
            run(Command::new(tool("icat")).args(["-f", "ntfs"]).arg(&file.0).arg(&number))
        };
        files.insert(path, data);
    }
    files
}

/// The names `ntfsls` lists in the directory `path` of `image`.
fn ntfsls(image: &[u8], path: &str) -> Vec<String> {
    let file = Scratch::new(image);
    let out = run(Command::new(tool("ntfsls")).args(["-p", path]).arg(&file.0));
    // `ntfsls` lists a directory's own entry, `.`, but for the root's.
    let mut names: Vec<String> =
        String::from_utf8(out).unwrap().lines().filter(|n| *n != ".").map(String::from).collect();
    names.sort();
    names
}

/// The bytes of the file at `path` in `image`, as `ntfscat` reads them.
fn ntfscat(image: &[u8], path: &str) -> Vec<u8> {
    let file = Scratch::new(image);
    run(Command::new(tool("ntfscat")).arg(&file.0).arg(path))
}

/// Checks that `image` is sound and holds exactly `expected`, as ntfs-3g
/// and The Sleuth Kit read it, and as Radix reads it mounted again.
fn check(image: &[u8], block: u32, expected: &BTreeMap<String, Vec<u8>>) {
    ntfs_3g_check(image);
    let seen = sleuthkit_files(image);
    assert_eq!(seen.keys().collect::<Vec<_>>(), expected.keys().collect::<Vec<_>>());
    for (path, data) in expected {
        assert!(seen[path] == *data, "{path}: The Sleuth Kit reads other bytes");
    }
    let volume = Volume::mount_in(block, image, true).unwrap();
    assert!(!volume.fs.dirty(), "synced, the volume is clean");
    for (path, data) in expected {
        let node = walk(&volume.root(), path).unwrap();
        if node.stat().kind == Kind::File {
            assert!(read_all(&node) == *data, "{path}: Radix reads other bytes");
        }
    }
}

#[test]
fn files_written_read_back_elsewhere() {
    for (mib, sector, cluster) in VOLUMES {
        let image = Image::mkntfs(mib, sector, cluster);
        let volume = Volume::new_in(sector, &image.bytes());
        assert!(!volume.fs.read_only());
        let free = volume.fs.info().free;
        let root = volume.root();
        let dir = root.create("Documents and Settings", Kind::Directory, DIR).unwrap();
        let sub = dir.create("sub", Kind::Directory, DIR).unwrap();
        let mut expected = BTreeMap::new();
        expected.insert(String::from("Documents and Settings"), Vec::new());
        expected.insert(String::from("Documents and Settings/sub"), Vec::new());
        for (i, name) in test_names().iter().enumerate() {
            let data = pattern(i as u32 + 1, SIZES[i % SIZES.len()]);
            let parent = [&root, &dir, &sub][i % 3];
            // Written whole, in pieces of a sector's size, and in pieces
            // that cross the end of the record and of clusters.
            make_file(parent, name, &data, [usize::MAX, 512, 777][i % 3]);
            let path = ["", "Documents and Settings/", "Documents and Settings/sub/"][i % 3];
            expected.insert(format!("{path}{name}"), data);
        }
        assert!(volume.fs.info().free < free);
        drop((root, dir, sub));
        let written = volume.image();
        check(&written, sector, &expected);
        // ntfs-3g finds the files by their names, and lists them.
        let name = &test_names()[3];
        assert!(ntfscat(&written, &format!("/{name}")) == expected[name.as_str()]);
        let mut listed = ntfsls(&written, "/Documents and Settings");
        listed.retain(|n| n != "sub");
        let mut want: Vec<String> = test_names()
            .into_iter()
            .enumerate()
            .filter(|(i, _)| i % 3 == 1)
            .map(|(_, n)| n)
            .collect();
        want.sort();
        assert_eq!(listed, want);
        // The free clusters are those `$Bitmap` has clear.
        let volume = volume.remount();
        let bitmap = ntfscat(&written, "$Bitmap");
        let info = volume.fs.info();
        let used =
            (0..info.blocks).filter(|&c| bitmap[(c / 8) as usize] & (1 << (c % 8)) != 0).count();
        assert_eq!(info.free, info.blocks - used as u64);
        assert_eq!(names(&volume.root()).len(), 1 + test_names().len().div_ceil(3));
    }
}

#[test]
fn a_directory_of_thousands_grows_and_empties() {
    for (mib, sector, cluster) in [(16, 512, 512), (32, 4096, 4096)] {
        let image = Image::mkntfs(mib, sector, cluster);
        let volume = Volume::new_in(sector, &image.bytes());
        let dir = volume.root().create("many", Kind::Directory, DIR).unwrap();
        // More names than the master file table first holds records, in
        // more index blocks than one level holds.
        let name = |i: usize| format!("{i:05} a name to fill index blocks sooner.txt");
        const COUNT: usize = 2500;
        for i in (0..COUNT).rev() {
            let data = pattern(i as u32, i % 3 * 300);
            make_file(&dir, &name(i), &data, usize::MAX);
        }
        let listed: Vec<String> = names(&dir).into_iter().map(|(n, _)| n).collect();
        let want: Vec<String> = (0..COUNT).map(name).collect();
        assert_eq!(listed, want);
        let image = volume.image();
        ntfs_3g_check(&image);
        assert_eq!(ntfsls(&image, "/many"), want);
        assert!(ntfscat(&image, &format!("/many/{}", name(1001))) == pattern(1001, 600));
        // Every other one taken out.
        for i in (0..COUNT).step_by(2) {
            dir.remove(&name(i)).unwrap();
        }
        let image = volume.image();
        ntfs_3g_check(&image);
        let want: Vec<String> = (1..COUNT).step_by(2).map(name).collect();
        assert_eq!(ntfsls(&image, "/many"), want);
        // Taken out as `rm -r` does, a listing's names at a time.
        let mut cursor = 0;
        let mut seen = Vec::new();
        loop {
            let mut batch = Vec::new();
            dir.list(cursor, &mut |next, name, _| {
                if batch.len() == 50 {
                    return false;
                }
                batch.push(String::from(name));
                cursor = next;
                true
            })
            .unwrap();
            if batch.is_empty() {
                break;
            }
            for name in &batch {
                dir.remove(name).unwrap();
            }
            seen.extend(batch);
        }
        assert_eq!(seen, want);
        assert!(names(&dir).is_empty());
        drop(dir);
        volume.root().remove("many").unwrap();
        check(&volume.image(), sector, &BTreeMap::new());
    }
}

#[test]
fn names_move_and_links_count() {
    let image = Image::mkntfs(32, 512, 4096);
    let volume = Volume::new_in(512, &image.bytes());
    let root = volume.root();
    let one = root.create("one", Kind::Directory, DIR).unwrap();
    let two = root.create("two", Kind::Directory, DIR).unwrap();
    let data = pattern(7, 9000);
    let file = make_file(&root, "a.txt", &data, usize::MAX);
    // Another case of the same name is the same name; a rename changes
    // its case.
    assert_eq!(root.create("A.TXT", Kind::File, FILE).err(), Some(Error::Exists));
    root.rename("a.txt", &*root, "A.TXT").unwrap();
    assert_eq!(names(&root)[0].0, "A.TXT");
    root.rename("a.txt", &*one, "b.txt").unwrap();
    two.link("c.txt", &file).unwrap();
    assert_eq!(file.stat().links, 2);
    let small = make_file(&two, "d.txt", b"replaced", usize::MAX);
    // A rename over a file replaces it, and its record is freed.
    two.rename("c.txt", &*two, "d.txt").unwrap();
    assert_eq!(file.stat().links, 2);
    drop(small);
    // A directory moves, with what it holds.
    root.rename("one", &*two, "one").unwrap();
    assert_eq!(walk(&root, "two/one/b.txt").unwrap().stat().id, file.stat().id);
    let other = root.create("three", Kind::Directory, DIR).unwrap();
    make_file(&other, "x", b"x", usize::MAX);
    assert_eq!(root.rename("two", &*root, "three").err(), Some(Error::NotEmpty));
    assert_eq!(root.remove("three").err(), Some(Error::NotEmpty));
    let mut expected = BTreeMap::new();
    for dir in ["two", "two/one", "three"] {
        expected.insert(String::from(dir), Vec::new());
    }
    expected.insert(String::from("three/x"), b"x".to_vec());
    expected.insert(String::from("two/one/b.txt"), data.clone());
    expected.insert(String::from("two/d.txt"), data.clone());
    check(&volume.image(), 512, &expected);
    // One name gone, the other keeps the file.
    walk(&root, "two/one").unwrap().remove("b.txt").unwrap();
    assert_eq!(file.stat().links, 1);
    expected.remove("two/one/b.txt");
    check(&volume.image(), 512, &expected);
    // Bad names for Windows are refused.
    for bad in ["a:b", "CON", "nul.txt", "end.", "end ", "q?"] {
        assert_eq!(root.create(bad, Kind::File, FILE).err(), Some(Error::BadName), "{bad}");
    }
    // The last name gone, the file is too once nothing holds it.
    two.remove("d.txt").unwrap();
    assert_eq!(file.stat().links, 0);
    drop(file);
    expected.remove("two/d.txt");
    drop((root, one, two, other));
    check(&volume.image(), 512, &expected);
}

#[test]
fn sizes_change_both_ways() {
    for (mib, sector, cluster) in [(16, 512, 512), (32, 512, 4096), (64, 512, 65536)] {
        let image = Image::mkntfs(mib, sector, cluster);
        let volume = Volume::new_in(sector, &image.bytes());
        let root = volume.root();
        // Appended in pieces from nothing: in the record, then out of it.
        let grown = root.create("grown", Kind::File, FILE).unwrap();
        let mut want = Vec::new();
        for (i, len) in [1usize, 100, 400, 3000, 1, 70_000].into_iter().enumerate() {
            let part = pattern(i as u32 + 40, len);
            write_all(&grown, want.len() as u64, &part);
            want.extend_from_slice(&part);
        }
        // Cut short, then made longer: what it grows by reads as zeros.
        grown.set_size(5000).unwrap();
        want.truncate(5000);
        grown.set_size(200_000).unwrap();
        want.resize(200_000, 0);
        // Written past its end: the gap reads as zeros.
        let tail = pattern(9, 1234);
        write_all(&grown, 1 << 20, &tail);
        want.resize(1 << 20, 0);
        want.extend_from_slice(&tail);
        // A small file made longer while still in its record.
        let small = make_file(&root, "small", b"hello", usize::MAX);
        small.set_size(64).unwrap();
        let mut small_want = b"hello".to_vec();
        small_want.resize(64, 0);
        // A file emptied.
        let emptied = make_file(&root, "emptied", &pattern(3, 50_000), usize::MAX);
        emptied.set_size(0).unwrap();
        assert_eq!(emptied.stat().allocated, 0);
        let mut expected = BTreeMap::new();
        expected.insert(String::from("grown"), want.clone());
        expected.insert(String::from("small"), small_want);
        expected.insert(String::from("emptied"), Vec::new());
        drop((root, grown, small, emptied));
        check(&volume.image(), sector, &expected);
        let volume = volume.remount();
        let grown = walk(&volume.root(), "grown").unwrap();
        assert_eq!(grown.stat().size, want.len() as u64);
        assert!(grown.stat().allocated >= want.len() as u64);
    }
}

#[test]
fn a_full_volume_says_so_and_empties() {
    let image = Image::mkntfs(16, 512, 4096);
    let volume = Volume::new_in(512, &image.bytes());
    let root = volume.root();
    let big = root.create("big", Kind::File, FILE).unwrap();
    // The master file table grew for its record, and stays grown.
    let free = volume.fs.info().free;
    let piece = pattern(1, 1 << 20);
    let mut written = 0u64;
    let full = loop {
        match big.write(written, &Slice(&piece)) {
            Ok(n) if n == piece.len() => written += n as u64,
            Ok(n) => break (written + n as u64, Ok(n)),
            Err(e) => break (written, Err(e)),
        }
    };
    assert!(matches!(full.1, Err(Error::NoSpace) | Ok(_)), "{full:?}");
    assert!(full.0 > 8 << 20, "{full:?}");
    let image = volume.image();
    ntfs_3g_check(&image);
    drop(big);
    root.remove("big").unwrap();
    drop(root);
    let image = volume.image();
    check(&image, 512, &BTreeMap::new());
    assert_eq!(volume.fs.info().free, free);
}

#[test]
fn what_radix_must_not_change_mounts_read_only() {
    let image = Image::mkntfs(16, 512, 4096);
    // Left dirty: written to, and never synced.
    let volume = Volume::new_in(512, &image.bytes());
    make_file(&volume.root(), "x", b"x", usize::MAX);
    let dirty = volume.raw();
    drop(volume);
    assert_eq!(Volume::mount_in(512, &dirty, false).err().unwrap(), Error::NotSupported);
    let kept = Volume::mount_in(512, &dirty, true).unwrap();
    assert!(kept.fs.dirty());
    assert!(kept.fs.kept_from_writing().is_some_and(|why| why.contains("checked")));
    drop(kept);
    let fresh = Volume::mount_in(512, &image.bytes(), true).unwrap();
    assert_eq!(fresh.fs.kept_from_writing(), None);
    drop(fresh);
    // A log with changes to replay: a restart page saying a client is in
    // it, and not that the log is clean.
    let mut logged = image.bytes();
    let info = run(Command::new(tool("ntfsinfo")).args(["-i", "2", "-v"]).arg(&image.0));
    let info = String::from_utf8(info).unwrap();
    let runs = info.lines().skip_while(|l| !l.contains("Runlist:")).nth(1).unwrap();
    let lcn =
        u64::from_str_radix(runs.split_whitespace().nth(1).unwrap().trim_start_matches("0x"), 16);
    let at = (lcn.unwrap() * 4096) as usize;
    let page = &mut logged[at..at + 4096];
    page[..4].copy_from_slice(b"RSTR");
    page[0x04..0x06].copy_from_slice(&0x1eu16.to_le_bytes());
    page[0x06..0x08].copy_from_slice(&9u16.to_le_bytes());
    page[0x10..0x14].copy_from_slice(&4096u32.to_le_bytes());
    page[0x14..0x18].copy_from_slice(&4096u32.to_le_bytes());
    page[0x18..0x1a].copy_from_slice(&0x30u16.to_le_bytes());
    page[0x1a..0x1c].copy_from_slice(&1u16.to_le_bytes());
    page[0x1c..0x1e].copy_from_slice(&1u16.to_le_bytes());
    page[0x30 + 0x0c..0x30 + 0x0e].copy_from_slice(&0u16.to_le_bytes());
    page[0x30 + 0x0e..0x30 + 0x10].copy_from_slice(&0u16.to_le_bytes());
    assert_eq!(Volume::mount_in(512, &logged, false).err().unwrap(), Error::NotSupported);
    let kept = Volume::mount_in(512, &logged, true).unwrap();
    assert!(kept.fs.kept_from_writing().is_some_and(|why| why.contains("journal")));
}

#[test]
fn what_ntfs_3g_wrote_is_written_to() {
    let image = crate::tests::ntfs_3g();
    let volume = Volume::new_in(512, &image);
    let root = volume.root();
    // Compressed files are not written yet (ADR 0237); plain ones are,
    // in the same directories.
    let compressed = walk(&root, "compressed/text.txt").unwrap();
    assert_eq!(compressed.write(0, &Slice(b"x")).err(), Some(Error::NotSupported));
    // Nor files whose record overflows into others.
    let linked = walk(&root, "linked.txt").unwrap();
    assert_eq!(linked.write(0, &Slice(b"x")).err(), Some(Error::NotSupported));
    drop(linked);
    let data = pattern(5, 20_000);
    let dir = walk(&root, "compressed").unwrap();
    make_file(&dir, "plain.bin", &data, usize::MAX);
    drop((root, dir, compressed));
    let written = volume.image();
    ntfs_3g_check(&written);
    let seen = sleuthkit_files(&written);
    assert!(seen["compressed/plain.bin"] == data);
    assert!(seen.contains_key("compressed/text.txt"));
}

/// What a step of [`random_changes_read_back`] does.
#[derive(Clone, Debug)]
enum Step {
    /// Makes a file of `len` bytes in directory `dir` (of those made).
    File { dir: usize, len: usize },
    /// Makes a directory in directory `dir`.
    Dir { dir: usize },
    /// Writes `len` bytes at `at` into a file.
    Write { file: usize, at: usize, len: usize },
    /// Sets a file's size.
    Size { file: usize, len: usize },
    /// Moves a file to another directory, under a new name.
    Move { file: usize, dir: usize },
    /// Takes a file out.
    Remove { file: usize },
}

fn step() -> impl Strategy<Value = Step> {
    prop_oneof![
        (0..8usize, 0..20_000usize).prop_map(|(dir, len)| Step::File { dir, len }),
        (0..8usize).prop_map(|dir| Step::Dir { dir }),
        (0..32usize, 0..30_000usize, 1..9000usize).prop_map(|(file, at, len)| Step::Write {
            file,
            at,
            len
        }),
        (0..32usize, 0..40_000usize).prop_map(|(file, len)| Step::Size { file, len }),
        (0..32usize, 0..8usize).prop_map(|(file, dir)| Step::Move { file, dir }),
        (0..32usize).prop_map(|file| Step::Remove { file }),
    ]
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 12, ..ProptestConfig::default() })]
    #[test]
    fn random_changes_read_back(steps in proptest::collection::vec(step(), 1..60)) {
        let image = Image::mkntfs(16, 512, 1024);
        let volume = Volume::new_in(512, &image.bytes());
        // The directories made, by path ("" the root), and the files, by
        // path, with what they hold.
        let mut dirs = vec![String::new()];
        let mut files: Vec<(String, Vec<u8>)> = Vec::new();
        let mut made = 0u32;
        let node = |path: &str| walk(&volume.root(), path).unwrap();
        let join = |dir: &str, name: &str| if dir.is_empty() { String::from(name) } else { format!("{dir}/{name}") };
        for step in steps {
            made += 1;
            match step {
                Step::File { dir, len } => {
                    let dir = dirs[dir % dirs.len()].clone();
                    let path = join(&dir, &format!("file {made}.dat"));
                    let data = pattern(made, len);
                    make_file(&node(&dir), &format!("file {made}.dat"), &data, 4096);
                    files.push((path, data));
                }
                Step::Dir { dir } => {
                    let dir = dirs[dir % dirs.len()].clone();
                    node(&dir).create(&format!("dir {made}"), Kind::Directory, DIR).unwrap();
                    dirs.push(join(&dir, &format!("dir {made}")));
                }
                Step::Write { file, at, len } if !files.is_empty() => {
                    let i = file % files.len();
                    let (path, data) = &mut files[i];
                    let part = pattern(made, len);
                    write_all(&node(path), at as u64, &part);
                    if data.len() < at + len {
                        data.resize(at + len, 0);
                    }
                    data[at..at + len].copy_from_slice(&part);
                }
                Step::Size { file, len } if !files.is_empty() => {
                    let i = file % files.len();
                    let (path, data) = &mut files[i];
                    node(path).set_size(len as u64).unwrap();
                    data.resize(len, 0);
                }
                Step::Move { file, dir } if !files.is_empty() => {
                    let to = dirs[dir % dirs.len()].clone();
                    let i = file % files.len();
                    let (path, _) = &mut files[i];
                    let (from, name) = path.rsplit_once('/').unwrap_or(("", path.as_str()));
                    let new = format!("moved {made}");
                    node(from).rename(name, &*node(&to), &new).unwrap();
                    *path = join(&to, &new);
                }
                Step::Remove { file } if !files.is_empty() => {
                    let (path, _) = files.remove(file % files.len());
                    let (from, name) = path.rsplit_once('/').unwrap_or(("", path.as_str()));
                    node(from).remove(name).unwrap();
                }
                _ => {}
            }
        }
        let mut expected: BTreeMap<String, Vec<u8>> = files.into_iter().collect();
        for dir in dirs.into_iter().skip(1) {
            expected.insert(dir, Vec::new());
        }
        check(&volume.image(), 512, &expected);
    }
}
