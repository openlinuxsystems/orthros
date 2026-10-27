// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Host tests of writing Btrfs (ADR 0129): a filesystem `mkfs.btrfs`
//! made is changed through [`Writer`], committed, and then read back by
//! this crate and checked by Linux's `btrfs check`.

use std::collections::BTreeSet;
use std::format;
use std::process::Command;
use std::string::{String, ToString};
use std::vec;
use std::vec::Vec;

use radix_block::DeviceRef;
use radix_block::ram::RamDisk;
use radix_vfs::Error;
use radix_vfs::mem::{self, KArc};

use super::{XorShift, listing, scratch, tool};
use crate::Volume;
use crate::cow::Writer;
use crate::format::{Key, item, name_hash, tree};

/// The top directory's inode number.
const TOP: u64 = 256;

/// The size of the filesystems these tests write.
pub(super) const SIZE: u64 = 128 << 20;

/// An empty filesystem from `mkfs.btrfs args`, `SIZE` bytes.
pub(super) fn mkfs_empty(args: &[&str]) -> Vec<u8> {
    let path = scratch("empty");
    let file = std::fs::File::create(&path).unwrap();
    file.set_len(SIZE).unwrap();
    let out = Command::new(tool("mkfs.btrfs"))
        .args(["-q", "-f"])
        .args(args)
        .arg(&path)
        .stdin(std::process::Stdio::null())
        .output()
        .unwrap();
    assert!(out.status.success(), "mkfs.btrfs: {}", String::from_utf8_lossy(&out.stderr));
    let image = std::fs::read(&path).unwrap();
    let _ = std::fs::remove_file(&path);
    image
}

/// A disk that can be written, holding `image`.
pub(super) fn writable(image: &[u8]) -> KArc<RamDisk> {
    let disk = mem::arc(RamDisk::with_blocks(image.len() as u64 / 512, false, 512)).unwrap();
    disk.load(0, image).unwrap();
    disk
}

/// What the disk holds.
pub(super) fn image_of(disk: &RamDisk, len: usize) -> Vec<u8> {
    let mut out = vec![0; len];
    disk.dump(0, &mut out);
    out
}

/// What `btrfs check` says of `image`: whether it found it whole, and
/// what it printed.
pub(super) fn btrfs_check(image: &[u8]) -> (bool, String) {
    let path = scratch("check");
    write_sparse(&path, image);
    let out = Command::new(tool("btrfs"))
        .args(["check", "--readonly", "--check-data-csum"])
        .arg(&path)
        .stdin(std::process::Stdio::null())
        .output()
        .unwrap();
    let _ = std::fs::remove_file(&path);
    let text =
        format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
    (out.status.success() && !text.contains("ERROR"), text)
}

/// Writes `image` to `path`, leaving its zeros as holes: most of a
/// filesystem these tests write is empty.
fn write_sparse(path: &std::path::Path, image: &[u8]) {
    use std::io::{Seek, SeekFrom, Write};
    let mut file = std::fs::File::create(path).unwrap();
    file.set_len(image.len() as u64).unwrap();
    for (n, chunk) in image.chunks(64 << 10).enumerate() {
        if chunk.iter().any(|&b| b != 0) {
            file.seek(SeekFrom::Start((n * (64 << 10)) as u64)).unwrap();
            file.write_all(chunk).unwrap();
        }
    }
}

pub(super) fn assert_checks(image: &[u8]) {
    let (whole, text) = btrfs_check(image);
    assert!(whole, "btrfs check failed:\n{text}");
}

/// An inode item: a directory or a regular file, made in transaction
/// `generation`, of `size` bytes and `nlink` links.
fn inode_item(generation: u64, mode: u32, size: u64, nlink: u32) -> [u8; 160] {
    let mut b = [0u8; 160];
    b[0..8].copy_from_slice(&generation.to_le_bytes());
    b[8..16].copy_from_slice(&generation.to_le_bytes());
    b[16..24].copy_from_slice(&size.to_le_bytes());
    b[40..44].copy_from_slice(&nlink.to_le_bytes());
    b[52..56].copy_from_slice(&mode.to_le_bytes());
    b
}

