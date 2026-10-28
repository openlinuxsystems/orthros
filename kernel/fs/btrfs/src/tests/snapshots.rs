// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Host tests of subvolumes and snapshots (ADR 0138): made, changed on
//! both sides and removed through the VFS, synced, then read back by a
//! read-only mount and checked by Linux's `btrfs check`, which follows
//! every reference of every block and extent.

use std::format;
use std::string::{String, ToString};
use std::vec::Vec;

use radix_vfs::{Error, FileSystem, Kind, NodeRef, Slice};

use radix_block::ram::RamDisk;
use radix_block::{BlockDevice, SECTOR};

use super::XorShift;
use super::files::{mount, noise};
use super::rw::{Model, Thing, mount_rw, put, snapshot as model_of, synced};
use super::write::{image_of, mkfs_empty, writable};
use crate::format::{Key, item, tree};
use crate::refs::FULL_BACKREF;
use crate::tree::Cursor;

/// How many tree blocks on `disk` refer to what is below them by their
/// address.
fn by_address(disk: &RamDisk) -> usize {
    let image = image_of(disk, (disk.info().sectors * SECTOR) as usize);
    let fs = mount(&image).unwrap();
    let volume = fs.volume();
    let extents = volume.tree(tree::EXTENT).unwrap().unwrap();
    let mut cursor = Cursor::seek(volume, &extents, &Key::new(0, 0, 0)).unwrap();
    let mut full = 0;
    while let Some((key, data)) = cursor.get() {
        let flags = u64::from_le_bytes(data[16..24].try_into().unwrap());
        if key.kind == item::METADATA_ITEM && flags & FULL_BACKREF != 0 {
            full += 1;
        }
        cursor.advance().unwrap();
    }
    full
}

/// What `model` holds under `prefix`, keyed from there.
fn under(model: &Model, prefix: &str) -> Model {
    model
        .iter()
        .filter_map(|(path, thing)| {
            let rest = path.strip_prefix(prefix)?;
            rest.starts_with('/').then(|| (rest.to_string(), thing.clone()))
        })
        .collect()
}

/// `model` put under `prefix`.
fn moved(model: &Model, prefix: &str) -> Model {
    model.iter().map(|(path, thing)| (format!("{prefix}{path}"), thing.clone())).collect()
}

/// Takes everything under `prefix` out of `model`, and `prefix` too.
fn forget(model: &mut Model, prefix: &str) {
    model.retain(|path, _| path != prefix && !path.starts_with(&format!("{prefix}/")));
}

/// Makes `n` files of up to `max` bytes in `dir`, which is `path` in
/// `model`, from `seed`.
fn fill(dir: &NodeRef, path: &str, n: usize, max: usize, seed: u64, model: &mut Model) {
    for i in 0..n {
        let name = format!("f{i}");
        let file = dir.create(&name, Kind::File).unwrap();
        let data = noise(seed + i as u64, (i * 7919 + seed as usize * 31) % max);
        put(&file, 0, &data);
        model.insert(format!("{path}/{name}"), Thing::File(data));
    }
}

/// The node at `path` ("/a/b") under `root`.
fn node(root: &NodeRef, path: &str) -> NodeRef {
    let mut node = root.clone();
    for name in path.split('/').filter(|n| !n.is_empty()) {
        node = node.lookup(name).unwrap_or_else(|e| panic!("{path}: {e:?}"));
    }
    node
}

/// Writes `data` over the file `path`, there and in `model`.
fn rewrite(root: &NodeRef, path: &str, data: Vec<u8>, model: &mut Model) {
    let file = node(root, path);
    file.set_size(0).unwrap();
    put(&file, 0, &data);
    model.insert(path.to_string(), Thing::File(data));
}

