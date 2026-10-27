// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Host tests of writing Btrfs files through the VFS (ADR 0130): files,
//! directories and links are made, written, cut, renamed and removed on a
//! writable mount, synced, and then read back by a read-only mount and
//! checked, data checksums too, by Linux's `btrfs check`.

use std::collections::btree_map::Entry;
use std::collections::{BTreeMap, BTreeSet};
use std::format;
use std::path::Path;
use std::process::Command;
use std::string::{String, ToString};
use std::vec;
use std::vec::Vec;

use radix_block::ram::RamDisk;
use radix_block::{BlockDevice, DeviceRef};
use radix_vfs::mem::{self, KArc};
use radix_vfs::{Error, FileSystem, Kind, NodeRef, Slice, SliceMut};

use super::files::{contents, mount, noise};
use super::write::{
    CrashDisk, Op, SIZE, assert_checks, btrfs_check, image_of, mkfs_empty, mkfs_sized, writable,
};
use super::{XorShift, scratch, tool};
use crate::files::{ORPHAN_ITEM, object};
use crate::format::{Key, item, tree};
use crate::fs::BtrfsFs;
use crate::tree::find;

/// What a path of a filesystem holds.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Thing {
    File(Vec<u8>),
    Dir,
    Link(String),
}

/// Every path under a directory, and what it holds.
type Model = BTreeMap<String, Thing>;

/// The filesystem on `disk`, mounted to write.
fn mount_rw(disk: &KArc<RamDisk>) -> KArc<BtrfsFs> {
    let device: DeviceRef = disk.clone();
    BtrfsFs::mount_writable(device).unwrap()
}

/// A filesystem of `SIZE` bytes from `mkfs.btrfs args`, holding the files
/// of `dir`, with room to write.
fn mkfs_with(args: &[&str], dir: &Path) -> Vec<u8> {
    let path = scratch("rootdir");
    let file = std::fs::File::create(&path).unwrap();
    file.set_len(SIZE).unwrap();
    let out = Command::new(tool("mkfs.btrfs"))
        .args(["-q", "-f"])
        .args(args)
        .arg("--rootdir")
        .arg(dir)
        .arg(&path)
        .stdin(std::process::Stdio::null())
        .output()
        .unwrap();
    assert!(out.status.success(), "mkfs.btrfs: {}", String::from_utf8_lossy(&out.stderr));
    let image = std::fs::read(&path).unwrap();
    let _ = std::fs::remove_file(&path);
    image
}

/// The node at `path` ("/a/b") under `root`.
fn at(root: &NodeRef, path: &str) -> Result<NodeRef, Error> {
    let mut node = root.clone();
    for name in path.split('/').filter(|n| !n.is_empty()) {
        node = node.lookup(name)?;
    }
    Ok(node)
}

/// The directory holding `path`, and the last name of it.
fn parent<'p>(root: &NodeRef, path: &'p str) -> (NodeRef, &'p str) {
    let (dir, name) = path.rsplit_once('/').unwrap();
    (at(root, dir).unwrap(), name)
}

/// What every path under `root` holds.
fn snapshot(root: &NodeRef) -> Model {
    let mut out = Model::new();
    walk(root, "", &mut out);
    out
}

fn walk(dir: &NodeRef, prefix: &str, out: &mut Model) {
    let mut names = Vec::new();
    dir.list(0, &mut |_, name, kind| {
        names.push((name.to_string(), kind));
        true
    })
    .unwrap();
    for (name, kind) in names {
        let path = format!("{prefix}/{name}");
        let node = dir.lookup(&name).unwrap();
        assert_eq!(node.kind(), kind, "{path}");
        match kind {
            Kind::Directory => {
                out.insert(path.clone(), Thing::Dir);
                walk(&node, &path, out);
            }
            Kind::File => {
                let data = contents(&*node, 1 << 16).unwrap();
                assert_eq!(node.stat().size, data.len() as u64, "{path}");
                out.insert(path, Thing::File(data));
            }
            Kind::Symlink => {
                let mut buf = vec![0; 4096];
                let n = node.read_link(&mut SliceMut(&mut buf)).unwrap();
                out.insert(path, Thing::Link(String::from_utf8(buf[..n].to_vec()).unwrap()));
            }
            _ => {}
        }
    }
}

