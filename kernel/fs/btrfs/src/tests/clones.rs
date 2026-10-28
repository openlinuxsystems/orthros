// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Host tests of clones (ADR 0139): files that share their data, whole
//! or in part, made through the VFS, changed and removed on either side,
//! synced, then read back by a read-only mount and checked by Linux's
//! `btrfs check`, which counts every reference of every extent.

use std::format;
use std::vec::Vec;

use radix_vfs::{Error, FileSystem, Kind, NodeRef};

use super::XorShift;
use super::files::noise;
use super::rw::{Model, Thing, mount_rw, put, synced};
use super::write::{mkfs_empty, writable};

/// Makes the file `name` in `dir`, which is `path`, holding `data`.
fn file(dir: &NodeRef, path: &str, name: &str, data: &[u8], model: &mut Model) -> NodeRef {
    let file = dir.create(name, Kind::File).unwrap();
    put(&file, 0, data);
    model.insert(format!("{path}/{name}"), Thing::File(data.to_vec()));
    file
}

/// The bytes of the file `path` in `model`.
fn bytes<'m>(model: &'m mut Model, path: &str) -> &'m mut Vec<u8> {
    match model.get_mut(path) {
        Some(Thing::File(bytes)) => bytes,
        _ => panic!("{path} is no file"),
    }
}

/// Clones `len` bytes (0: to the end) of `source`, `from` on, to `to` in
/// `dest`, there and in `model`.
fn clone(
    (dest, dest_path): (&NodeRef, &str),
    (source, source_path): (&NodeRef, &str),
    from: u64,
    len: u64,
    to: u64,
    model: &mut Model,
) {
    dest.clone_range(source, from, len, to).unwrap();
    let data = bytes(model, source_path).clone();
    let len = if len == 0 { data.len() - from as usize } else { len as usize };
    let piece = data[from as usize..from as usize + len].to_vec();
    let out = bytes(model, dest_path);
    let end = to as usize + len;
    if out.len() < end {
        out.resize(end, 0);
    }
    out[to as usize..end].copy_from_slice(&piece);
}

/// Writes `data` at `at` in the file `path`, there and in `model`.
fn write(file: &NodeRef, path: &str, at: u64, data: &[u8], model: &mut Model) {
    put(file, at, data);
    let out = bytes(model, path);
    let end = at as usize + data.len();
    if out.len() < end {
        out.resize(end, 0);
    }
    out[at as usize..end].copy_from_slice(data);
}

#[test]
fn a_clone_shares_its_sources_data_until_either_changes_it() {
    let disk = writable(&mkfs_empty(&[]));
    let fs = mount_rw(&disk);
    let root = fs.root();
    let mut model = Model::new();
    let a = file(&root, "", "a", &noise(1, 900_000), &mut model);
    synced(&fs, &disk, &model, "written");
    let free = fs.info().free;
    let b = root.create("b", Kind::File).unwrap();
    model.insert("/b".into(), Thing::File(Vec::new()));
    clone((&b, "/b"), (&a, "/a"), 0, 0, 0, &mut model);
    synced(&fs, &disk, &model, "cloned whole");
    // The data is there once.
    let took = free.saturating_sub(fs.info().free);
    assert!(took < 100_000, "the clone took {took} bytes");
    // Part of it, into the middle of a file with data of its own, which
    // goes there.
    let c = file(&root, "", "c", &noise(2, 100_000), &mut model);
    clone((&c, "/c"), (&a, "/a"), 8192, 40_960, 4096, &mut model);
    // Past the end of a file, which grows to hold it, and the source's
    // last block, to its end.
    clone((&c, "/c"), (&a, "/a"), 868_352, 0, 200_704, &mut model);
    synced(&fs, &disk, &model, "cloned in part");
    // Past the end of a file cut within a block, which the disk still
    // holds past the end: the gap reads as zeros.
    let g = file(&root, "", "g", &noise(6, 100_000), &mut model);
    synced(&fs, &disk, &model, "written to cut");
    g.set_size(50_000).unwrap();
    bytes(&mut model, "/g").truncate(50_000);
    clone((&g, "/g"), (&a, "/a"), 0, 8192, 65_536, &mut model);
    synced(&fs, &disk, &model, "past a cut end");
    // Each side changes, and the others keep what they had.
    write(&a, "/a", 4096, &noise(3, 20_000), &mut model);
    write(&b, "/b", 500_000, &noise(4, 70_000), &mut model);
    write(&c, "/c", 12_000, &noise(5, 3000), &mut model);
    synced(&fs, &disk, &model, "changed on each side");
    // Within one file, apart.
    clone((&a, "/a"), (&a, "/a"), 0, 65_536, 655_360, &mut model);
    synced(&fs, &disk, &model, "within a file");
    // Parts of compressed extents, which hold 128 KiB of a file each.
    fs.set_compress(true);
    let text: Vec<u8> =
        (0..60_000u32).flat_map(|i| format!("line {} ", i % 977).into_bytes()).collect();
    let z = file(&root, "", "z", &text, &mut model);
    synced(&fs, &disk, &model, "compressed");
    let e = file(&root, "", "e", &[], &mut model);
    clone((&e, "/e"), (&z, "/z"), 12_288, 196_608, 0, &mut model);
    fs.set_compress(false);
    synced(&fs, &disk, &model, "parts of compressed extents");
    // Into another subvolume.
    root.make_subvolume("sub").unwrap();
    model.insert("/sub".into(), Thing::Dir);
    let sub = root.lookup("sub").unwrap();
    let d = sub.create("d", Kind::File).unwrap();
    model.insert("/sub/d".into(), Thing::File(Vec::new()));
    clone((&d, "/sub/d"), (&b, "/b"), 0, 0, 0, &mut model);
    synced(&fs, &disk, &model, "into another subvolume");
    // The source goes; the clones keep its data.
    drop(a);
    root.remove("a").unwrap();
    model.remove("/a");
    synced(&fs, &disk, &model, "the source removed");
    drop((b, c, d, e, g, z));
    for name in ["b", "c", "e", "g", "z"] {
        root.remove(name).unwrap();
        model.remove(&format!("/{name}"));
    }
    sub.remove("d").unwrap();
    model.remove("/sub/d");
    drop(sub);
    root.remove_subvolume("sub").unwrap();
    model.remove("/sub");
    synced(&fs, &disk, &model, "every clone removed");
    assert!(fs.info().free >= free, "the data went with its last file");
}