#[test]
fn a_snapshot_keeps_what_its_source_held_while_both_change() {
    // Blocks of 4 KiB and hundreds of files: the two trees share nodes
    // above their leaves.
    let disk = writable(&mkfs_empty(&["--nodesize", "4096"]));
    let fs = mount_rw(&disk);
    let root = fs.root();
    let mut model = Model::new();
    let d = root.create("d", Kind::Directory).unwrap();
    model.insert("/d".into(), Thing::Dir);
    fill(&d, "/d", 300, 20_000, 1, &mut model);
    drop(d);
    synced(&fs, &disk, &model, "filled");
    let filled = model.clone();
    root.snapshot("snap", &root, false).unwrap();
    assert_eq!(root.snapshot("snap", &root, false), Err(Error::Exists));
    model.insert("/snap".into(), Thing::Dir);
    model.extend(moved(&filled, "/snap"));
    synced(&fs, &disk, &model, "snapshotted");
    // Each side changes files of its own, and some the other changes too.
    for i in (0..300).step_by(3) {
        rewrite(&root, &format!("/d/f{i}"), noise(1000 + i, 5000), &mut model);
        rewrite(&root, &format!("/snap/d/f{}", i + 1), noise(2000 + i, 9000), &mut model);
        if i % 9 == 0 {
            rewrite(&root, &format!("/snap/d/f{i}"), noise(3000 + i, 70_000), &mut model);
        }
    }
    for i in (2..300).step_by(10) {
        node(&root, "/d").remove(&format!("f{i}")).unwrap();
        model.remove(&format!("/d/f{i}"));
        node(&root, "/snap/d").remove(&format!("f{}", i + 3)).unwrap();
        model.remove(&format!("/snap/d/f{}", i + 3));
    }
    synced(&fs, &disk, &model, "both changed");
    // A read-only snapshot of the snapshot, which nothing changes.
    let snap = root.lookup("snap").unwrap();
    root.snapshot("ro", &snap, true).unwrap();
    drop(snap);
    let ro = under(&model, "/snap");
    model.insert("/ro".into(), Thing::Dir);
    model.extend(moved(&ro, "/ro"));
    let file = node(&root, "/ro/d/f0");
    assert_eq!(file.write(0, &Slice(b"x")), Err(Error::ReadOnly));
    drop(file);
    assert_eq!(node(&root, "/ro/d").create("new", Kind::File).map(|_| ()), Err(Error::ReadOnly));
    // A snapshot of the top holds the subvolumes there as empty
    // directories, as on Linux.
    root.snapshot("again", &root, false).unwrap();
    let mut again = Model::new();
    for (path, thing) in &model {
        let top = path.split('/').nth(1).unwrap();
        if top == "snap" || top == "ro" {
            if path == "/snap" || path == "/ro" {
                again.insert(path.clone(), Thing::Dir);
            }
        } else {
            again.insert(path.clone(), thing.clone());
        }
    }
    model.insert("/again".into(), Thing::Dir);
    model.extend(moved(&again, "/again"));
    synced(&fs, &disk, &model, "snapshots of snapshots");
    // The source of a snapshot goes: the blocks it made that the snapshot
    // keeps come to refer by their address.
    root.remove_subvolume("snap").unwrap();
    forget(&mut model, "/snap");
    synced(&fs, &disk, &model, "the source removed");
    assert!(by_address(&disk) > 0, "the blocks it made refer by address");
    let ro_node = root.lookup("ro").unwrap();
    root.snapshot("rw", &ro_node, false).unwrap();
    drop(ro_node);
    model.insert("/rw".into(), Thing::Dir);
    model.extend(moved(&ro, "/rw"));
    root.remove_subvolume("ro").unwrap();
    forget(&mut model, "/ro");
    synced(&fs, &disk, &model, "a copy of the read-only one");
    // Its blocks are another's, and it alone has them now.
    for i in (0..300).step_by(7) {
        let path = format!("/rw/d/f{i}");
        if model.contains_key(&path) {
            rewrite(&root, &path, noise(4000 + i, 12_000), &mut model);
        }
    }
    synced(&fs, &disk, &model, "the copy changed");
    // The stale entries of a snapshot of the top go as empty directories.
    assert_eq!(node(&root, "/again").remove_subvolume("snap"), Err(Error::NotSupported));
    node(&root, "/again").remove("snap").unwrap();
    node(&root, "/again").remove("ro").unwrap();
    forget(&mut model, "/again/snap");
    forget(&mut model, "/again/ro");
    for name in ["rw", "again"] {
        root.remove_subvolume(name).unwrap();
        forget(&mut model, &format!("/{name}"));
    }
    synced(&fs, &disk, &model, "every snapshot removed");
    assert_eq!(model_of(&root), model);
    assert_eq!(by_address(&disk), 0, "no block is shared");
}