/// Writes `data` at `offset` of `file`.
fn put(file: &NodeRef, offset: u64, data: &[u8]) {
    assert_eq!(file.write(offset, &Slice(data)).unwrap(), data.len());
}

/// Writes `data` at `offset` of the file in `model`.
fn put_model(model: &mut Model, path: &str, offset: u64, data: &[u8]) {
    let Some(Thing::File(bytes)) = model.get_mut(path) else { panic!("{path} is no file") };
    let end = offset as usize + data.len();
    if bytes.len() < end {
        bytes.resize(end, 0);
    }
    bytes[offset as usize..end].copy_from_slice(data);
}

/// Syncs `fs` on `disk`, checks it with `btrfs check`, and checks a
/// read-only mount of it reads `model`.
fn synced(fs: &BtrfsFs, disk: &RamDisk, model: &Model, what: &str) {
    fs.sync().unwrap_or_else(|e| panic!("{what}: sync: {e:?}"));
    let image = image_of(disk, (disk.info().sectors * radix_block::SECTOR) as usize);
    let (whole, text) = btrfs_check(&image);
    assert!(whole, "{what}: btrfs check failed:\n{text}");
    let ro = mount(&image).unwrap();
    let read = snapshot(&ro.root());
    assert_eq!(read.keys().collect::<Vec<_>>(), model.keys().collect::<Vec<_>>(), "{what}");
    for (path, thing) in model {
        assert!(read[path] == *thing, "{what}: {path} differs");
    }
}

#[test]
fn files_written_read_back_and_pass_btrfs_check() {
    let disk = writable(&mkfs_empty(&[]));
    let fs = mount_rw(&disk);
    assert!(!fs.read_only());
    let free = fs.info().free;
    let root = fs.root();
    let mut model = Model::new();
    root.create("docs", Kind::Directory).unwrap();
    root.create("docs/x", Kind::File).map(|_| ()).unwrap_err();
    let docs = root.lookup("docs").unwrap();
    model.insert("/docs".into(), Thing::Dir);
    let deep = docs.create("deeper", Kind::Directory).unwrap();
    model.insert("/docs/deeper".into(), Thing::Dir);
    // Sizes around the sector and the page cache's runs of 32 pages.
    for (n, len) in
        [0, 1, 100, 4095, 4096, 4097, 131_072, 131_073, 300_000, 1_048_579].into_iter().enumerate()
    {
        let dir = if n % 2 == 0 { &docs } else { &deep };
        let name = format!("size-{len}");
        let file = dir.create(&name, Kind::File).unwrap();
        let data = noise(n as u64 + 1, len);
        put(&file, 0, &data);
        assert_eq!(file.stat().size, len as u64);
        let path =
            if n % 2 == 0 { format!("/docs/{name}") } else { format!("/docs/deeper/{name}") };
        model.insert(path, Thing::File(data));
    }
    root.make_link("link", "docs/size-100").unwrap();
    model.insert("/link".into(), Thing::Link("docs/size-100".into()));
    let long = "x/".repeat(1000);
    root.make_link("long-link", &long).unwrap();
    model.insert("/long-link".into(), Thing::Link(long));
    assert_eq!(root.create("docs", Kind::File).map(|_| ()), Err(Error::Exists));
    assert_eq!(root.create("pipe", Kind::Pipe).map(|_| ()), Err(Error::NotSupported));
    assert_eq!(root.create("", Kind::File).map(|_| ()), Err(Error::BadName));
    assert_eq!(snapshot(&root), model, "before the sync");
    // The pages written take their room as they change, as on Linux.
    let dirty = fs.info().free;
    assert!(dirty < free - 1_400_000 / 4096, "the data takes space");
    synced(&fs, &disk, &model, "made");
    assert_eq!(fs.info().free, dirty, "and keeps it once written");
    // Each file's bytes are counted as Linux counts them.
    let file = at(&root, "/docs/deeper/size-4097").unwrap();
    assert_eq!(file.stat().allocated, 8192);
}

