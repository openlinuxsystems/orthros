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

use std::collections::{BTreeMap, BTreeSet};
use std::process::Command;
use std::string::String;
use std::vec::Vec;
use std::{format, vec};

use proptest::prelude::*;
use radix_block::ram::RamDisk;
use radix_block::{BlockDevice, DeviceRef};
use radix_vfs::mem::{self, KArc};
use radix_vfs::{Access, Error, FileSystem, Kind, NodeRef, Slice, SliceMut};

use crate::NtfsFs;
use crate::fs::Options;
use crate::node::NtfsNode;
use crate::tests::{
    Image, SIZES, VOLUMES, names, pattern, read_all, read_link, run, scratch, test_names, tool,
    walk,
};
use crate::windows_tests::{self, Found};

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
        Volume::mount_with(block, image, Options { read_only, ..Options::default() })
    }

    fn mount_with(block: u32, image: &[u8], options: Options) -> Result<Volume, Error> {
        radix_vfs::set_clock(clock);
        let sectors = image.len() as u64 / 512;
        let disk = mem::arc(RamDisk::with_blocks(sectors, false, block)).unwrap();
        disk.load(0, image).unwrap();
        let device: DeviceRef = disk.clone();
        let fs = NtfsFs::mount_with(device, options)?;
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
    let mut find = Vec::new();
    for (path, data) in expected {
        let node = walk(&volume.root(), path).unwrap();
        if node.stat().kind == Kind::File {
            assert!(read_all(&node) == *data, "{path}: Radix reads other bytes");
            find.push((path.as_str(), Found::File(data)));
        } else {
            find.push((path.as_str(), Found::Dir));
        }
    }
    windows_tests::keep(image, &find, false);
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
fn a_hibernation_is_discarded_when_asked() {
    let discard = Options { discard_hibernation: true, ..Options::default() };
    let image = Image::mkntfs(16, 512, 4096);
    let mut hiberfil = b"wake".to_vec();
    hiberfil.extend(pattern(5, 20_000));
    image.copy_in("hiberfil.sys", &hiberfil);
    hiberfil[..4].copy_from_slice(b"HIBR");
    // Hibernated, and left dirty, as a hibernated Windows leaves it.
    let dirty = {
        let volume = Volume::new_in(512, &image.bytes());
        write_all(&walk(&volume.root(), "hiberfil.sys").unwrap(), 0, b"HIBR");
        volume.fs.sync().unwrap();
        make_file(&volume.root(), "dirty", b"x", usize::MAX);
        volume.raw()
    };
    // Not asked, it is only read.
    assert_eq!(Volume::mount_in(512, &dirty, false).err(), Some(Error::NotSupported));
    let volume = Volume::mount_with(512, &dirty, discard).unwrap();
    assert!(volume.fs.hibernated() && volume.fs.discarded_hibernation());
    make_file(&volume.root(), "written.txt", b"written over a hibernation", usize::MAX);
    let written = volume.image();
    drop(volume);
    // The hibernation is gone, the rest of the file is as it was, and the
    // volume stays marked for Windows to check.
    // ntfs-3g reads a volume marked for checking only when forced to.
    let forced_cat = |path: &str| {
        let file = Scratch::new(&written);
        run(Command::new(tool("ntfscat")).arg("-f").arg(&file.0).arg(path))
    };
    let after = forced_cat("/hiberfil.sys");
    assert!(after[..4096].iter().all(|&b| b == 0) && after[4096..] == hiberfil[4096..]);
    let again = Volume::mount_in(512, &written, true).unwrap();
    assert!(again.fs.dirty() && !again.fs.hibernated());
    assert_eq!(
        read_all(&walk(&again.root(), "written.txt").unwrap()),
        b"written over a hibernation"
    );
    drop(again);
    assert_eq!(forced_cat("/written.txt"), b"written over a hibernation");
    let file = Scratch::new(&written);
    let out =
        Command::new(tool("ntfsresize")).args(["-i", "-f", "-n"]).arg(&file.0).output().unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stdout));
    // Asked of a volume Windows did not hibernate with: a clean one
    // mounts as any does, and one left dirty is still only read.
    let clean = Volume::mount_with(512, &image.bytes(), Options::default()).unwrap().image();
    let volume = Volume::mount_with(512, &clean, discard).unwrap();
    assert!(!volume.fs.hibernated() && !volume.fs.discarded_hibernation());
    drop(volume);
    let plain = Image::mkntfs(16, 512, 4096);
    let left = {
        let volume = Volume::new_in(512, &plain.bytes());
        make_file(&volume.root(), "x", b"x", usize::MAX);
        volume.raw()
    };
    assert_eq!(Volume::mount_with(512, &left, discard).err(), Some(Error::NotSupported));
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