/// A directory item naming inode `ino`, a regular file, as `name`.
fn dir_item(ino: u64, generation: u64, name: &[u8]) -> Vec<u8> {
    let mut b = vec![0u8; 30];
    b[0..8].copy_from_slice(&ino.to_le_bytes());
    b[8] = item::INODE_ITEM;
    b[17..25].copy_from_slice(&generation.to_le_bytes());
    b[27..29].copy_from_slice(&(name.len() as u16).to_le_bytes());
    // BTRFS_FT_REG_FILE.
    b[29] = 1;
    b.extend_from_slice(name);
    b
}

/// The files the tests make in the top directory: each name, its inode
/// number and its directory index.
#[derive(Default)]
struct Files {
    names: BTreeSet<String>,
    next_ino: u64,
    next_index: u64,
    /// The top directory's size: twice its names' lengths, as Linux has.
    dir_size: u64,
    entries: Vec<(String, u64, u64)>,
}

impl Files {
    fn new() -> Files {
        Files { next_ino: 257, next_index: 2, ..Files::default() }
    }

    /// Makes an empty regular file `name` in the top directory.
    fn create(&mut self, w: &mut Writer, volume: &Volume, name: &str) -> Result<(), Error> {
        let generation = w.generation;
        let (ino, index) = (self.next_ino, self.next_index);
        let fs = tree::FS;
        w.insert(
            volume,
            fs,
            &Key::new(ino, item::INODE_ITEM, 0),
            &inode_item(generation, 0o100644, 0, 1),
        )?;
        let mut iref = vec![0u8; 10];
        iref[0..8].copy_from_slice(&index.to_le_bytes());
        iref[8..10].copy_from_slice(&(name.len() as u16).to_le_bytes());
        iref.extend_from_slice(name.as_bytes());
        w.insert(volume, fs, &Key::new(ino, item::INODE_REF, TOP), &iref)?;
        let entry = dir_item(ino, generation, name.as_bytes());
        w.insert(volume, fs, &Key::new(TOP, item::DIR_ITEM, name_hash(name.as_bytes())), &entry)?;
        w.insert(volume, fs, &Key::new(TOP, item::DIR_INDEX, index), &entry)?;
        self.dir_size += 2 * name.len() as u64;
        self.set_dir_size(w, volume)?;
        self.next_ino += 1;
        self.next_index += 1;
        self.names.insert(name.to_string());
        self.entries.push((name.to_string(), ino, index));
        Ok(())
    }

    /// Removes the file `entries[at]`.
    fn remove(&mut self, w: &mut Writer, volume: &Volume, at: usize) -> Result<(), Error> {
        let (name, ino, index) = self.entries.swap_remove(at);
        let fs = tree::FS;
        w.delete(volume, fs, &Key::new(ino, item::INODE_ITEM, 0))?;
        w.delete(volume, fs, &Key::new(ino, item::INODE_REF, TOP))?;
        w.delete(volume, fs, &Key::new(TOP, item::DIR_ITEM, name_hash(name.as_bytes())))?;
        w.delete(volume, fs, &Key::new(TOP, item::DIR_INDEX, index))?;
        self.dir_size -= 2 * name.len() as u64;
        self.set_dir_size(w, volume)?;
        self.names.remove(&name);
        Ok(())
    }

    fn set_dir_size(&self, w: &mut Writer, volume: &Volume) -> Result<(), Error> {
        let size = self.dir_size;
        let generation = w.generation;
        w.update(volume, tree::FS, &Key::new(TOP, item::INODE_ITEM, 0), |data| {
            data[8..16].copy_from_slice(&generation.to_le_bytes());
            data[16..24].copy_from_slice(&size.to_le_bytes());
        })
    }
}