#[test]
fn clones_linux_refuses_are_refused() {
    let disk = writable(&mkfs_empty(&[]));
    let fs = mount_rw(&disk);
    let root = fs.root();
    let mut model = Model::new();
    let a = file(&root, "", "a", &noise(1, 50_000), &mut model);
    let b = file(&root, "", "b", &noise(2, 90_000), &mut model);
    let dir = root.create("dir", Kind::Directory).unwrap();
    model.insert("/dir".into(), Thing::Dir);
    // Not whole blocks.
    assert_eq!(b.clone_range(&a, 100, 4096, 0), Err(Error::InvalidArgument));
    assert_eq!(b.clone_range(&a, 0, 4096, 100), Err(Error::InvalidArgument));
    assert_eq!(b.clone_range(&a, 0, 5000, 0), Err(Error::InvalidArgument));
    // The source's last block, partly, into the middle of a longer file.
    assert_eq!(b.clone_range(&a, 0, 0, 0), Err(Error::InvalidArgument));
    // Past the source's end.
    assert_eq!(b.clone_range(&a, 0, 65_536, 0), Err(Error::InvalidArgument));
    // Ranges of one file that overlap.
    assert_eq!(b.clone_range(&b, 0, 16_384, 8192), Err(Error::InvalidArgument));
    // Directories.
    assert_eq!(dir.clone_range(&a, 0, 0, 0), Err(Error::IsDirectory));
    assert_eq!(b.clone_range(&dir, 0, 0, 0), Err(Error::IsDirectory));
    // Another filesystem.
    let other_disk = writable(&mkfs_empty(&[]));
    let other = mount_rw(&other_disk);
    let x = other.root().create("x", Kind::File).unwrap();
    assert_eq!(x.clone_range(&a, 0, 0, 0), Err(Error::CrossMount));
    // Nothing changed.
    synced(&fs, &disk, &model, "refused");
}

#[test]
fn random_clones_and_writes_match_a_model() {
    let disk = writable(&mkfs_empty(&["--nodesize", "4096"]));
    let fs = mount_rw(&disk);
    let root = fs.root();
    let mut model = Model::new();
    let mut random = XorShift(0xc10e);
    let mut files = Vec::new();
    for i in 0..6 {
        let name = format!("f{i}");
        let len = random.below(300_000);
        files.push((file(&root, "", &name, &noise(i, len), &mut model), format!("/{name}")));
    }
    let block = |random: &mut XorShift, max: usize| (random.below(max / 4096 + 1) * 4096) as u64;
    for step in 0..300 {
        let (dest, dest_path) = files[random.below(files.len())].clone();
        let (source, source_path) = files[random.below(files.len())].clone();
        let size = bytes(&mut model, &source_path).len();
        match random.below(4) {
            0 => {
                let at = random.below(400_000) as u64;
                write(&dest, &dest_path, at, &noise(1000 + step, random.below(30_000)), &mut model);
            }
            _ if size >= 4096 => {
                let from = block(&mut random, size - 4096);
                let len = block(&mut random, (size - from as usize).min(100_000));
                let to = block(&mut random, 400_000);
                let len = if len == 0 { 4096 } else { len };
                let apart = dest_path != source_path || from + len <= to || to + len <= from;
                if apart {
                    clone((&dest, &dest_path), (&source, &source_path), from, len, to, &mut model);
                }
            }
            _ => {}
        }
        if step % 50 == 49 {
            synced(&fs, &disk, &model, &format!("step {step}"));
        }
    }
    drop(files);
    synced(&fs, &disk, &model, "the end");
}