/// Writes `data` at `at` into the file at `path` under `root`, and checks
/// that it reads back so; `expected` keeps what each file holds.
fn changed(
    root: &NodeRef,
    expected: &mut BTreeMap<String, Vec<u8>>,
    path: &str,
    at: usize,
    data: &[u8],
) -> NodeRef {
    let node = walk(root, path).unwrap();
    let mut now = expected.remove(path).unwrap_or_else(|| read_all(&node));
    write_all(&node, at as u64, data);
    if now.len() < at + data.len() {
        now.resize(at + data.len(), 0);
    }
    now[at..at + data.len()].copy_from_slice(data);
    assert!(read_all(&node) == now, "{path}: reads back otherwise");
    expected.insert(String::from(path), now);
    node
}

#[test]
fn what_ntfs_3g_wrote_is_written_to() {
    let image = crate::tests::ntfs_3g();
    let volume = Volume::new_in(512, &image);
    let root = volume.root();
    let mut expected: BTreeMap<String, Vec<u8>> = BTreeMap::new();
    let mut change =
        |path: &str, at: usize, data: &[u8]| changed(&root, &mut expected, path, at, data);
    // Compressed files, a unit (8 KiB here) at a time: into the middle of
    // text that compresses, over noise that does not, and into the zeros
    // of a unit left a hole; and past their ends.
    let text = change("compressed/text.txt", 50_000, b"changed in the middle of a unit");
    change("compressed/text.txt", 8192, &pattern(9, 8192));
    let size = read_all(&text).len();
    change("compressed/text.txt", size + 3000, b"past the end, after zeros");
    change("compressed/random.bin", 100, &[0u8; 3000]);
    change("compressed/mixed.bin", 12_000, b"in the hole");
    change("compressed/mixed.bin", 40_000, &crate::tests::text()[..5000]);
    // A sparse file: into its hole, and past its end, where a hole grows.
    change("sparse.bin", 1 << 20, b"in the hole");
    change("sparse.bin", (8 << 20) + 5, b"far past the end");
    drop(change);
    // Cut inside a unit, and grown again: what was past the cut is zeros.
    let mixed = walk(&root, "compressed/mixed.bin").unwrap();
    mixed.set_size(12_005).unwrap();
    mixed.set_size(30_000).unwrap();
    let mut now = expected["compressed/mixed.bin"][..12_005].to_vec();
    now.resize(30_000, 0);
    assert!(read_all(&mixed) == now);
    expected.insert(String::from("compressed/mixed.bin"), now);
    // What is made in the directory marked compressed is compressed.
    let dir = walk(&root, "compressed").unwrap();
    let made = crate::tests::text();
    let new = make_file(&dir, "new.txt", &made, 3000);
    let small = make_file(&dir, "small.txt", b"small, in its record", usize::MAX);
    expected.insert(String::from("compressed/new.txt"), made.clone());
    expected.insert(String::from("compressed/small.txt"), b"small, in its record".to_vec());
    // Files whose names overflow their record into others are not
    // written yet.
    let linked = walk(&root, "linked.txt").unwrap();
    assert_eq!(linked.write(0, &Slice(b"x")).err(), Some(Error::NotSupported));
    drop((root, dir, text, mixed, new, small, linked));
    let volume = volume.remount();
    let new = walk(&volume.root(), "compressed/new.txt").unwrap();
    assert!(read_all(&new) == made);
    assert!(new.stat().allocated < made.len() as u64 / 4, "new.txt is not compressed");
    let sparse = walk(&volume.root(), "sparse.bin").unwrap();
    assert!(sparse.stat().allocated < 1 << 20, "sparse.bin lost its holes");
    drop((new, sparse));
    let written = volume.image();
    ntfs_3g_check(&written);
    let seen = sleuthkit_files(&written);
    for (path, data) in &expected {
        assert!(seen[path] == *data, "{path}: The Sleuth Kit reads other bytes");
        assert!(
            ntfscat(&written, &format!("/{path}")) == *data,
            "{path}: ntfs-3g reads other bytes"
        );
    }
    let find: Vec<_> =
        expected.iter().map(|(path, data)| (path.as_str(), Found::File(data))).collect();
    windows_tests::keep(&written, &find, false);
}