#[test]
fn an_empty_subvolume_goes_as_an_empty_directory_does() {
    let disk = writable(&mkfs_empty(&[]));
    let fs = mount_rw(&disk);
    let root = fs.root();
    let mut model = Model::new();
    root.make_subvolume("sub").unwrap();
    assert_eq!(root.make_subvolume("sub"), Err(Error::Exists));
    model.insert("/sub".into(), Thing::Dir);
    let sub = root.lookup("sub").unwrap();
    assert_eq!(sub.kind(), Kind::Directory);
    assert_ne!(sub.stat().id, root.stat().id);
    fill(&sub, "/sub", 20, 30_000, 7, &mut model);
    synced(&fs, &disk, &model, "a subvolume");
    assert_eq!(root.remove("sub"), Err(Error::NotEmpty));
    // In use, it stays.
    assert_eq!(root.remove_subvolume("sub"), Err(Error::InUse));
    for i in 0..20 {
        sub.remove(&format!("f{i}")).unwrap();
        model.remove(&format!("/sub/f{i}"));
    }
    drop(sub);
    root.remove("sub").unwrap();
    model.remove("/sub");
    synced(&fs, &disk, &model, "removed as a directory");
    // One holding another goes after it.
    root.make_subvolume("outer").unwrap();
    node(&root, "/outer").make_subvolume("inner").unwrap();
    assert_eq!(root.remove_subvolume("outer"), Err(Error::NotEmpty));
    model.insert("/outer".into(), Thing::Dir);
    model.insert("/outer/inner".into(), Thing::Dir);
    synced(&fs, &disk, &model, "nested");
    node(&root, "/outer").remove_subvolume("inner").unwrap();
    root.remove_subvolume("outer").unwrap();
    model.clear();
    root.create("plain", Kind::Directory).unwrap();
    model.insert("/plain".into(), Thing::Dir);
    assert_eq!(root.remove_subvolume("plain"), Err(Error::NotSupported));
    assert_eq!(root.remove_subvolume("missing"), Err(Error::NotFound));
    synced(&fs, &disk, &model, "nested ones removed");
}

#[test]
fn random_snapshots_changes_and_removals_match_a_model() {
    let disk = writable(&mkfs_empty(&["--nodesize", "4096"]));
    let fs = mount_rw(&disk);
    let root = fs.root();
    let mut model = Model::new();
    let mut random = XorShift(0x5a95);
    // The subvolumes, by name, and whether each is read-only.
    let mut subvolumes: Vec<(String, bool)> = Vec::new();
    let mut made = 0;
    for step in 0..240 {
        let writable: Vec<String> =
            subvolumes.iter().filter(|s| !s.1).map(|s| s.0.clone()).collect();
        match random.below(10) {
            0 if subvolumes.len() < 6 => {
                let name = format!("s{made}");
                made += 1;
                root.make_subvolume(&name).unwrap();
                let dir = root.lookup(&name).unwrap();
                fill(&dir, &format!("/{name}"), 40, 15_000, random.next() % 1000, &mut model);
                model.insert(format!("/{name}"), Thing::Dir);
                subvolumes.push((name, false));
            }
            1 | 2 if !subvolumes.is_empty() && subvolumes.len() < 6 => {
                let (source, _) = subvolumes[random.below(subvolumes.len())].clone();
                let name = format!("s{made}");
                made += 1;
                let read_only = random.below(3) == 0;
                let from = root.lookup(&source).unwrap();
                root.snapshot(&name, &from, read_only).unwrap();
                drop(from);
                let copy = under(&model, &format!("/{source}"));
                model.insert(format!("/{name}"), Thing::Dir);
                model.extend(moved(&copy, &format!("/{name}")));
                subvolumes.push((name, read_only));
            }
            3 if !subvolumes.is_empty() => {
                let at = random.below(subvolumes.len());
                let (name, _) = subvolumes.remove(at);
                root.remove_subvolume(&name).unwrap();
                forget(&mut model, &format!("/{name}"));
            }
            _ if !writable.is_empty() => {
                let sub = &writable[random.below(writable.len())];
                let path = format!("/{sub}/f{}", random.below(50));
                let held = model.contains_key(&path);
                if held && random.below(4) == 0 {
                    let (dir, name) = path.rsplit_once('/').unwrap();
                    node(&root, dir).remove(name).unwrap();
                    model.remove(&path);
                } else if held {
                    rewrite(&root, &path, noise(random.next(), random.below(40_000)), &mut model);
                } else {
                    let (dir, name) = path.rsplit_once('/').unwrap();
                    let file = node(&root, dir).create(name, Kind::File).unwrap();
                    let data = noise(random.next(), random.below(40_000));
                    put(&file, 0, &data);
                    model.insert(path, Thing::File(data));
                }
            }
            _ => {}
        }
        if step % 60 == 59 {
            synced(&fs, &disk, &model, &format!("step {step}"));
        }
    }
    for (name, _) in subvolumes {
        root.remove_subvolume(&name).unwrap();
        forget(&mut model, &format!("/{name}"));
    }
    synced(&fs, &disk, &model, "all removed");
    assert!(model.is_empty());
}