#[test]
fn overwrites_cuts_and_holes_keep_every_byte() {
    let disk = writable(&mkfs_empty(&[]));
    let fs = mount_rw(&disk);
    let root = fs.root();
    let mut model = Model::new();
    let file = root.create("f", Kind::File).unwrap();
    model.insert("/f".into(), Thing::File(Vec::new()));
    let step = |model: &mut Model, offset: u64, data: Vec<u8>, what: &str| {
        put(&file, offset, &data);
        put_model(model, "/f", offset, &data);
        synced(&fs, &disk, model, what);
    };
    step(&mut model, 0, noise(1, 1 << 20), "written");
    step(&mut model, 100_000, noise(2, 10_000), "the middle of one extent");
    step(&mut model, 4096 * 31, noise(3, 4096 * 3), "across two extents");
    step(&mut model, 0, noise(4, 5), "the first bytes");
    // Cut inside a sector, and inside an extent.
    file.set_size(500_001).unwrap();
    if let Some(Thing::File(bytes)) = model.get_mut("/f") {
        bytes.truncate(500_001);
    }
    synced(&fs, &disk, &model, "cut");
    // Grown with a hole, then written past it.
    file.set_size(2 << 20).unwrap();
    if let Some(Thing::File(bytes)) = model.get_mut("/f") {
        bytes.resize(2 << 20, 0);
    }
    step(&mut model, 3 << 19, noise(5, 7000), "past a hole");
    step(&mut model, 2 << 20, noise(6, 12_345), "appended");
    file.set_size(0).unwrap();
    model.insert("/f".into(), Thing::File(Vec::new()));
    synced(&fs, &disk, &model, "emptied");
    step(&mut model, 0, noise(7, 9000), "written again");
}

#[test]
fn removing_and_renaming_keep_directories_whole() {
    let disk = writable(&mkfs_empty(&["--nodesize", "4096"]));
    let fs = mount_rw(&disk);
    let root = fs.root();
    let mut model = Model::new();
    for dir in ["/a", "/a/b", "/c", "/empty"] {
        let (at_dir, name) = parent(&root, dir);
        at_dir.create(name, Kind::Directory).unwrap();
        model.insert(dir.into(), Thing::Dir);
    }
    for (n, path) in ["/a/one", "/a/b/two", "/c/three", "/four"].into_iter().enumerate() {
        let (dir, name) = parent(&root, path);
        let file = dir.create(name, Kind::File).unwrap();
        let data = noise(n as u64 + 10, 5000 * (n + 1));
        put(&file, 0, &data);
        model.insert(path.into(), Thing::File(data));
    }
    // Many names in one directory split its leaves (4 KiB nodes).
    let c = root.lookup("c").unwrap();
    for n in 0..300 {
        c.create(&format!("name-{n:03}-{}", "y".repeat(30)), Kind::File).unwrap();
        model.insert(format!("/c/name-{n:03}-{}", "y".repeat(30)), Thing::File(Vec::new()));
    }
    synced(&fs, &disk, &model, "made");
    let a = root.lookup("a").unwrap();
    assert_eq!(root.remove("a"), Err(Error::NotEmpty));
    assert_eq!(root.remove("missing"), Err(Error::NotFound));
    root.remove("empty").unwrap();
    model.remove("/empty");
    root.remove("four").unwrap();
    model.remove("/four");
    for n in (0..300).step_by(2) {
        c.remove(&format!("name-{n:03}-{}", "y".repeat(30))).unwrap();
        model.remove(&format!("/c/name-{n:03}-{}", "y".repeat(30)));
    }
    synced(&fs, &disk, &model, "removed");
    // Within a directory, across, over a file, and a directory over an
    // empty one.
    a.rename("one", &*a, "uno").unwrap();
    let one = model.remove("/a/one").unwrap();
    model.insert("/a/uno".into(), one);
    a.rename("uno", &*c, "three").unwrap();
    let uno = model.remove("/a/uno").unwrap();
    model.insert("/c/three".into(), uno);
    root.create("d", Kind::Directory).unwrap();
    model.insert("/d".into(), Thing::Dir);
    let b = a.lookup("b").unwrap();
    assert_eq!(a.rename("b", &*b, "inside"), Err(Error::BadName));
    assert_eq!(root.rename("a", &*b, "inside"), Err(Error::BadName));
    assert_eq!(c.rename("three", &*root, "d"), Err(Error::IsDirectory));
    assert_eq!(root.rename("d", &*c, "three"), Err(Error::NotDirectory));
    assert_eq!(root.rename("c", &*root, "a"), Err(Error::NotEmpty));
    a.rename("b", &*root, "d").unwrap();
    let moved: Vec<(String, Thing)> = model
        .range("/a/b".to_string()..)
        .take_while(|(k, _)| k.starts_with("/a/b"))
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();
    for (path, _) in &moved {
        model.remove(path);
    }
    for (path, thing) in moved {
        model.insert(path.replacen("/a/b", "/d", 1), thing);
    }
    // The moved directory's files are found under its new name.
    assert_eq!(contents(&*at(&root, "/d/two").unwrap(), 4096).unwrap().len(), 10_000);
    synced(&fs, &disk, &model, "renamed");
}