/// What `$Extend\$Reparse` lists in `volume`: each reparse point's tag
/// and its file's record.
fn reparse_points(volume: &Volume) -> Vec<(u32, u64)> {
    let root = volume.root();
    let node = radix_vfs::downcast::<NtfsNode>(&*root).unwrap();
    node.reparse_points().unwrap().clone()
}

#[test]
fn links_are_made_as_windows_and_wsl_make_them() {
    use crate::reparse::tag::{LX_SYMLINK, SYMLINK};
    for (mib, sector, cluster) in VOLUMES {
        let image = Image::mkntfs(mib, sector, cluster);
        let volume = Volume::new_in(sector, &image.bytes());
        let root = volume.root();
        let docs = root.create("docs", Kind::Directory, DIR).unwrap();
        make_file(&docs, "a.txt", b"a\n", usize::MAX);
        // Relative paths of names Windows takes are Windows' links, to a
        // directory as one; any other path is WSL's, as it is.
        let links: &[(&str, &str, &str, u32)] = &[
            ("", "to docs", "docs", SYMLINK),
            ("", "to a", "docs/a.txt", SYMLINK),
            ("", "nowhere", "missing/file", SYMLINK),
            ("docs", "up", "..", SYMLINK),
            ("docs", "here", "./a.txt", SYMLINK),
            ("", "absolute", "/system/resources", LX_SYMLINK),
            ("", "colon", "a:b", LX_SYMLINK),
            ("", "slash", "docs/", LX_SYMLINK),
            ("docs", "\u{1f600} emoji", "\u{e9}t\u{e9}/\u{1f600}", SYMLINK),
        ];
        let mut records = Vec::new();
        for &(dir, name, target, tag) in links {
            let at = if dir.is_empty() { root.clone() } else { walk(&root, dir).unwrap() };
            let node = at.make_link(name, target, FILE).unwrap();
            assert_eq!(node.stat().kind, Kind::Symlink);
            assert_eq!(read_link(&node), target);
            records.push((tag, node.stat().id));
        }
        records.sort_by_key(|&(tag, record)| (tag, record));
        assert_eq!(reparse_points(&volume), records);
        assert_eq!(root.make_link("to a", "x", FILE).err(), Some(Error::Exists));
        assert_eq!(root.make_link("bad?", "x", FILE).err(), Some(Error::BadName));
        // Another name, a new name, and the last name going.
        let to_a = walk(&root, "to a").unwrap();
        root.link("to a again", &to_a).unwrap();
        root.remove("to a").unwrap();
        root.rename("colon", &*docs, "colon moved").unwrap();
        root.remove("nowhere").unwrap();
        let gone = walk(&root, "absolute").unwrap().stat().id;
        root.remove("absolute").unwrap();
        drop((to_a, docs, root));
        records.retain(|&(_, record)| record != gone);
        let volume = volume.remount();
        let root = volume.root();
        let left = [
            ("to docs", "docs"),
            ("to a again", "docs/a.txt"),
            ("docs/up", ".."),
            ("docs/here", "./a.txt"),
            ("docs/colon moved", "a:b"),
            ("slash", "docs/"),
            ("docs/\u{1f600} emoji", "\u{e9}t\u{e9}/\u{1f600}"),
        ];
        let mut listed = Vec::new();
        for (path, target) in left {
            let node = walk(&root, path).unwrap();
            assert_eq!(node.stat().kind, Kind::Symlink, "{path}");
            assert_eq!(read_link(&node), target, "{path}");
            let tag =
                if target.contains(':') || target.ends_with('/') { LX_SYMLINK } else { SYMLINK };
            listed.push((tag, node.stat().id));
        }
        listed.sort_unstable();
        assert_eq!(reparse_points(&volume), listed);
        assert!(walk(&root, "nowhere").is_err() && walk(&root, "absolute").is_err());
        drop(root);
        let written = volume.image();
        ntfs_3g_check(&written);
        // Windows takes a link to a directory as a directory, as The
        // Sleuth Kit shows it, and any other as a file.
        let seen = fls(&Scratch::new(&written));
        for (path, _) in left {
            let dir = matches!(path, "to docs" | "docs/up");
            assert_eq!(seen[path].0 == Kind::Directory, dir, "{path}");
        }
        let mut find = vec![("docs", Found::Dir), ("docs/a.txt", Found::File(b"a\n"))];
        find.extend(left.iter().map(|&(path, _)| (path, Found::Link)));
        windows_tests::keep(&written, &find, false);
    }
}