fn open(disk: &KArc<RamDisk>) -> Volume {
    let device: DeviceRef = disk.clone();
    Volume::open(device).unwrap()
}

fn names(volume: &Volume) -> BTreeSet<String> {
    listing(volume).unwrap()
}

#[test]
fn a_commit_with_nothing_changed_writes_nothing() {
    let image = mkfs_empty(&[]);
    let disk = writable(&image);
    let volume = open(&disk);
    let mut w = Writer::new(&volume).unwrap();
    let before = disk.requests();
    w.commit(&volume).unwrap();
    assert_eq!(disk.requests(), before);
    assert_eq!(image_of(&disk, image.len()), image);
}

#[test]
fn files_made_and_committed_read_back_and_pass_btrfs_check() {
    let image = mkfs_empty(&[]);
    let disk = writable(&image);
    let mut files = Files::new();
    {
        let volume = open(&disk);
        let mut w = Writer::new(&volume).unwrap();
        for n in 0..10 {
            files.create(&mut w, &volume, &format!("file-{n}")).unwrap();
        }
        // Readers see the transaction's changes before its commit.
        assert_eq!(names(&volume), files.names);
        w.commit(&volume).unwrap();
        assert_eq!(names(&volume), files.names);
    }
    let written = image_of(&disk, image.len());
    assert_checks(&written);
    // And the disk holds them, opened afresh.
    let volume = open(&disk);
    assert_eq!(names(&volume), files.names);
    // A second commit's flush takes the first one's copy of the
    // superblock at 64 MiB to the disk: it leads to whole trees too.
    let mut w = Writer::new(&volume).unwrap();
    files.create(&mut w, &volume, "second").unwrap();
    w.commit(&volume).unwrap();
    let path = scratch("mirror");
    write_sparse(&path, &image_of(&disk, image.len()));
    let out = Command::new(tool("btrfs"))
        .args(["check", "--readonly", "--super", "1"])
        .arg(&path)
        .stdin(std::process::Stdio::null())
        .output()
        .unwrap();
    let _ = std::fs::remove_file(&path);
    assert!(
        out.status.success(),
        "btrfs check --super 1: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn a_block_group_kept_as_bitmaps_stays_bitmaps_and_whole() {
    use crate::chunk::kind;
    use crate::format::u64_at;
    use crate::space::read_free;
    use crate::tree::{Cursor, find};
    let image = mkfs_empty(&[]);
    let disk = writable(&image);
    let volume = open(&disk);
    let mut w = Writer::new(&volume).unwrap();
    // The metadata block group, which every tree block comes from.
    let extents = volume.tree(tree::EXTENT).unwrap().unwrap();
    let mut cursor = Cursor::seek(&volume, &extents, &Key::new(0, 0, 0)).unwrap();
    let mut group = None;
    while let Some((key, data)) = cursor.get() {
        if key.kind == item::BLOCK_GROUP_ITEM && u64_at(data, 16) & kind::METADATA != 0 {
            group = Some((key.objectid, key.offset));
        }
        cursor.advance().unwrap();
    }
    drop(cursor);
    let (start, len) = group.unwrap();
    // Linux turns a group's extents into bitmaps of 8 MiB each when they
    // are many; done here by hand.
    let free_tree = volume.tree(tree::FREE_SPACE).unwrap().unwrap();
    let (free, bitmaps) = read_free(&volume, &free_tree, start, len).unwrap();
    assert!(!bitmaps);
    for &(s, e) in &free {
        w.delete(&volume, tree::FREE_SPACE, &Key::new(s, item::FREE_SPACE_EXTENT, e - s)).unwrap();
    }
    let sector = 4096u64;
    let span = 2048 * sector;
    let mut at = start;
    while at < start + len {
        let end = (at + span).min(start + len);
        let mut bits = vec![0u8; ((end - at) / sector).div_ceil(8) as usize];
        for &(s, e) in &free {
            let mut x = s.max(at);
            while x < e.min(end) {
                let bit = (x - at) / sector;
                bits[(bit / 8) as usize] |= 1 << (bit % 8);
                x += sector;
            }
        }
        w.insert(
            &volume,
            tree::FREE_SPACE,
            &Key::new(at, item::FREE_SPACE_BITMAP, end - at),
            &bits,
        )
        .unwrap();
        at = end;
    }
    let info = Key::new(start, item::FREE_SPACE_INFO, len);
    w.update(&volume, tree::FREE_SPACE, &info, |data| data[4] |= 1).unwrap();
    w.commit(&volume).unwrap();
    assert_checks(&image_of(&disk, image.len()));
    let mut files = Files::new();
    for n in 0..300 {
        files.create(&mut w, &volume, &format!("bits-{n}-{}", "b".repeat(50))).unwrap();
    }
    w.commit(&volume).unwrap();
    assert_checks(&image_of(&disk, image.len()));
    let volume = open(&disk);
    assert_eq!(names(&volume), files.names);
    let free_tree = volume.tree(tree::FREE_SPACE).unwrap().unwrap();
    let info = find(&volume, &free_tree, &info).unwrap().unwrap();
    assert_eq!(info[4] & 1, 1, "the group is still kept as bitmaps");
}

#[test]
fn a_filesystem_with_a_snapshot_is_not_written_yet() {
    // A snapshot made the way `btrfs subvolume snapshot` keys it: by the
    // transaction that made it. Its blocks are shared (10c.2).
    let image = mkfs_empty(&[]);
    let disk = writable(&image);
    let volume = open(&disk);
    let mut w = Writer::new(&volume).unwrap();
    let fs_key = Key::new(tree::FS, item::ROOT_ITEM, 0);
    let root_tree = volume.root_tree();
    let fs_item = crate::tree::find(&volume, &root_tree, &fs_key).unwrap().unwrap();
    let mut item = fs_item.clone();
    item[200..208].copy_from_slice(&5u64.to_le_bytes());
    w.update(&volume, tree::ROOT, &fs_key, |data| data.copy_from_slice(&item)).unwrap();
    w.commit(&volume).unwrap();
    let volume = open(&disk);
    assert_eq!(Writer::new(&volume).err(), Some(Error::NotSupported));
}

#[test]
fn thousands_of_files_split_every_level_and_deleting_them_frees_their_blocks() {
    // Blocks of 4 KiB hold 121 key pointers: the tree soon has nodes
    // above nodes.
    let image = mkfs_empty(&["--nodesize", "4096"]);
    let disk = writable(&image);
    let mut files = Files::new();
    let volume = open(&disk);
    let mut w = Writer::new(&volume).unwrap();
    // Long names fill leaves fast.
    let long = "x".repeat(200);
    for n in 0..3000 {
        files.create(&mut w, &volume, &format!("{long}-{n:05}")).unwrap();
        if n % 1000 == 999 {
            w.commit(&volume).unwrap();
        }
    }
    let fs = volume.tree(tree::FS).unwrap().unwrap();
    assert!(fs.level >= 2, "the tree stayed at level {}", fs.level);
    assert_eq!(names(&volume), files.names);
    assert_checks(&image_of(&disk, image.len()));
    let mut random = XorShift(0x5eed);
    while files.entries.len() > 10 {
        let at = random.below(files.entries.len());
        files.remove(&mut w, &volume, at).unwrap();
        if files.entries.len().is_multiple_of(700) {
            w.commit(&volume).unwrap();
        }
    }
    w.commit(&volume).unwrap();
    assert_eq!(names(&volume), files.names);
    let written = image_of(&disk, image.len());
    assert_checks(&written);
    let fs = volume.tree(tree::FS).unwrap().unwrap();
    // Ten files' items fill three leaves of 4 KiB: merged, they hang from
    // one node again.
    assert_eq!(fs.level, 1, "the emptied leaves were not merged");
    assert_eq!(names(&open(&disk)), files.names);
}

#[test]
fn a_change_already_there_or_missing_fails_and_changes_nothing() {
    let image = mkfs_empty(&[]);
    let disk = writable(&image);
    let volume = open(&disk);
    let mut w = Writer::new(&volume).unwrap();
    let mut files = Files::new();
    files.create(&mut w, &volume, "one").unwrap();
    let key = Key::new(257, item::INODE_ITEM, 0);
    assert_eq!(w.insert(&volume, tree::FS, &key, &[0; 160]), Err(Error::Exists));
    let missing = Key::new(999, item::INODE_ITEM, 0);
    assert_eq!(w.delete(&volume, tree::FS, &missing), Err(Error::NotFound));
    assert_eq!(w.update(&volume, tree::FS, &missing, |_| ()), Err(Error::NotFound));
    w.commit(&volume).unwrap();
    assert_checks(&image_of(&disk, image.len()));
}

#[test]
fn every_checksum_and_node_size_writes_what_btrfs_check_passes() {
    for args in [
        &["--csum", "xxhash"][..],
        &["--csum", "sha256"],
        &["--csum", "blake2"],
        &["--nodesize", "4096"],
        &["--nodesize", "65536"],
        &["--metadata", "single"],
        &["-O", "block-group-tree"],
    ] {
        let image = mkfs_empty(args);
        let disk = writable(&image);
        let volume = open(&disk);
        let mut w = Writer::new(&volume).unwrap();
        let mut files = Files::new();
        for n in 0..400 {
            files.create(&mut w, &volume, &format!("{n:04}-{}", "y".repeat(60))).unwrap();
        }
        w.commit(&volume).unwrap();
        for at in (0..400).step_by(3).rev() {
            if at < files.entries.len() {
                files.remove(&mut w, &volume, at).unwrap();
            }
        }
        w.commit(&volume).unwrap();
        let (whole, text) = btrfs_check(&image_of(&disk, image.len()));
        assert!(whole, "mkfs.btrfs {args:?}: btrfs check failed:\n{text}");
        assert_eq!(names(&open(&disk)), files.names, "mkfs.btrfs {args:?}");
    }
}

/// What a [`CrashDisk`] was asked to do.
#[derive(Clone)]
pub(super) enum Op {
    /// A write into the disk's cache: durable at the next flush.
    Write(u64, Vec<u8>),
    /// A flush.
    Flush,
    /// A write through to the medium, durable when it returns.
    Through(u64, Vec<u8>),
}

/// A disk with a write cache that loses power: it records every write
/// and flush, so that what a cut at any point leaves on the medium can be
/// made afterwards ([`CrashDisk::cut`]).
pub(super) struct CrashDisk {
    base: Vec<u8>,
    now: std::sync::Mutex<Vec<u8>>,
    log: std::sync::Mutex<Vec<Op>>,
}

impl CrashDisk {
    pub(super) fn new(image: &[u8]) -> CrashDisk {
        CrashDisk {
            base: image.to_vec(),
            now: std::sync::Mutex::new(image.to_vec()),
            log: std::sync::Mutex::new(Vec::new()),
        }
    }

    /// What the medium holds after a cut before op `at`: every write before
    /// the last flush, and each one after it as `keep` says; and how many
    /// writes through it holds.
    pub(super) fn cut(&self, at: usize, mut keep: impl FnMut() -> bool) -> (Vec<u8>, usize) {
        let log = self.log.lock().unwrap();
        let mut medium = self.base.clone();
        let mut cache: Vec<(u64, Vec<u8>)> = Vec::new();
        let mut throughs = 0;
        let put = |medium: &mut Vec<u8>, offset: u64, data: &[u8]| {
            medium[offset as usize..offset as usize + data.len()].copy_from_slice(data);
        };
        for op in &log[..at] {
            match op {
                Op::Write(offset, data) => cache.push((*offset, data.clone())),
                Op::Flush => {
                    for (offset, data) in cache.drain(..) {
                        put(&mut medium, offset, &data);
                    }
                }
                Op::Through(offset, data) => {
                    put(&mut medium, *offset, data);
                    throughs += 1;
                }
            }
        }
        for (offset, data) in cache {
            if keep() {
                put(&mut medium, offset, &data);
            }
        }
        (medium, throughs)
    }

    pub(super) fn ops(&self) -> Vec<Op> {
        self.log.lock().unwrap().clone()
    }

    fn write_at(&self, sector: u64, segments: &[radix_block::Segment<'_>]) -> (u64, Vec<u8>) {
        let mut data = Vec::new();
        for s in segments {
            data.extend_from_slice(&s.page.bytes()[s.offset..s.offset + s.len]);
        }
        let offset = sector * radix_block::SECTOR;
        let mut now = self.now.lock().unwrap();
        now[offset as usize..offset as usize + data.len()].copy_from_slice(&data);
        (offset, data)
    }
}

impl radix_block::BlockDevice for CrashDisk {
    fn info(&self) -> radix_block::Info {
        let sectors = self.base.len() as u64 / radix_block::SECTOR;
        radix_block::Info {
            sectors,
            logical_block: 512,
            block_size: 512,
            read_only: false,
            max_segments: 64,
            discard_sectors: 0,
            rotational: false,
        }
    }

    fn read(
        &self,
        sector: u64,
        segments: &mut [radix_block::SegmentMut<'_>],
    ) -> Result<(), radix_block::Error> {
        let now = self.now.lock().unwrap();
        let mut at = (sector * radix_block::SECTOR) as usize;
        for s in segments.iter_mut() {
            s.page.bytes_mut()[s.offset..s.offset + s.len].copy_from_slice(&now[at..at + s.len]);
            at += s.len;
        }
        Ok(())
    }

    fn write(
        &self,
        sector: u64,
        segments: &[radix_block::Segment<'_>],
    ) -> Result<(), radix_block::Error> {
        let (offset, data) = self.write_at(sector, segments);
        self.log.lock().unwrap().push(Op::Write(offset, data));
        Ok(())
    }

    fn flush(&self) -> Result<(), radix_block::Error> {
        self.log.lock().unwrap().push(Op::Flush);
        Ok(())
    }

    fn write_through(
        &self,
        sector: u64,
        segments: &[radix_block::Segment<'_>],
    ) -> Result<(), radix_block::Error> {
        let (offset, data) = self.write_at(sector, segments);
        self.log.lock().unwrap().push(Op::Through(offset, data));
        Ok(())
    }
}

#[test]
fn a_power_cut_anywhere_leaves_the_last_durable_commit_whole() {
    let image = mkfs_empty(&[]);
    let disk = mem::arc(CrashDisk::new(&image)).unwrap();
    let mut committed = vec![BTreeSet::new()];
    {
        let device: DeviceRef = disk.clone();
        let volume = Volume::open(device).unwrap();
        let mut w = Writer::new(&volume).unwrap();
        let mut files = Files::new();
        let mut random = XorShift(0xc0ffee);
        for round in 0..4 {
            for n in 0..250 {
                files
                    .create(&mut w, &volume, &format!("r{round}-{n:03}-{}", "z".repeat(40)))
                    .unwrap();
            }
            for _ in 0..(round * 60) {
                let at = random.below(files.entries.len());
                files.remove(&mut w, &volume, at).unwrap();
            }
            w.commit(&volume).unwrap();
            committed.push(files.names.clone());
        }
    }
    let ops = disk.ops();
    // Cut at every flush and write through, on either side, and at random
    // writes between them.
    let mut cuts: BTreeSet<usize> = BTreeSet::new();
    let mut random = XorShift(0xdecade);
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
        let ram = writable(&medium);
        assert_eq!(
            names(&open(&ram)),
            committed[durable],
            "a cut before op {cut} of {} holds commit {durable}",
            ops.len()
        );
    }
}