#[test]
fn files_linux_made_change_like_any_other() {
    let dir = scratch("linux");
    std::fs::create_dir_all(dir.join("sub")).unwrap();
    let mut model = Model::new();
    model.insert("/sub".into(), Thing::Dir);
    for (n, len) in [100usize, 2047, 3000, 4097, 200_000].into_iter().enumerate() {
        let data = noise(n as u64 + 20, len);
        std::fs::write(dir.join(format!("size-{len}")), &data).unwrap();
        model.insert(format!("/size-{len}"), Thing::File(data));
    }
    let text = "Dukovo writes Btrfs.\n".repeat(20_000).into_bytes();
    std::fs::write(dir.join("sub/text"), &text).unwrap();
    model.insert("/sub/text".into(), Thing::File(text.clone()));
    std::fs::write(dir.join("hard-a"), b"two names").unwrap();
    std::fs::hard_link(dir.join("hard-a"), dir.join("sub/hard-b")).unwrap();
    model.insert("/hard-a".into(), Thing::File(b"two names".to_vec()));
    model.insert("/sub/hard-b".into(), Thing::File(b"two names".to_vec()));
    for args in [&["--compress", "zstd"][..], &["--compress", "zlib"], &[]] {
        let image = mkfs_with(args, &dir);
        assert_checks(&image);
        let disk = writable(&image);
        let fs = mount_rw(&disk);
        let root = fs.root();
        let mut model = model.clone();
        let mut change = |path: &str, offset: u64, data: &[u8]| {
            put(&at(&root, path).unwrap(), offset, data);
            put_model(&mut model, path, offset, data);
        };
        // Inline files written past their start, and in place.
        change("/size-100", 50, b"over the inline bytes");
        change("/size-2047", 2047, &noise(30, 3000));
        change("/size-3000", 0, b"start");
        // Compressed extents cut by a write in their middle.
        change("/sub/text", 70_000, &noise(31, 5000));
        change("/size-200000", 4096, &noise(32, 100));
        let file = at(&root, "/size-4097").unwrap();
        file.set_size(10).unwrap();
        if let Some(Thing::File(bytes)) = model.get_mut("/size-4097") {
            bytes.truncate(10);
        }
        // One of two names goes; the other keeps the file.
        root.remove("hard-a").unwrap();
        model.remove("/hard-a");
        let sub = root.lookup("sub").unwrap();
        sub.rename("hard-b", &*root, "hard-c").unwrap();
        let b = model.remove("/sub/hard-b").unwrap();
        model.insert("/hard-c".into(), b);
        synced(&fs, &disk, &model, &format!("{args:?}"));
        sub.remove("text").unwrap();
        model.remove("/sub/text");
        synced(&fs, &disk, &model, &format!("{args:?}, text removed"));
    }
}

/// Whether inode `ino` of the top subvolume has an item, and an orphan
/// item, in `image`.
fn inode_items(image: &[u8], ino: u64) -> (bool, bool) {
    let ro = mount(image).unwrap();
    let volume = ro.volume();
    let fs = volume.tree(tree::FS).unwrap().unwrap();
    let inode = find(volume, &fs, &Key::new(ino, item::INODE_ITEM, 0)).unwrap().is_some();
    let orphan = find(volume, &fs, &Key::new(object::ORPHAN, ORPHAN_ITEM, ino)).unwrap().is_some();
    (inode, orphan)
}