#[test]
fn reparse_index_grows_into_blocks_and_back() {
    let image = Image::mkntfs(32, 512, 4096);
    let volume = Volume::new_in(512, &image.bytes());
    let root = volume.root();
    let dir = root.create("links", Kind::Directory, DIR).unwrap();
    let mut made = Vec::new();
    for i in 0..600 {
        let target = if i % 3 == 0 { format!("/absolute/{i}") } else { format!("file {i}") };
        let node = dir.make_link(&format!("link {i:03}"), &target, FILE).unwrap();
        made.push((i, node.stat().id));
    }
    let listed = reparse_points(&volume);
    assert_eq!(listed.len(), 600);
    assert!(listed.is_sorted(), "$Reparse keeps its keys in order");
    for (i, _) in made.iter().filter(|(i, _)| i % 2 == 0) {
        dir.remove(&format!("link {i:03}")).unwrap();
    }
    drop((dir, root));
    let volume = volume.remount();
    let mut left: Vec<(u32, u64)> = made
        .iter()
        .filter(|(i, _)| i % 2 == 1)
        .map(|&(i, id)| {
            let tag = if i % 3 == 0 {
                crate::reparse::tag::LX_SYMLINK
            } else {
                crate::reparse::tag::SYMLINK
            };
            (tag, id)
        })
        .collect();
    left.sort_unstable();
    assert_eq!(reparse_points(&volume), left);
    let root = volume.root();
    let dir = walk(&root, "links").unwrap();
    for (i, _) in made.iter().filter(|(i, _)| i % 2 == 1) {
        dir.remove(&format!("link {i:03}")).unwrap();
    }
    assert_eq!(reparse_points(&volume), []);
    drop((dir, root));
    ntfs_3g_check(&volume.image());
}

#[test]
fn links_windows_and_wsl_made_go() {
    let volume = Volume::new_in(512, &crate::tests::ntfs_3g());
    let root = volume.root();
    let before = reparse_points(&volume);
    // ntfs-3g listed the reparse points it made: Windows' links, the
    // junction and WSL's.
    assert_eq!(before.len(), 4, "{before:x?}");
    for path in ["windows link", "junction", "wsl link", "Documents/Deep/er/absolute link"] {
        let (dir, name) = path.rsplit_once('/').unwrap_or(("", path));
        let dir = if dir.is_empty() { root.clone() } else { walk(&root, dir).unwrap() };
        dir.remove(name).unwrap();
    }
    root.remove("interix link").unwrap();
    assert_eq!(reparse_points(&volume), []);
    drop(root);
    let volume = volume.remount();
    assert_eq!(reparse_points(&volume), []);
    ntfs_3g_check(&volume.image());
}

/// Power cuts all through a sync: the disk takes the first `n` requests
/// of it and fails the rest, for cuts from its start to its end. The
/// first change since the volume was last clean marked it dirty on the
/// disk at once, so whatever a cut leaves is dirty: Radix mounts it only
/// to read it, and Windows checks it at its next mount (CI's Windows job
/// runs `chkdsk /f` on each). The files synced before the changes, which
/// none changed, keep their records and their bytes, as The Sleuth Kit
/// reads them by record.
/// `image`'s master file table, as The Sleuth Kit reads it.
fn mft(image: &[u8]) -> Vec<u8> {
    let file = Scratch::new(image);
    run(Command::new(tool("icat")).args(["-f", "ntfs"]).arg(&file.0).arg("0"))
}

