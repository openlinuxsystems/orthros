// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Host tests of simple quotas (ADR 0170): turned on, each subvolume's
//! group counts what it writes, through snapshots, clones, removals and
//! a balance; a limit stops writes past it; turned off, nothing is left
//! but the owners data extents name. Linux's `btrfs check` counts every
//! group again from the extents after each, and Linux's full quota
//! groups are marked inconsistent once Radix writes.

use std::vec::Vec;

use radix_vfs::{Balance, BalanceArgs, Error, FileSystem, Kind, Qgroup, Slice};

use super::FILE;
use super::files::noise;
use super::rw::{Model, mount_rw, put, snapshot as model_of, synced};
use super::write::{btrfs_check, image_of, mkfs_empty, writable};
use crate::BtrfsFs;

/// The group of subvolume `id` of `fs`.
fn group(fs: &BtrfsFs, id: u64) -> Qgroup {
    let groups = fs.qgroups().unwrap();
    *groups.iter().find(|g| g.id == id).unwrap_or_else(|| panic!("no group 0/{id}"))
}

/// Syncs `fs`, and checks `btrfs check` finds every group's counts as it
/// counts them again.
fn counts_check(fs: &BtrfsFs, disk: &radix_block::ram::RamDisk, len: usize, what: &str) {
    fs.sync().unwrap();
    let (whole, text) = btrfs_check(&image_of(disk, len));
    assert!(whole && !text.contains("are different"), "{what}: btrfs check:\n{text}");
}

#[test]
fn each_subvolume_counts_what_it_wrote() {
    let image = mkfs_empty(&[]);
    let disk = writable(&image);
    let fs = mount_rw(&disk);
    put(&fs.root().create("before", Kind::File, FILE).unwrap(), 0, &noise(1, 100_000));
    fs.quota(true).unwrap();
    // A second time does nothing.
    fs.quota(true).unwrap();
    let groups = fs.qgroups().unwrap();
    assert_eq!(groups.iter().map(|g| g.id).collect::<Vec<_>>(), [5]);
    assert_eq!((groups[0].referenced, groups[0].max_referenced), (0, u64::MAX));
    let root = fs.root();
    put(&root.create("a", Kind::File, FILE).unwrap(), 0, &noise(2, 300_000));
    root.make_subvolume("sub").unwrap();
    let sub = root.lookup("sub").unwrap();
    put(&sub.create("b", Kind::File, FILE).unwrap(), 0, &noise(3, 200_000));
    counts_check(&fs, &disk, image.len(), "files in two subvolumes");
    let top = group(&fs, 5);
    assert!(top.referenced >= 300 << 10 && top.referenced == top.exclusive, "{top:?}");
    let first = group(&fs, 256);
    assert!(first.referenced >= 200 << 10, "{first:?}");
    // A snapshot counts its own blocks, not what it shares: those stay
    // its source's.
    root.snapshot("snap", &root, false).unwrap();
    let snap = group(&fs, 257);
    assert!(snap.referenced < 64 << 10, "{snap:?}");
    let a = root.lookup("a").unwrap();
    put(&a, 4096, &noise(4, 8192));
    let copy = sub.create("copy", Kind::File, FILE).unwrap();
    copy.clone_range(&a, 0, 128 << 10, 0).unwrap();
    counts_check(&fs, &disk, image.len(), "a snapshot, a change and a clone");
    // What only the snapshot still refers to stays counted to the top.
    root.remove("a").unwrap();
    root.remove("before").unwrap();
    counts_check(&fs, &disk, image.len(), "files removed");
    root.remove_subvolume("snap").unwrap();
    counts_check(&fs, &disk, image.len(), "the snapshot removed");
    // A balance moves every extent, counted again to its owner.
    let every = BalanceArgs { on: true, ..BalanceArgs::default() };
    fs.balance(&Balance { data: every, metadata: every, system: every }).unwrap();
    // The removed file's extents go once nothing has it open.
    drop((root, sub, a, copy));
    counts_check(&fs, &disk, image.len(), "a balance");
    let (top, first) = (group(&fs, 5), group(&fs, 256));
    drop(fs);
    // A mount reads the groups as they were.
    let fs = mount_rw(&disk);
    assert_eq!(group(&fs, 5), top);
    assert_eq!(group(&fs, 256), first);
}