#[test]
fn a_file_removed_while_open_goes_when_it_closes_or_at_the_next_mount() {
    let disk = writable(&mkfs_empty(&[]));
    let fs = mount_rw(&disk);
    let root = fs.root();
    let file = root.create("open", Kind::File).unwrap();
    let data = noise(40, 70_000);
    put(&file, 0, &data);
    let ino = file.stat().id;
    fs.sync().unwrap();
    root.remove("open").unwrap();
    assert_eq!(root.lookup("open").map(|_| ()), Err(Error::NotFound));
    // It still reads, and writes, while open.
    assert_eq!(contents(&*file, 4096).unwrap(), data);
    put(&file, 0, b"still mine");
    fs.sync().unwrap();
    let image = image_of(&disk, SIZE as usize);
    assert_checks(&image);
    assert_eq!(inode_items(&image, ino), (true, true));
    drop(file);
    fs.sync().unwrap();
    let image = image_of(&disk, SIZE as usize);
    assert_checks(&image);
    assert_eq!(inode_items(&image, ino), (false, false));

    // Removed while open, and the power goes before it closes: the next
    // writable mount frees it.
    let file = root.create("crash", Kind::File).unwrap();
    put(&file, 0, &data);
    let ino = file.stat().id;
    root.remove("crash").unwrap();
    fs.sync().unwrap();
    let image = image_of(&disk, SIZE as usize);
    assert_eq!(inode_items(&image, ino), (true, true));
    let after = writable(&image);
    let again = mount_rw(&after);
    again.sync().unwrap();
    let image = image_of(&after, SIZE as usize);
    assert_checks(&image);
    assert_eq!(inode_items(&image, ino), (false, false));
    drop(file);
}

#[test]
fn files_written_closed_and_removed_before_a_sync_give_their_room_back_at_once() {
    // Half again what the filesystem holds, made and removed with no sync
    // between: each file's items and room go with its last name, as on
    // Linux, not at the sync.
    let disk = writable(&mkfs_empty(&[]));
    let fs = mount_rw(&disk);
    let root = fs.root();
    let data = noise(41, 64 * 1024);
    let mut gone = Vec::new();
    for n in 0..3000 {
        let file = root.create("temp", Kind::File).unwrap();
        put(&file, 0, &data);
        if n % 500 == 0 {
            gone.push(file.stat().id);
        }
        drop(file);
        root.remove("temp").unwrap();
    }
    fs.sync().unwrap();
    let image = image_of(&disk, SIZE as usize);
    assert_checks(&image);
    for ino in gone {
        assert_eq!(inode_items(&image, ino), (false, false), "{ino}");
    }
}