/// Record `number` of the master file table `mft` of records of `size`
/// bytes, its update sequence put back.
fn record_in(mft: &[u8], number: u64, size: usize) -> Vec<u8> {
    let mut rec = mft[number as usize * size..][..size].to_vec();
    crate::record::fix_up(&mut rec, b"FILE").unwrap();
    rec
}

/// The records other than its own that the attribute list of the file in
/// record `number` names for pieces of its data, and whether it has a
/// list.
fn data_records(mft: &[u8], number: u64, size: usize) -> (bool, Vec<u64>) {
    let rec = record_in(mft, number, size);
    let rec = crate::record::Record::new(&rec).unwrap();
    let Some(list) = rec.attrs().map(Result::unwrap).find(|a| a.kind() == 0x20) else {
        return (false, Vec::new());
    };
    assert!(!list.non_resident(), "Radix keeps its lists in their records");
    let mut records = Vec::new();
    for entry in crate::record::list_entries(list.value()) {
        let entry = entry.unwrap();
        let at = crate::record::record_of(entry.reference());
        if entry.kind() == 0x80 && entry.name().is_empty() && at != number {
            records.push(at);
        }
    }
    (true, records)
}

/// Whether record `number` of `mft` is a file's.
fn in_use(mft: &[u8], number: u64, size: usize) -> bool {
    crate::record::Record::new(&record_in(mft, number, size)).unwrap().in_use()
}

/// Files `names` in `dir`, `clusters` clusters each of `cluster` bytes,
/// written a cluster of each in turn, so that each takes every
/// `names.len()`th cluster: as many runs as clusters, more than a record
/// holds where of them. Their data, by name.
fn interleaved(
    dir: &NodeRef,
    names: &[&str],
    clusters: usize,
    cluster: usize,
) -> BTreeMap<String, Vec<u8>> {
    let nodes: Vec<NodeRef> =
        names.iter().map(|name| dir.create(name, Kind::File, FILE).unwrap()).collect();
    let data: Vec<Vec<u8>> =
        (0..names.len()).map(|i| pattern(60 + i as u32, clusters * cluster)).collect();
    for c in 0..clusters {
        for (node, data) in nodes.iter().zip(&data) {
            let at = c * cluster;
            write_all(node, at as u64, &data[at..at + cluster]);
        }
    }
    names.iter().map(|n| String::from(*n)).zip(data).collect()
}

#[test]
fn fragmented_files_spread_over_records() {
    for (mib, sector, cluster) in [(16, 512, 512), (32, 512, 4096), (32, 4096, 4096)] {
        let image = Image::mkntfs(mib, sector, cluster);
        let volume = Volume::new_in(sector, &image.bytes());
        let rs = volume.fs.geometry().record_size as usize;
        let root = volume.root();
        // Two files of 2000 runs each: 6000 bytes of runs, more than a
        // record of 4 KiB holds.
        let mut expected = interleaved(&root, &["big.bin", "gone.bin"], 2000, cluster as usize);
        let big = walk(&root, "big.bin").unwrap();
        let gone = walk(&root, "gone.bin").unwrap();
        let (big_id, gone_id) = (big.stat().id, gone.stat().id);
        drop((big, gone));
        let image = volume.image();
        check(&image, sector, &expected);
        assert!(ntfscat(&image, "/big.bin") == expected["big.bin"], "ntfs-3g reads other bytes");
        let table = mft(&image);
        let (listed, big_pieces) = data_records(&table, big_id, rs);
        assert!(listed && big_pieces.len() >= 2, "{big_pieces:?}");
        let (_, gone_pieces) = data_records(&table, gone_id, rs);
        assert!(gone_pieces.len() >= 2, "{gone_pieces:?}");
        for &n in big_pieces.iter().chain(&gone_pieces) {
            assert!(in_use(&table, n, rs));
        }

        // Mounted again, its pieces read from its list: changed in the
        // middle, given another name, grown, and the other file removed.
        let volume = volume.remount();
        let root = volume.root();
        let big = walk(&root, "big.bin").unwrap();
        let middle = expected["big.bin"].len() / 2 + 77;
        let data = expected.get_mut("big.bin").unwrap();
        write_all(&big, middle as u64, b"changed in a piece in another record");
        data[middle..middle + 36].copy_from_slice(b"changed in a piece in another record");
        let tail = pattern(3, 300_000);
        write_all(&big, data.len() as u64, &tail);
        data.extend_from_slice(&tail);
        root.link("big link.bin", &big).unwrap();
        root.remove("gone.bin").unwrap();
        expected.remove("gone.bin");
        expected.insert(String::from("big link.bin"), expected["big.bin"].clone());
        let image = volume.image();
        check(&image, sector, &expected);
        let table = mft(&image);
        let (listed, pieces) = data_records(&table, big_id, rs);
        assert!(listed && pieces.len() >= 2, "{pieces:?}");
        assert!(!in_use(&table, gone_id, rs));
        for &n in &gone_pieces {
            assert!(!in_use(&table, n, rs), "gone.bin's record {n} is still in use");
        }

        // Cut short, its runs fit its record again: its list goes, and
        // with it the records it named.
        big.set_size(10 * u64::from(cluster)).unwrap();
        root.remove("big link.bin").unwrap();
        expected.remove("big link.bin");
        expected.get_mut("big.bin").unwrap().truncate(10 * cluster as usize);
        drop((root, big));
        let image = volume.image();
        check(&image, sector, &expected);
        let table = mft(&image);
        assert_eq!(data_records(&table, big_id, rs), (false, Vec::new()));
        for &n in &pieces {
            assert!(!in_use(&table, n, rs), "big.bin's record {n} is still in use");
        }
    }
}