#[test]
fn a_limit_stops_writes_past_it_and_removing_frees_room() {
    let image = mkfs_empty(&[]);
    let disk = writable(&image);
    let fs = mount_rw(&disk);
    fs.quota(true).unwrap();
    fs.limit_qgroup(5, 1 << 20, u64::MAX).unwrap();
    assert_eq!(fs.limit_qgroup(300, 1, 1), Err(Error::NotFound));
    let root = fs.root();
    let small = root.create("small", Kind::File, FILE).unwrap();
    put(&small, 0, &noise(1, 512 << 10));
    // A write stops short at the limit, and the next one fails, as
    // Linux's do.
    let big = root.create("big", Kind::File, FILE).unwrap();
    let data = noise(2, 1 << 20);
    let wrote = big.write(0, &Slice(&data)).unwrap();
    assert!(wrote > 0 && wrote < 512 << 10, "{wrote}");
    let rest = Slice(&data[wrote..]);
    assert_eq!(big.write(wrote as u64, &rest), Err(Error::QuotaExceeded));
    counts_check(&fs, &disk, image.len(), "a write stopped");
    // Its extents go once nothing has it open.
    root.remove("small").unwrap();
    drop(small);
    fs.sync().unwrap();
    put(&big, wrote as u64, &noise(3, 256 << 10));
    let top = group(&fs, 5);
    assert_eq!(top.max_referenced, 1 << 20);
    assert!(top.referenced <= 1 << 20, "{top:?}");
    // No limit again.
    fs.limit_qgroup(5, u64::MAX, u64::MAX).unwrap();
    put(&root.create("more", Kind::File, FILE).unwrap(), 0, &noise(4, 1 << 20));
    counts_check(&fs, &disk, image.len(), "the limit gone");
    assert_eq!(group(&fs, 5).max_referenced, u64::MAX);
}

#[test]
fn quotas_turned_off_leave_nothing_to_count() {
    let image = mkfs_empty(&[]);
    let disk = writable(&image);
    let fs = mount_rw(&disk);
    let mut model = Model::new();
    fs.quota(true).unwrap();
    put(&fs.root().create("a", Kind::File, FILE).unwrap(), 0, &noise(1, 200_000));
    fs.sync().unwrap();
    fs.quota(false).unwrap();
    assert_eq!(fs.qgroups(), Err(Error::NotSupported));
    put(&fs.root().lookup("a").unwrap(), 8192, &noise(2, 8192));
    model.extend(model_of(&fs.root()));
    synced(&fs, &disk, &model, "quotas turned off");
    // And on again, from zero.
    fs.quota(true).unwrap();
    assert_eq!(group(&fs, 5).referenced, 0);
    counts_check(&fs, &disk, image.len(), "quotas on again");
}

#[test]
fn simple_quotas_mkfs_made_count_on() {
    let image = mkfs_empty(&["-O", "squota"]);
    let disk = writable(&image);
    let fs = mount_rw(&disk);
    let before = group(&fs, 5);
    put(&fs.root().create("a", Kind::File, FILE).unwrap(), 0, &noise(1, 300_000));
    counts_check(&fs, &disk, image.len(), "a file on mkfs's simple quotas");
    let after = group(&fs, 5);
    assert!(after.referenced >= before.referenced + 300_000, "{before:?} {after:?}");
}

#[test]
fn full_quota_groups_are_marked_inconsistent() {
    let image = mkfs_empty(&["-O", "quota"]);
    let disk = writable(&image);
    let fs = mount_rw(&disk);
    assert_eq!(fs.quota(true), Err(Error::NotSupported));
    assert_eq!(fs.quota(false), Err(Error::NotSupported));
    assert_eq!(fs.limit_qgroup(5, 1 << 20, u64::MAX), Err(Error::NotSupported));
    assert!(fs.qgroups().unwrap().iter().any(|g| g.id == 5));
    put(&fs.root().create("a", Kind::File, FILE).unwrap(), 0, &noise(1, 300_000));
    fs.sync().unwrap();
    let (whole, text) = btrfs_check(&image_of(&disk, image.len()));
    assert!(whole && text.contains("difference in qgroup accounting is expected"), "{text}");
}
