// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Host tests of the log of fsyncs Radix writes (ADR 0154): an fsync of a
//! file the last commit has writes a log, not a commit, about as many
//! bytes as Linux's; a power cut anywhere keeps what each fsync made
//! durable, as a mount replays it, and passes `btrfs check`; a new file, a
//! renamed one and a directory are made durable by a commit.

use std::collections::{BTreeMap, BTreeSet};
use std::vec::Vec;

use radix_block::DeviceRef;
use radix_vfs::mem::{self, KArc};
use radix_vfs::{FileSystem, Kind, NodeRef};

use super::XorShift;
use super::files::noise;
use super::log::{list, log_root as image_log_root, replayed_list, unsparse};
use super::rw::{Model, Thing, put, snapshot};
use super::write::{CrashDisk, Op, btrfs_check, image_of, mkfs_empty, mkfs_sized, writable};
use super::{DIR, FILE};
use crate::fs::BtrfsFs;

/// How many writes through `disk` had: one a superblock.
fn throughs(disk: &CrashDisk) -> usize {
    disk.ops().iter().filter(|op| matches!(op, Op::Through(..))).count()
}

/// The log root the last superblock written to `disk` names.
fn log_root(disk: &CrashDisk) -> u64 {
    let ops = disk.ops();
    let Some(Op::Through(at, data)) = ops.iter().rev().find(|op| matches!(op, Op::Through(..)))
    else {
        return 0;
    };
    assert_eq!(*at, 0x1_0000);
    u64::from_le_bytes(data[0x60..0x68].try_into().unwrap())
}

/// A writable mount of `disk`.
fn mount_crash(disk: &KArc<CrashDisk>) -> KArc<BtrfsFs> {
    let device: DeviceRef = disk.clone();
    BtrfsFs::mount_writable(device).unwrap()
}

/// Sets file `path` of `model` to `size` bytes.
fn resize(model: &mut Model, path: &str, size: usize) {
    let Some(Thing::File(bytes)) = model.get_mut(path) else { panic!("{path} is no file") };
    bytes.resize(size, 0);
}

/// Writes `data` at `offset` of file `path` of `model`.
fn put_model(model: &mut Model, path: &str, offset: usize, data: &[u8]) {
    let Some(Thing::File(bytes)) = model.get_mut(path) else { panic!("{path} is no file") };
    if bytes.len() < offset + data.len() {
        bytes.resize(offset + data.len(), 0);
    }
    bytes[offset..offset + data.len()].copy_from_slice(data);
}

/// What each count of writes through `disk` keeps, cut anywhere: mounted
/// (which replays the log) it reads that, and `btrfs check` passes.
fn every_cut_keeps(disk: &CrashDisk, states: &BTreeMap<usize, Model>, seed: u64) {
    let ops = disk.ops();
    let mut cuts: BTreeSet<usize> = BTreeSet::new();
    let mut random = XorShift(seed);
    for (at, op) in ops.iter().enumerate() {
        if matches!(op, Op::Flush | Op::Through(..)) {
            cuts.extend([at, at + 1]);
        }
    }
    for _ in 0..16 {
        cuts.insert(random.below(ops.len() + 1));
    }
    for cut in cuts {
        let (medium, durable) = disk.cut(cut, || random.next() & 1 == 0);
        let (whole, text) = btrfs_check(&medium);
        assert!(whole, "a cut before op {cut} of {}: btrfs check failed:\n{text}", ops.len());
        // A disk to write, which a replay needs.
        let ram = writable(&medium);
        let device: DeviceRef = ram.clone();
        let fs = BtrfsFs::mount(device).unwrap_or_else(|e| panic!("a cut before op {cut}: {e:?}"));
        let (whole, text) = btrfs_check(&image_of(&ram, medium.len()));
        assert!(
            whole,
            "a cut before op {cut} of {}, replayed: btrfs check failed:\n{text}",
            ops.len()
        );
        let (_, want) = states.range(..=durable).next_back().unwrap();
        let read = snapshot(&fs.root());
        assert_eq!(
            read.keys().collect::<Vec<_>>(),
            want.keys().collect::<Vec<_>>(),
            "a cut before op {cut} of {}, {durable} superblocks",
            ops.len()
        );
        for (path, thing) in want {
            if let (Thing::File(r), Thing::File(w)) = (&read[path], thing)
                && r != w
            {
                let first = r.iter().zip(w).position(|(a, b)| a != b);
                panic!(
                    "a cut before op {cut} of {} ({durable} superblocks): {path} differs: {} bytes, \
                     {} wanted, first at {first:?}",
                    ops.len(),
                    r.len(),
                    w.len()
                );
            }
            assert!(read[path] == *thing, "a cut before op {cut} of {}: {path} differs", ops.len());
        }
    }
}