#[test]
fn compressed_files_spread_on_their_units_edges() {
    for (mib, sector, cluster, units) in [(16, 512, 512, 250), (32, 512, 4096, 150)] {
        let image = Image::mkntfs(mib, sector, cluster);
        let volume = Volume::new_in(sector, &image.bytes());
        let rs = volume.fs.geometry().record_size as usize;
        let root = volume.root();
        let dir = root.create("compressed", Kind::Directory, DIR).unwrap();
        radix_vfs::downcast::<NtfsNode>(&*dir).unwrap().mark_compressed().unwrap();
        // Two files' units written in turn, each written back: a run of
        // each unit's clusters, apart, and a hole for what it saves.
        let unit = 16 * cluster as usize;
        let text = crate::tests::text();
        let names = ["a.txt", "b.txt"];
        let mut expected = BTreeMap::new();
        expected.insert(String::from("compressed"), Vec::new());
        let nodes: Vec<NodeRef> =
            names.iter().map(|n| dir.create(n, Kind::File, FILE).unwrap()).collect();
        let data: Vec<Vec<u8>> = (0..2)
            .map(|i| text.iter().cycle().skip(i * 1000).take(units * unit).copied().collect())
            .collect();
        for u in 0..units {
            for (node, data) in nodes.iter().zip(&data) {
                write_all(node, (u * unit) as u64, &data[u * unit..(u + 1) * unit]);
                radix_vfs::downcast::<NtfsNode>(&**node).unwrap().flush().unwrap();
            }
        }
        for (name, data) in names.iter().zip(&data) {
            expected.insert(format!("compressed/{name}"), data.clone());
        }
        let ids: Vec<u64> = nodes.iter().map(|n| n.stat().id).collect();
        drop((root, dir, nodes));
        let image = volume.image();
        check(&image, sector, &expected);
        let table = mft(&image);
        for &id in &ids {
            let (listed, pieces) = data_records(&table, id, rs);
            assert!(listed && !pieces.is_empty(), "{pieces:?}");
            // Each piece starts on a unit's edge.
            for &n in &pieces {
                let rec = record_in(&table, n, rs);
                let rec = crate::record::Record::new(&rec).unwrap();
                for attr in rec.attrs().map(Result::unwrap).filter(|a| a.kind() == 0x80) {
                    assert_eq!(attr.lowest_vcn() % 16, 0, "a piece in record {n} splits a unit");
                    assert_eq!((attr.highest_vcn() + 1) % 16, 0, "record {n} splits a unit");
                }
            }
        }
        // Mounted again, a unit in a piece in another record changed.
        let volume = volume.remount();
        let a = walk(&volume.root(), "compressed/a.txt").unwrap();
        let at = units / 2 * unit + 100;
        write_all(&a, at as u64, &pattern(5, 3000));
        let want = expected.get_mut("compressed/a.txt").unwrap();
        want[at..at + 3000].copy_from_slice(&pattern(5, 3000));
        drop(a);
        check(&volume.image(), sector, &expected);
    }
}