#[test]
fn a_power_cut_anywhere_keeps_the_last_sync() {
    let image = mkfs_empty(&[]);
    let disk = mem::arc(CrashDisk::new(&image)).unwrap();
    let mut states: Vec<Model> = vec![Model::new()];
    {
        let device: DeviceRef = disk.clone();
        let fs = BtrfsFs::mount_writable(device).unwrap();
        let root = fs.root();
        let mut model = Model::new();
        let mut random = XorShift(0x5eed);
        for round in 0..4 {
            let dir = format!("round-{round}");
            root.create(&dir, Kind::Directory).unwrap();
            model.insert(format!("/{dir}"), Thing::Dir);
            let d = root.lookup(&dir).unwrap();
            for n in 0..6 {
                let file = d.create(&format!("f{n}"), Kind::File).unwrap();
                let data = noise(random.next(), random.below(200_000));
                put(&file, 0, &data);
                model.insert(format!("/{dir}/f{n}"), Thing::File(data));
            }
            if round > 0 {
                // Earlier rounds' files change, go, and move.
                let prev = format!("/round-{}", round - 1);
                let data = noise(random.next(), 30_000);
                put(&at(&root, &format!("{prev}/f0")).unwrap(), 10_000, &data);
                put_model(&mut model, &format!("{prev}/f0"), 10_000, &data);
                at(&root, &prev).unwrap().remove("f1").unwrap();
                model.remove(&format!("{prev}/f1"));
                at(&root, &prev).unwrap().rename("f2", &*d, "moved").unwrap();
                let moved = model.remove(&format!("{prev}/f2")).unwrap();
                model.insert(format!("/{dir}/moved"), moved);
                at(&root, &format!("{prev}/f3")).unwrap().set_size(1000).unwrap();
                if let Some(Thing::File(bytes)) = model.get_mut(&format!("{prev}/f3")) {
                    bytes.truncate(1000);
                }
            }
            fs.sync().unwrap();
            states.push(model.clone());
        }
    }
    let ops = disk.ops();
    let mut cuts: BTreeSet<usize> = BTreeSet::new();
    let mut random = XorShift(0xfeed);
    for (at, op) in ops.iter().enumerate() {
        if matches!(op, Op::Flush | Op::Through(..)) {
            cuts.extend([at, at + 1]);
        }
    }
    for _ in 0..12 {
        cuts.insert(random.below(ops.len() + 1));
    }
    for cut in cuts {
        let (medium, durable) = disk.cut(cut, || random.next() & 1 == 0);
        let (whole, text) = btrfs_check(&medium);
        assert!(whole, "a cut before op {cut} of {}: btrfs check failed:\n{text}", ops.len());
        let ro = mount(&medium).unwrap();
        assert!(
            snapshot(&ro.root()) == states[durable],
            "a cut before op {cut} of {} holds sync {durable}",
            ops.len()
        );
    }
}

/// The storage boot test's churn (`storage churn`): a file written,
/// synced, renamed, its directory synced, and the file of eight rounds
/// before removed and the directory synced again, with no sync of the
/// whole filesystem between, as on a fast machine. A power cut anywhere
/// keeps the last commit.
#[test]
fn a_power_cut_in_a_churn_of_synced_files_keeps_the_last_commit() {
    let image = mkfs_empty(&[]);
    let disk = mem::arc(CrashDisk::new(&image)).unwrap();
    let throughs =
        |disk: &CrashDisk| disk.ops().iter().filter(|op| matches!(op, Op::Through(..))).count();
    let mut states: BTreeMap<usize, Model> = BTreeMap::new();
    states.insert(0, Model::new());
    {
        let device: DeviceRef = disk.clone();
        let fs = BtrfsFs::mount_writable(device).unwrap();
        let root = fs.root();
        let mut model = Model::new();
        let mut note = |model: &Model| {
            states.insert(throughs(&disk), model.clone());
        };
        for round in 0..48u64 {
            let (temp, file) = (format!("temp-{round:04}"), format!("file-{round:04}"));
            let data = noise(round, (round as usize * 40_009) % (192 << 10) + 1);
            let made = root.create(&temp, Kind::File).unwrap();
            put(&made, 0, &data);
            model.insert(format!("/{temp}"), Thing::File(data));
            made.sync().unwrap();
            note(&model);
            drop(made);
            root.rename(&temp, &*root, &file).unwrap();
            let moved = model.remove(&format!("/{temp}")).unwrap();
            model.insert(format!("/{file}"), moved);
            root.sync().unwrap();
            note(&model);
            if let Some(old) = round.checked_sub(8) {
                root.remove(&format!("file-{old:04}")).unwrap();
                model.remove(&format!("/file-{old:04}"));
                root.sync().unwrap();
                note(&model);
            }
        }
    }
    let ops = disk.ops();
    let mut cuts: BTreeSet<usize> = BTreeSet::new();
    let mut random = XorShift(0xc0ffee);
    for (at, op) in ops.iter().enumerate() {
        if matches!(op, Op::Flush | Op::Through(..)) {
            cuts.extend([at, at + 1]);
        }
    }
    for _ in 0..24 {
        cuts.insert(random.below(ops.len() + 1));
    }
    for cut in cuts {
        let (medium, durable) = disk.cut(cut, || random.next() & 1 == 0);
        let (whole, text) = btrfs_check(&medium);
        assert!(whole, "a cut before op {cut} of {}: btrfs check failed:\n{text}", ops.len());
        let ro = mount(&medium).unwrap();
        let (_, want) = states.range(..=durable).next_back().unwrap();
        let read = snapshot(&ro.root());
        assert_eq!(
            read.keys().collect::<Vec<_>>(),
            want.keys().collect::<Vec<_>>(),
            "a cut before op {cut} of {}, {durable} commits",
            ops.len()
        );
        assert!(read == *want, "a cut before op {cut} of {}: the files differ", ops.len());
    }
}