#[test]
fn a_power_cut_anywhere_keeps_each_fsync() {
    let image = mkfs_empty(&[]);
    let disk = mem::arc(CrashDisk::new(&image)).unwrap();
    let mut states: BTreeMap<usize, Model> = BTreeMap::new();
    states.insert(0, Model::new());
    {
        let fs = mount_crash(&disk);
        let root = fs.root();
        let mut model = Model::new();
        let file = |dir: &NodeRef, name: &str, model: &mut Model, path: &str, len: usize| {
            let node = dir.create(name, Kind::File, FILE).unwrap();
            let data = noise(path.len() as u64, len);
            put(&node, 0, &data);
            model.insert(path.into(), Thing::File(data));
            node
        };
        let a = file(&root, "a", &mut model, "/a", 300_000);
        let b = file(&root, "b", &mut model, "/b", 50_000);
        let d = root.create("d", Kind::Directory, DIR).unwrap();
        model.insert("/d".into(), Thing::Dir);
        let c = file(&d, "c", &mut model, "/d/c", 20_000);
        fs.sync().unwrap();
        states.insert(throughs(&disk), model.clone());
        // What a power cut keeps: the commit, and each file as its last
        // fsync left it.
        let mut durable = model.clone();
        let fsync = |node: &NodeRef, path: &str, model: &Model, durable: &mut Model| {
            node.sync().unwrap();
            assert_ne!(log_root(&disk), 0, "{path}: a log, not a commit");
            durable.insert(path.into(), model[path].clone());
            (throughs(&disk), durable.clone())
        };
        let mut random = XorShift(0x10e);
        // Overwritten in place, a sector and less.
        let data = noise(random.next(), 4096);
        put(&a, 8192, &data);
        put_model(&mut model, "/a", 8192, &data);
        let (n, m) = fsync(&a, "/a", &model, &mut durable);
        states.insert(n, m);
        let data = noise(random.next(), 100);
        put(&b, 10, &data);
        put_model(&mut model, "/b", 10, &data);
        let (n, m) = fsync(&b, "/b", &model, &mut durable);
        states.insert(n, m);
        // Grown, cut, and grown with a hole.
        let data = noise(random.next(), 70_000);
        put(&a, 300_000, &data);
        put_model(&mut model, "/a", 300_000, &data);
        let (n, m) = fsync(&a, "/a", &model, &mut durable);
        states.insert(n, m);
        a.set_size(100_000).unwrap();
        resize(&mut model, "/a", 100_000);
        let (n, m) = fsync(&a, "/a", &model, &mut durable);
        states.insert(n, m);
        a.set_size(250_000).unwrap();
        resize(&mut model, "/a", 250_000);
        let data = noise(random.next(), 8192);
        put(&a, 180_000, &data);
        put_model(&mut model, "/a", 180_000, &data);
        let (n, m) = fsync(&a, "/a", &model, &mut durable);
        states.insert(n, m);
        // A file in a directory; one changed and not synced.
        let data = noise(random.next(), 30_000);
        put(&c, 5000, &data);
        put_model(&mut model, "/d/c", 5000, &data);
        let (n, m) = fsync(&c, "/d/c", &model, &mut durable);
        states.insert(n, m);
        let data = noise(random.next(), 9000);
        put(&b, 20_000, &data);
        put_model(&mut model, "/b", 20_000, &data);
        // Overwritten again and again, as a database does.
        for i in 0..12 {
            let data = noise(random.next(), 4096);
            put(&a, i * 4096, &data);
            put_model(&mut model, "/a", i as usize * 4096, &data);
            let (n, m) = fsync(&a, "/a", &model, &mut durable);
            states.insert(n, m);
        }
        // A new file's fsync commits, with all the transaction holds: not
        // what waits in another file's pages.
        let n = file(&root, "n", &mut model, "/n", 10_000);
        n.sync().unwrap();
        assert_eq!(log_root(&disk), 0, "a new file commits");
        durable.insert("/n".into(), model["/n"].clone());
        states.insert(throughs(&disk), durable.clone());
        // After a commit, a log again.
        let data = noise(random.next(), 4096);
        put(&b, 0, &data);
        put_model(&mut model, "/b", 0, &data);
        let (n, m) = fsync(&b, "/b", &model, &mut durable);
        states.insert(n, m);
        // A renamed file's fsync commits.
        root.rename("b", &*d, "moved").unwrap();
        let moved = model.remove("/b").unwrap();
        model.insert("/d/moved".into(), moved);
        b.sync().unwrap();
        assert_eq!(log_root(&disk), 0, "a renamed file commits");
        states.insert(throughs(&disk), model.clone());
    }
    every_cut_keeps(&disk, &states, 0xfeed);
}