#[test]
fn a_power_cut_leaves_what_was_synced() {
    let image = Image::mkntfs(16, 512, 4096);
    let volume = Volume::new_in(512, &image.bytes());
    let root = volume.root();
    let kept = root.create("kept", Kind::Directory, DIR).unwrap();
    let mut synced = BTreeMap::new();
    synced.insert(String::from("kept"), Vec::new());
    let mut records = Vec::new();
    for i in 0..40 {
        let (path, data) =
            (format!("kept/file {i}.txt"), pattern(i, SIZES[i as usize % SIZES.len()]));
        let node = make_file(&kept, &path[5..], &data, 4096);
        records.push((node.stat().id, data.clone()));
        synced.insert(path, data);
    }
    let big = pattern(99, 300_000);
    records.push((make_file(&root, "big.bin", &big, 1 << 16).stat().id, big.clone()));
    synced.insert(String::from("big.bin"), big);
    drop((root, kept));
    let before = volume.image();
    drop(volume);
    check(&before, 512, &synced);
    // The same changes each time: the synced directory grows into index
    // blocks, the master file table grows, and a new directory holds a
    // large file.
    let changes = |volume: &Volume| {
        let root = volume.root();
        let kept = walk(&root, "kept").unwrap();
        for i in 0..300 {
            make_file(&kept, &format!("new {i}"), &pattern(1000 + i, 3000), usize::MAX);
        }
        let fresh = root.create("fresh", Kind::Directory, DIR).unwrap();
        make_file(&fresh, "large.bin", &pattern(7, 2 << 20), 1 << 16);
    };
    let (whole, after) = {
        let volume = Volume::new_in(512, &before);
        changes(&volume);
        let start = volume.disk.requests();
        volume.fs.sync().unwrap();
        (volume.disk.requests() - start, volume.raw())
    };
    assert!(whole > 20, "a sync of {whole} requests");
    let mut find: Vec<(&str, Found<'_>)> = vec![("kept", Found::Dir)];
    find.extend(
        synced.iter().filter(|(p, _)| *p != "kept").map(|(p, d)| (p.as_str(), Found::File(d))),
    );
    // A dozen cuts through it, and each of its last requests.
    let mut cuts: Vec<u64> = (0..whole).step_by((whole / 12) as usize).collect();
    cuts.extend(whole - 6..whole);
    for n in cuts {
        let volume = Volume::new_in(512, &before);
        changes(&volume);
        volume.disk.fail_after(n as u32);
        assert!(volume.fs.sync().is_err(), "a sync cut after {n} of {whole} requests");
        let cut = volume.raw();
        drop(volume);
        let read = Volume::mount_in(512, &cut, true).unwrap();
        let dirty = read.fs.dirty();
        drop(read);
        // Clean, the cut came after the last write, before its flush.
        if !dirty {
            assert!(cut == after, "cut after {n} of {whole} requests, clean but not all written");
            continue;
        }
        assert_eq!(Volume::mount_in(512, &cut, false).err(), Some(Error::NotSupported));
        let file = Scratch::new(&cut);
        for (record, data) in &records {
            let found = run(Command::new(tool("icat"))
                .args(["-f", "ntfs"])
                .arg(&file.0)
                .arg(record.to_string()));
            assert!(found == *data, "cut after {n} of {whole} requests: record {record} changed");
        }
        windows_tests::keep(&cut, &find, true);
    }
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
    #![proptest_config(ProptestConfig { cases: 24, ..ProptestConfig::default() })]
    #[test]
    fn random_changes_read_back(
        steps in proptest::collection::vec(step(), 1..60),
        compressed: bool,
    ) {
        let image = Image::mkntfs(16, 512, 1024);
        let volume = Volume::new_in(512, &image.bytes());
        // Half the time, in a root marked compressed: what is made in it is
        // compressed, 16 clusters at a time.
        if compressed {
            let root = volume.root();
            radix_vfs::downcast::<NtfsNode>(&*root).unwrap().mark_compressed().unwrap();
        }
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

/// [`crate::tests::ntfs_3g`]'s volume, and the sectors of it that hold
/// metadata: its boot sector, and its records and index blocks, found by
/// their signatures.
fn damage_volume() -> &'static (Vec<u8>, Vec<usize>) {
    static VOLUME: std::sync::OnceLock<(Vec<u8>, Vec<usize>)> = std::sync::OnceLock::new();
    VOLUME.get_or_init(|| {
        let image = crate::tests::ntfs_3g();
        let mut sectors = vec![0];
        for (i, sector) in image.chunks(512).enumerate() {
            if sector.starts_with(b"FILE") {
                sectors.extend(i..i + 2);
            } else if sector.starts_with(b"INDX") {
                sectors.extend(i..i + 8);
            }
        }
        (image, sectors)
    })
}

/// Reads everything under `dir` once, ignoring what fails.
fn read_everything(dir: &NodeRef, seen: &mut BTreeSet<u64>) {
    let mut names = Vec::new();
    let _ = dir.list(0, &mut |_, name, _| {
        names.push(String::from(name));
        names.len() < 1000
    });
    for name in names {
        let Ok(node) = dir.lookup(&name) else { continue };
        let stat = node.stat();
        if !seen.insert(stat.id) {
            continue;
        }
        match stat.kind {
            Kind::Directory => read_everything(&node, seen),
            Kind::Symlink => {
                let _ = node.read_link(&mut SliceMut(&mut [0; 300]));
            }
            _ => {
                let _ = node.read(0, &mut SliceMut(&mut vec![0; 70_000]));
            }
        }
    }
}

/// Changes what is under `dir` once, ignoring what fails: each file
/// written into and cut, every other name taken out, or moved, and a
/// file, a directory and a link made in each directory.
fn change_everything(dir: &NodeRef, seen: &mut BTreeSet<u64>) {
    let mut names = Vec::new();
    let _ = dir.list(0, &mut |_, name, _| {
        names.push(String::from(name));
        names.len() < 200
    });
    for (i, name) in names.iter().enumerate() {
        let Ok(node) = dir.lookup(name) else { continue };
        let stat = node.stat();
        if !seen.insert(stat.id) {
            continue;
        }
        match stat.kind {
            Kind::Directory => change_everything(&node, seen),
            Kind::File => {
                let _ = node.write(3000, &Slice(&pattern(i as u32, 9000)));
                let _ = node.set_size(5000);
            }
            _ => {}
        }
        if i % 2 == 1 {
            let _ = dir.remove(name);
        } else if i % 4 == 0 {
            let _ = dir.rename(name, &**dir, &format!("{name} moved"));
        }
    }
    if let Ok(node) = dir.create("new file", Kind::File, FILE) {
        let _ = node.write(0, &Slice(&pattern(5, 20_000)));
    }
    let _ = dir.create("new dir", Kind::Directory, DIR);
    let _ = dir.make_link("new link", "new file", FILE);
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(128))]

    /// Damaged metadata never panics the driver, reading or writing: the
    /// ntfs-3g volume, its records and index blocks with bytes flipped,
    /// past what their update sequences catch, is mounted to be read and
    /// to be written, and everything on it read, then written, cut,
    /// moved and taken out. What fails, fails.
    #[test]
    fn damaged_metadata_never_panics(
        flips in prop::collection::vec((any::<prop::sample::Index>(), 0usize..512, any::<u8>()), 1..24),
    ) {
        let (image, sectors) = damage_volume();
        let mut image = image.clone();
        for (sector, at, bits) in flips {
            image[sectors[sector.index(sectors.len())] * 512 + at] ^= bits | 1;
        }
        if let Ok(volume) = Volume::mount_in(512, &image, true) {
            read_everything(&volume.root(), &mut BTreeSet::default());
        }
        if let Ok(volume) = Volume::mount_in(512, &image, false) {
            let root = volume.root();
            read_everything(&root, &mut BTreeSet::default());
            change_everything(&root, &mut BTreeSet::default());
            let _ = volume.fs.sync();
        }
    }
}