#[test]
fn random_changes_match_a_model() {
    for args in [&[][..], &["--nodesize", "4096", "--csum", "sha256"]] {
        let disk = writable(&mkfs_empty(args));
        let fs = mount_rw(&disk);
        let root = fs.root();
        let mut model = Model::new();
        let mut random = XorShift(0xd0c0 + args.len() as u64);
        let dirs = ["", "/x", "/y"];
        for dir in &dirs[1..] {
            root.create(&dir[1..], Kind::Directory).unwrap();
            model.insert(dir.to_string(), Thing::Dir);
        }
        let files = |model: &Model| -> Vec<String> {
            model
                .iter()
                .filter(|(_, t)| matches!(t, Thing::File(_)))
                .map(|(k, _)| k.clone())
                .collect()
        };
        for step in 0..600 {
            let names = files(&model);
            match random.below(10) {
                0 | 1 => {
                    let path = format!("{}/n{}", dirs[random.below(3)], random.below(40));
                    if let Entry::Vacant(slot) = model.entry(path) {
                        let (dir, name) = parent(&root, slot.key());
                        dir.create(name, Kind::File).unwrap();
                        slot.insert(Thing::File(Vec::new()));
                    }
                }
                2..=5 if !names.is_empty() => {
                    let path = &names[random.below(names.len())];
                    let Some(Thing::File(bytes)) = model.get(path) else { unreachable!() };
                    let offset = random.below(bytes.len() + 50_000) as u64;
                    let data = noise(random.next(), random.below(150_000));
                    put(&at(&root, path).unwrap(), offset, &data);
                    put_model(&mut model, path, offset, &data);
                }
                6 if !names.is_empty() => {
                    let path = &names[random.below(names.len())];
                    let Some(Thing::File(bytes)) = model.get_mut(path) else { unreachable!() };
                    let size = random.below(bytes.len() + 10_000);
                    bytes.resize(size, 0);
                    at(&root, path).unwrap().set_size(size as u64).unwrap();
                }
                7 if !names.is_empty() => {
                    let path = names[random.below(names.len())].clone();
                    let (dir, name) = parent(&root, &path);
                    dir.remove(name).unwrap();
                    model.remove(&path);
                }
                8 | 9 if !names.is_empty() => {
                    let from = names[random.below(names.len())].clone();
                    let to = format!("{}/n{}", dirs[random.below(3)], random.below(40));
                    if !matches!(model.get(&to), Some(Thing::Dir)) {
                        let (a, an) = parent(&root, &from);
                        let (b, bn) = parent(&root, &to);
                        a.rename(an, &*b, bn).unwrap();
                        let thing = model.remove(&from).unwrap();
                        model.insert(to, thing);
                    }
                }
                _ => {}
            }
            if step % 100 == 99 {
                synced(&fs, &disk, &model, &format!("{args:?}, step {step}"));
            }
        }
        assert_eq!(snapshot(&root), model);
    }
}