/// The bench's fsync (`posix_btrfs_fsync`): 4 KiB written over a file the
/// last commit has, and synced, on a disk of 192 MiB whose metadata is
/// kept twice. Linux's log writes two blocks, each twice, the data and
/// the superblock: 72 KiB, a flush, and a write through.
#[test]
fn an_fsync_writes_about_what_linuxs_log_does() {
    let image = mkfs_sized(192 << 20, &[]);
    let disk = mem::arc(CrashDisk::new(&image)).unwrap();
    let fs = mount_crash(&disk);
    let file = fs.root().create("f", Kind::File, FILE).unwrap();
    put(&file, 0, &noise(1, 64 << 10));
    fs.sync().unwrap();
    for round in 0..4 {
        let before = disk.ops().len();
        put(&file, 0, &noise(round + 2, 4096));
        file.sync().unwrap();
        let ops = disk.ops();
        let (mut bytes, mut flushes, mut through) = (0, 0, 0);
        for op in &ops[before..] {
            match op {
                Op::Write(_, data) => bytes += data.len(),
                Op::Through(_, data) => {
                    bytes += data.len();
                    through += 1;
                }
                Op::Flush => flushes += 1,
            }
        }
        assert_eq!((flushes, through), (1, 1), "round {round}");
        assert!(bytes <= 72 << 10, "round {round}: {} KiB", bytes >> 10);
    }
}

/// The disk `ob fixture btrfs-radix-log` boots Linux on, written to the
/// path `RADIX_BTRFS_LOG_OUT` names: files committed, then changed with an
/// fsync after each change, one changed and not synced, and the power cut
/// after the last fsync.
#[test]
#[ignore = "made for `ob fixture btrfs-radix-log`"]
fn a_log_for_linux_to_replay() {
    let Some(out) = std::env::var_os("RADIX_BTRFS_LOG_OUT") else { return };
    let disk = mem::arc(CrashDisk::new(&mkfs_empty(&[]))).unwrap();
    let fs = mount_crash(&disk);
    let root = fs.root();
    let made = |dir: &NodeRef, name: &str, len: usize| {
        let node = dir.create(name, Kind::File, FILE).unwrap();
        put(&node, 0, &noise(name.len() as u64 * 7 + len as u64, len));
        node
    };
    let grown = made(&root, "grown", 100_000);
    let cut = made(&root, "cut", 200_000);
    let holes = made(&root, "holes", 300_000);
    let unsynced = made(&root, "unsynced", 5000);
    let times = made(&root, "times", 3000);
    let d = root.create("d", Kind::Directory, DIR).unwrap();
    let inner = made(&d, "inner", 50_000);
    fs.sync().unwrap();
    let mut random = XorShift(0x11b);
    put(&grown, 4096, &noise(random.next(), 4096));
    put(&grown, 100_000, &noise(random.next(), 50_000));
    grown.sync().unwrap();
    cut.set_size(50_000).unwrap();
    cut.sync().unwrap();
    holes.set_size(10_000).unwrap();
    holes.set_size(400_000).unwrap();
    put(&holes, 200_000, &noise(random.next(), 8192));
    holes.sync().unwrap();
    put(&inner, 1000, &noise(random.next(), 300));
    inner.sync().unwrap();
    times.set_times(Some(1_000_000_000), Some(2_000_000_000)).unwrap();
    times.sync().unwrap();
    put(&unsynced, 0, &noise(random.next(), 5000));
    put(&grown, 0, &noise(random.next(), 4096));
    grown.sync().unwrap();
    let (medium, _) = disk.cut(disk.ops().len(), || false);
    assert_ne!(image_log_root(&medium), 0, "the last fsync wrote a log");
    std::fs::write(out, medium).unwrap();
}

/// The disk [`a_log_for_linux_to_replay`] made, as `ob fixture
/// btrfs-radix-log` keeps it.
const RADIX_LOG: &[u8] = include_bytes!("fixtures/btrfs-radix-log.sparse");

/// Linux's lists of that disk, before and after its replay.
const RADIX_LISTS: &str = include_str!("fixtures/btrfs-radix-log.txt");

#[test]
fn linux_replays_radixs_log_as_radix_does() {
    let image = unsparse(RADIX_LOG);
    assert_ne!(image_log_root(&image), 0, "the fixture has no log");
    let committed: Vec<&str> = RADIX_LISTS
        .lines()
        .skip_while(|line| *line != "LIST committed")
        .take_while(|line| *line != "LIST replayed")
        .collect();
    let replayed = replayed_list(RADIX_LISTS);
    assert!(committed.len() > 1 && !replayed.is_empty(), "the fixture lists nothing");
    let disk = writable(&image);
    let fs = BtrfsFs::mount(disk.clone()).unwrap();
    assert_eq!(list(&fs), replayed);
    let (whole, text) = btrfs_check(&image_of(&disk, image.len()));
    assert!(whole, "btrfs check failed after the replay:\n{text}");
}
