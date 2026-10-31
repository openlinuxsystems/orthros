// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Host tests of the ISO 9660 driver, against discs that xorriso makes
//! (`apt install xorriso`; `ob doctor` checks for it): with Rock Ridge and
//! Joliet, with Joliet alone, and plain, on disks of 512-byte and of
//! 2048-byte blocks, as a disc image on a USB stick and a CD are.

use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::string::{String, ToString};
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, SystemTime};
use std::vec::Vec;
use std::{format, vec};

use radix_block::DeviceRef;
use radix_block::ram::RamDisk;
use radix_vfs::mem::{self, KArc};
use radix_vfs::{Error, FileSystem, Kind, NodeRef, SliceMut};

use crate::IsoFs;
use crate::format::{self, NameBuf};

/// A scratch path of its own for each use.
fn scratch(what: &str) -> PathBuf {
    static NEXT: AtomicU32 = AtomicU32::new(0);
    let n = NEXT.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!("radix-iso9660-{}-{what}-{n}", std::process::id()))
}

/// The disc xorriso makes of `tree`, with mkisofs's `args`.
fn xorriso(tree: &Path, args: &[&str]) -> Vec<u8> {
    let out = scratch("disc.iso");
    let made = Command::new("xorriso")
        .args(["-as", "mkisofs", "-quiet"])
        .args(args)
        .arg("-o")
        .arg(&out)
        .arg(tree)
        .output()
        .expect("xorriso is missing: apt install xorriso");
    assert!(made.status.success(), "xorriso: {}", String::from_utf8_lossy(&made.stderr));
    let image = std::fs::read(&out).unwrap();
    let _ = std::fs::remove_file(&out);
    image
}