/// Fills a filesystem of `size` bytes from `mkfs.btrfs args` with
/// `megs` MiB of files, past its first data chunk; removes half of them;
/// and fills their room again through a new mount.
fn fill(args: &[&str], size: u64, megs: u64) {
    let disk = writable(&mkfs_sized(size, args));
    let chunks = |disk: &KArc<RamDisk>| {
        let device: DeviceRef = disk.clone();
        crate::Volume::open(device).unwrap().with_chunks(|map| map.chunks().len())
    };
    let before = chunks(&disk);
    let mut model = Model::new();
    {
        let fs = mount_rw(&disk);
        let root = fs.root();
        for n in 0..megs / 4 {
            let file = root.create(&format!("f{n}"), Kind::File).unwrap();
            let data = noise(n, 4 << 20);
            put(&file, 0, &data);
            model.insert(format!("/f{n}"), Thing::File(data));
        }
        synced(&fs, &disk, &model, "filled");
        for n in (0..megs / 4).step_by(2) {
            root.remove(&format!("f{n}")).unwrap();
            model.remove(&format!("/f{n}"));
        }
        synced(&fs, &disk, &model, "half removed");
    }
    assert!(chunks(&disk) > before, "no chunk was made");
    // A new mount finds the chunks, and fills the room the files left.
    let fs = mount_rw(&disk);
    let root = fs.root();
    for n in 100..100 + megs / 8 {
        let file = root.create(&format!("f{n}"), Kind::File).unwrap();
        let data = noise(n, 4 << 20);
        put(&file, 0, &data);
        model.insert(format!("/f{n}"), Thing::File(data));
    }
    synced(&fs, &disk, &model, "refilled");
}

#[test]
fn files_past_the_first_data_chunk_go_to_new_ones() {
    // mkfs makes a data chunk of 8 MiB; new ones are 16 MiB.
    fill(&[], SIZE, 28);
}

#[test]
fn mixed_chunks_hold_the_tree_blocks_of_the_files_past_the_first() {
    fill(&["--mixed"], SIZE, 32);
}

#[test]
fn files_past_the_first_dup_data_chunk_go_to_new_dup_ones() {
    // mkfs makes a data chunk of 64 MiB, kept twice.
    fill(&["-d", "dup"], 256 << 20, 72);
}

/// Writes 1 MiB files to a filesystem, numbered from `first`, until a
/// write says it is full; returns how many were written whole.
fn fill_up(root: &NodeRef, model: &mut Model, first: usize) -> usize {
    for n in first.. {
        let name = format!("full-{n}");
        let file = match root.create(&name, Kind::File) {
            Ok(file) => file,
            Err(Error::NoSpace) => return n - first,
            Err(e) => panic!("create {name}: {e:?}"),
        };
        let data = noise(n as u64 + 1000, 1 << 20);
        let path = format!("/{name}");
        match file.write(0, &Slice(&data)) {
            Ok(len) if len == data.len() => {
                model.insert(path, Thing::File(data));
            }
            // The write stops where the room did; what it wrote stays.
            Ok(len) => {
                model.insert(path, Thing::File(data[..len].to_vec()));
                return n - first;
            }
            Err(Error::NoSpace) => {
                model.insert(path, Thing::File(Vec::new()));
                return n - first;
            }
            Err(e) => panic!("write {name}: {e:?}"),
        }
    }
    unreachable!()
}

/// Fills a filesystem from `mkfs.btrfs args` until a write says it is
/// full, then empties half of it and fills it again.
fn full(args: &[&str]) {
    let disk = writable(&mkfs_empty(args));
    let fs = mount_rw(&disk);
    let root = fs.root();
    let mut model = Model::new();
    let written = fill_up(&root, &mut model, 0);
    // Most of the device holds data: 39 MiB of it is free, past
    // mkfs's chunks.
    assert!(written >= 30, "{args:?}: {written} MiB");
    // What the writes took, write-back has room for.
    synced(&fs, &disk, &model, &format!("{args:?} full"));
    // Nothing is left but, on a mixed filesystem, what metadata keeps.
    let free = fs.info().free * 4096;
    assert!(free < 4 << 20, "{args:?}: {free} bytes free");
    // Removing files works on a full filesystem, and makes room.
    for n in 0..written / 2 {
        root.remove(&format!("full-{n}")).unwrap();
        model.remove(&format!("/full-{n}"));
    }
    synced(&fs, &disk, &model, &format!("{args:?} emptied"));
    let again = fill_up(&root, &mut model, 1000);
    assert!(again >= written / 2 - 2, "{args:?}: {again} MiB after {written}");
    synced(&fs, &disk, &model, &format!("{args:?} full again"));
}

#[test]
fn a_full_filesystem_fails_the_write_not_the_sync_and_empties() {
    full(&[]);
}

#[test]
fn a_full_mixed_filesystem_keeps_room_for_its_metadata() {
    full(&["--mixed"]);
}