/// Bytes that are not the same from one place to the next.
fn pattern(len: usize, seed: u32) -> Vec<u8> {
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

/// 2026-10-31 12:00:00 UTC.
const WHEN: u64 = 1_793_448_000;

/// The files of the trees the tests make: (path, size).
const FILES: [(&str, usize); 6] = [
    ("empty", 0),
    ("one", 1),
    ("Mixed Case.txt", 2048),
    ("dir/odd", 5000),
    ("dir/sub/big", 300 * 1024 + 7),
    ("dir/sub/whole", 1 << 20),
];

/// A tree of [`FILES`], with links and a long name.
fn tree() -> PathBuf {
    let root = scratch("tree");
    std::fs::create_dir_all(root.join("dir/sub")).unwrap();
    for (i, (path, size)) in FILES.iter().enumerate() {
        std::fs::write(root.join(path), pattern(*size, i as u32)).unwrap();
    }
    let long = "l".repeat(200);
    std::fs::write(root.join(&long), b"long").unwrap();
    symlink("dir/sub/big", root.join("relative")).unwrap();
    symlink("/system/bin/sh", root.join("absolute")).unwrap();
    symlink("../one", root.join("dir/up")).unwrap();
    std::fs::set_permissions(root.join("one"), std::fs::Permissions::from_mode(0o750)).unwrap();
    let when = SystemTime::UNIX_EPOCH + Duration::from_secs(WHEN);
    std::fs::File::options()
        .write(true)
        .open(root.join("dir/odd"))
        .unwrap()
        .set_modified(when)
        .unwrap();
    root
}

/// The volume in `image`, on a disk of `block`-byte blocks.
fn mount(image: &[u8], block: u32) -> Result<KArc<IsoFs>, Error> {
    let disk = mem::arc(RamDisk::with_blocks(image.len() as u64 / 512, false, block)).unwrap();
    disk.load(0, image).unwrap();
    let device: DeviceRef = disk;
    IsoFs::mount(device, true)
}

/// The node at `path` under `dir`.
fn walk(dir: &NodeRef, path: &str) -> Result<NodeRef, Error> {
    let mut node = dir.clone();
    for name in path.split('/').filter(|n| !n.is_empty()) {
        node = node.lookup(name)?;
    }
    Ok(node)
}

/// A file's bytes, read `chunk` bytes at a time.
fn read_all(node: &NodeRef, chunk: usize) -> Vec<u8> {
    let mut all = Vec::new();
    let mut buf = vec![0u8; chunk];
    loop {
        let n = node.read(all.len() as u64, &mut SliceMut(&mut buf)).unwrap();
        if n == 0 {
            return all;
        }
        all.extend_from_slice(&buf[..n]);
    }
}

/// A directory's entries, sorted.
fn names(dir: &NodeRef) -> Vec<(String, Kind)> {
    let mut all = Vec::new();
    dir.list(0, &mut |_, name, kind| {
        all.push((name.to_string(), kind));
        true
    })
    .unwrap();
    all.sort_by(|a, b| a.0.cmp(&b.0));
    all
}

/// A symbolic link's target.
fn target(node: &NodeRef) -> String {
    String::from_utf8(radix_vfs::link_target(&**node).unwrap().clone()).unwrap()
}

#[test]
fn rock_ridge_has_posix_names_modes_links_and_times() {
    let tree = tree();
    let image = xorriso(&tree, &["-R", "-J", "-V", "RADIX_TEST"]);
    for block in [512, 2048] {
        let fs = mount(&image, block).unwrap();
        assert_eq!(fs.kind(), radix_vfs::FsKind::Iso9660);
        assert!(fs.read_only());
        let mut label = String::new();
        fs.label(&mut label).unwrap();
        assert_eq!(label, "RADIX_TEST");
        let root = fs.root();
        let long = "l".repeat(200);
        let mut want: Vec<(String, Kind)> = [
            ("Mixed Case.txt", Kind::File),
            ("absolute", Kind::Symlink),
            ("dir", Kind::Directory),
            ("empty", Kind::File),
            ("one", Kind::File),
            ("relative", Kind::Symlink),
            (long.as_str(), Kind::File),
        ]
        .iter()
        .map(|(n, k)| (n.to_string(), *k))
        .collect();
        want.sort_by(|a, b| a.0.cmp(&b.0));
        assert_eq!(names(&root), want);
        for (i, (path, size)) in FILES.iter().enumerate() {
            let node = walk(&root, path).unwrap();
            assert_eq!(node.stat().size, *size as u64, "{path}");
            assert_eq!(read_all(&node, 4096), pattern(*size, i as u32), "{path}");
            // Reads that start and end in the middle of pages.
            assert_eq!(read_all(&node, 1000), pattern(*size, i as u32), "{path}");
        }
        assert_eq!(read_all(&walk(&root, &long).unwrap(), 64), b"long");
        assert_eq!(target(&walk(&root, "relative").unwrap()), "dir/sub/big");
        assert_eq!(target(&walk(&root, "absolute").unwrap()), "/system/bin/sh");
        assert_eq!(target(&walk(&root, "dir/up").unwrap()), "../one");
        assert_eq!(walk(&root, "relative").unwrap().stat().size, 11);
        assert_eq!(walk(&root, "one").unwrap().access().mode, 0o750);
        assert_eq!(walk(&root, "dir").unwrap().stat().kind, Kind::Directory);
        let odd = walk(&root, "dir/odd").unwrap().stat();
        assert_eq!(odd.modified / 1_000_000_000, WHEN);
        assert_eq!(root.lookup("missing").err(), Some(Error::NotFound));
        assert_eq!(root.lookup("mixed case.txt").err(), Some(Error::NotFound));
        assert_eq!(walk(&root, "one/x").err(), Some(Error::NotDirectory));
        assert_eq!(root.read(0, &mut SliceMut(&mut [0; 8])).err(), Some(Error::IsDirectory));
        // One node for each file while it is in use.
        let a = walk(&root, "dir/sub/big").unwrap();
        let b = walk(&root, "dir/sub/big").unwrap();
        assert!(core::ptr::addr_eq(&raw const *a, &raw const *b));
        assert_eq!(a.write(0, &radix_vfs::Slice(b"x")).err(), Some(Error::ReadOnly));
    }
    std::fs::remove_dir_all(tree).unwrap();
}

#[test]
fn joliet_keeps_case_without_rock_ridge() {
    let tree = tree();
    std::fs::remove_file(tree.join("l".repeat(200))).unwrap();
    let image = xorriso(&tree, &["--norock", "-J", "-V", "Joliet"]);
    let fs = mount(&image, 512).unwrap();
    let root = fs.root();
    let mut label = String::new();
    fs.label(&mut label).unwrap();
    assert_eq!(label, "Joliet");
    let found = names(&root);
    assert!(found.contains(&("Mixed Case.txt".to_string(), Kind::File)), "{found:?}");
    for (i, (path, size)) in FILES.iter().enumerate() {
        let node = walk(&root, path).unwrap();
        assert_eq!(read_all(&node, 4096), pattern(*size, i as u32), "{path}");
    }
    // Everyone may read and run what has no Rock Ridge mode, as on Linux.
    assert_eq!(walk(&root, "one").unwrap().access().mode, 0o555);
    std::fs::remove_dir_all(tree).unwrap();
}

#[test]
fn plain_names_are_lower_case_without_versions() {
    let root = scratch("plain");
    std::fs::create_dir_all(root.join("DOCS")).unwrap();
    std::fs::write(root.join("README.TXT"), b"read me").unwrap();
    std::fs::write(root.join("DOCS/A.BIN"), pattern(9000, 7)).unwrap();
    let image = xorriso(&root, &["--norock"]);
    let fs = mount(&image, 2048).unwrap();
    let top = fs.root();
    assert_eq!(
        names(&top),
        vec![("docs".to_string(), Kind::Directory), ("readme.txt".to_string(), Kind::File)]
    );
    assert_eq!(read_all(&walk(&top, "readme.txt").unwrap(), 4096), b"read me");
    // Found in either case.
    assert_eq!(read_all(&walk(&top, "DOCS/A.BIN").unwrap(), 4096), pattern(9000, 7));
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn deep_directories_are_found_where_they_belong() {
    let root = scratch("deep");
    let deep = "a/b/c/d/e/f/g/h/i/j/k";
    std::fs::create_dir_all(root.join(deep)).unwrap();
    std::fs::write(root.join(deep).join("file"), b"deep").unwrap();
    // Directories past ISO 9660's eighth level are moved to one of their
    // own, with Rock Ridge's links to where they belong.
    let image = xorriso(&root, &["-R", "-rr_reloc_dir", "rr_moved"]);
    assert!(image.windows(4).any(|w| w == b"CL\x0c\x01"));
    let fs = mount(&image, 512).unwrap();
    let top = fs.root();
    // Relocated directories are listed only where they belong.
    let listed: Vec<String> = names(&top).into_iter().map(|(name, _)| name).collect();
    assert_eq!(listed, ["a"]);
    let node = walk(&top, &format!("{deep}/file")).unwrap();
    assert_eq!(read_all(&node, 4096), b"deep");
    assert_eq!(names(&walk(&top, "a/b/c/d/e/f/g/h").unwrap()), vec![("i".into(), Kind::Directory)]);
    assert_eq!(
        names(&walk(&top, "a/b/c/d/e/f/g/h/i/j").unwrap()),
        vec![("k".into(), Kind::Directory)]
    );
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn listing_goes_on_from_its_cursor() {
    let root = scratch("many");
    std::fs::create_dir_all(&root).unwrap();
    // Enough entries for several blocks of records.
    for i in 0..300 {
        std::fs::write(root.join(format!("file-with-a-longer-name-{i:03}")), [i as u8]).unwrap();
    }
    let image = xorriso(&root, &["-R"]);
    let fs = mount(&image, 512).unwrap();
    let top = fs.root();
    let mut all = Vec::new();
    let mut cursor = 0;
    loop {
        let mut got = 0;
        top.list(cursor, &mut |next, name, _| {
            all.push(name.to_string());
            cursor = next;
            got += 1;
            got < 7
        })
        .unwrap();
        if got == 0 {
            break;
        }
    }
    assert_eq!(all.len(), 300);
    all.sort();
    all.dedup();
    assert_eq!(all.len(), 300);
    let node = walk(&top, "file-with-a-longer-name-299").unwrap();
    assert_eq!(read_all(&node, 16), [299u32 as u8]);
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn a_file_of_two_extents_reads_as_one() {
    let root = scratch("extents");
    std::fs::create_dir_all(&root).unwrap();
    std::fs::write(root.join("A"), pattern(2048, 1)).unwrap();
    std::fs::write(root.join("B"), pattern(3000, 2)).unwrap();
    let mut image = xorriso(&root, &["--norock"]);
    // Each extent of a file has a record of the file's name, the first
    // with the multi-extent flag: make `B` the second extent of `A`, as a
    // file of more than 4 GiB is made.
    let at =
        |image: &[u8], name: &[u8]| image.windows(name.len()).position(|w| w == name).unwrap() - 33;
    let a = at(&image, b"A.;1");
    let b = at(&image, b"B.;1");
    assert_eq!(b, a + usize::from(image[a]));
    image[a + 25] |= format::flag::MULTI_EXTENT;
    image[b + 33] = b'A';
    let fs = mount(&image, 512).unwrap();
    let top = fs.root();
    assert_eq!(names(&top), vec![("a".to_string(), Kind::File)]);
    let node = walk(&top, "a").unwrap();
    assert_eq!(node.stat().size, 5048);
    let mut want = pattern(2048, 1);
    want.extend_from_slice(&pattern(3000, 2));
    assert_eq!(read_all(&node, 4096), want);
    assert_eq!(read_all(&node, 999), want);
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn other_disks_and_writable_mounts_are_refused() {
    let zeros = vec![0u8; 1 << 20];
    assert_eq!(mount(&zeros, 512).err(), Some(Error::UnknownFilesystem));
    let root = scratch("refused");
    std::fs::create_dir_all(&root).unwrap();
    std::fs::write(root.join("x"), b"x").unwrap();
    let image = xorriso(&root, &["-R"]);
    let disk = mem::arc(RamDisk::new(image.len() as u64 / 512, false)).unwrap();
    disk.load(0, &image).unwrap();
    let device: DeviceRef = disk;
    assert_eq!(IsoFs::mount(device, false).err(), Some(Error::ReadOnly));
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn names_decode_as_their_trees_have_them() {
    let mut name = NameBuf::default();
    format::plain_name(b"README.TXT;1", &mut name);
    assert_eq!(name.name(), Some("readme.txt"));
    format::plain_name(b"NOEXT.;1", &mut name);
    assert_eq!(name.name(), Some("noext"));
    let joliet: Vec<u8> = "Été;1".encode_utf16().flat_map(u16::to_be_bytes).collect();
    format::joliet_name(&joliet, &mut name);
    assert_eq!(name.name(), Some("Été"));
    format::plain_name(&[0], &mut name);
    assert_eq!(name.name(), None);
}
