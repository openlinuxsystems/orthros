// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Host tests of the ext4 driver, against e2fsprogs, Linux's own tools
//! (`apt install e2fsprogs`; `ob doctor` checks for it): volumes are made
//! by `mkfs.ext4`, what Radix writes is checked by `e2fsck` and read back
//! by `debugfs`, and a volume Linux's kernel wrote, with transactions left
//! in its journal, is replayed ([`LINUX_JOURNAL`]). A power cut is a disk
//! that stops taking writes part way ([`RamDisk::fail_after`]): what it
//! holds must mount, replay, and pass `e2fsck`.

use std::collections::BTreeMap;
use std::os::unix::fs::symlink;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::string::{String, ToString};
use std::sync::atomic::{AtomicU32, Ordering};
use std::vec::Vec;
use std::{format, vec};

use proptest::prelude::*;
use radix_block::ram::RamDisk;
use radix_block::{BlockDevice, DeviceRef};
use radix_vfs::mem::{self, KArc};
use radix_vfs::{Error, FileSystem, Kind, NodeRef, Slice, SliceMut};

use crate::Ext4Fs;
use crate::inode::flags;
use crate::sb::{self, at, incompat};
use crate::{le16, le32};

/// 2026-10-22 00:42:22 UTC, for files' times.
fn clock() -> u64 {
    1_792_629_742_000_000_000
}

/// A tool of e2fsprogs.
fn tool(name: &str) -> PathBuf {
    for dir in ["/usr/sbin", "/sbin", "/usr/bin", "/bin", "/usr/local/bin", "/usr/local/sbin"] {
        let path = PathBuf::from(dir).join(name);
        if path.exists() {
            return path;
        }
    }
    panic!("{name} is missing: install e2fsprogs (apt install e2fsprogs)");
}

/// A path for a scratch image or directory.
fn scratch(what: &str) -> PathBuf {
    static NEXT: AtomicU32 = AtomicU32::new(0);
    let n = NEXT.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!("radix-ext4-{}-{what}-{n}", std::process::id()))
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

fn run(command: &mut Command) -> (bool, String) {
    let out = command.output().unwrap();
    let text =
        format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
    (out.status.success(), text)
}

/// The kinds of volume the tests use: a name, its size in MiB, and what
/// `mkfs.ext4` is given for it.
const VOLUMES: [(&str, u32, &[&str]); 13] = [
    ("4K blocks", 32, &[]),
    ("1K blocks", 16, &["-b", "1024"]),
    ("2K blocks", 24, &["-b", "2048"]),
    ("group checksums", 32, &["-O", "^metadata_csum,uninit_bg"]),
    ("no checksums", 16, &["-O", "^metadata_csum,^uninit_bg"]),
    ("32-bit", 32, &["-O", "^64bit"]),
    ("128-byte inodes", 16, &["-I", "128"]),
    ("no journal", 16, &["-O", "^has_journal"]),
    ("meta_bg", 48, &["-b", "1024", "-O", "meta_bg,^resize_inode,^64bit"]),
    ("no flex_bg", 32, &["-O", "^flex_bg"]),
    ("orphan file", 32, &["-O", "orphan_file"]),
    (
        "checksum seed",
        32,
        &["-O", "metadata_csum_seed", "-U", "12345678-9abc-def0-1234-56789abcdef0"],
    ),
    ("small groups", 24, &["-g", "1024", "-b", "1024", "-N", "2048"]),
];

/// A fresh volume from `mkfs.ext4`, labelled `RADIXTEST`, of `mib` MiB,
/// holding a copy of `from` if given.
fn mkfs_from(mib: u32, args: &[&str], from: Option<&Path>) -> Vec<u8> {
    let path = scratch("mkfs");
    let file = std::fs::File::create(&path).unwrap();
    file.set_len(u64::from(mib) << 20).unwrap();
    drop(file);
    let mut command = Command::new(tool("mkfs.ext4"));
    command
        .args(["-q", "-F", "-L", "RADIXTEST", "-E", "lazy_itable_init=1,root_owner=0:0"])
        .args(args);
    if let Some(from) = from {
        command.arg("-d").arg(from);
    }
    let (ok, text) = run(command.arg(&path));
    assert!(ok, "mkfs.ext4 {args:?}: {text}");
    let image = std::fs::read(&path).unwrap();
    let _ = std::fs::remove_file(&path);
    image
}

fn mkfs(mib: u32, args: &[&str]) -> Vec<u8> {
    mkfs_from(mib, args, None)
}

/// What `e2fsck -fn` says of `image`, and whether it found it clean.
fn fsck_report(image: &[u8]) -> (bool, String) {
    let path = scratch("fsck");
    std::fs::write(&path, image).unwrap();
    let report = run(Command::new(tool("e2fsck")).arg("-fn").arg(&path));
    let _ = std::fs::remove_file(&path);
    report
}

/// Checks `image` with `e2fsck`, which must find nothing wrong.
#[track_caller]
fn fsck(image: &[u8]) {
    let (clean, text) = fsck_report(image);
    assert!(clean, "e2fsck found problems:\n{text}");
}

/// `image` with its journal replayed by `e2fsck`, as Linux would, and
/// nothing else changed.
fn e2fsck_replay(image: &[u8]) -> Vec<u8> {
    let path = scratch("replay");
    std::fs::write(&path, image).unwrap();
    let (_, text) = run(Command::new(tool("e2fsck")).args(["-y", "-E", "journal_only"]).arg(&path));
    let replayed = std::fs::read(&path).unwrap();
    let _ = std::fs::remove_file(&path);
    assert!(!text.contains("rror"), "e2fsck replaying the journal: {text}");
    replayed
}

/// Runs `debugfs` commands on `image`, which it may change if `write`.
fn debugfs(image: &mut Vec<u8>, write: bool, commands: &str) -> String {
    let path = scratch("debugfs");
    std::fs::write(&path, &*image).unwrap();
    let mut command = Command::new(tool("debugfs"));
    if write {
        command.arg("-w");
    }
    let (ok, text) = run(command.arg("-R").arg(commands).arg(&path));
    assert!(ok, "debugfs {commands}: {text}");
    *image = std::fs::read(&path).unwrap();
    let _ = std::fs::remove_file(&path);
    text
}

/// What a tree holds, by path: a file's bytes, a directory, or a link's
/// target.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Thing {
    File(Vec<u8>),
    Dir,
    Link(String),
}

type Tree = BTreeMap<String, Thing>;

/// What a directory of the host holds.
fn host_tree(dir: &Path) -> Tree {
    fn walk(dir: &Path, prefix: &str, out: &mut Tree) {
        for entry in std::fs::read_dir(dir).unwrap() {
            let entry = entry.unwrap();
            let name = entry.file_name().into_string().unwrap();
            let path = format!("{prefix}{name}");
            let kind = entry.file_type().unwrap();
            if kind.is_symlink() {
                let target = std::fs::read_link(entry.path()).unwrap();
                out.insert(path, Thing::Link(target.into_os_string().into_string().unwrap()));
            } else if kind.is_dir() {
                out.insert(path.clone(), Thing::Dir);
                walk(&entry.path(), &format!("{path}/"), out);
            } else {
                out.insert(path, Thing::File(std::fs::read(entry.path()).unwrap()));
            }
        }
    }
    let mut out = Tree::new();
    walk(dir, "", &mut out);
    out
}

/// Every file of `image` but `lost+found`, as `debugfs` reads it.
fn debugfs_tree(image: &[u8]) -> Tree {
    let out = scratch("rdump");
    std::fs::create_dir_all(&out).unwrap();
    let mut image = image.to_vec();
    debugfs(&mut image, false, &format!("rdump / {}", out.display()));
    let mut tree = host_tree(&out);
    let _ = std::fs::remove_dir_all(&out);
    tree.retain(|path, _| !path.starts_with("lost+found"));
    tree
}

/// Writes `tree` into a new directory of the host, for `mkfs.ext4 -d`.
fn host_dir(tree: &Tree) -> PathBuf {
    let dir = scratch("tree");
    std::fs::create_dir_all(&dir).unwrap();
    for (path, thing) in tree {
        let at = dir.join(path);
        match thing {
            Thing::Dir => std::fs::create_dir_all(&at).unwrap(),
            Thing::File(data) => std::fs::write(&at, data).unwrap(),
            Thing::Link(target) => symlink(target, &at).unwrap(),
        }
    }
    dir
}

/// A volume on a RAM disk.
struct Volume {
    disk: KArc<RamDisk>,
    fs: Option<KArc<Ext4Fs>>,
}

impl Volume {
    fn new(image: &[u8]) -> Volume {
        Volume::mount_in(512, image, false).unwrap()
    }

    fn mount(image: &[u8], read_only: bool) -> Result<Volume, Error> {
        Volume::mount_in(512, image, read_only)
    }

    /// The volume in `image` on a disk of logical blocks of `block` bytes.
    fn mount_in(block: u32, image: &[u8], read_only: bool) -> Result<Volume, Error> {
        radix_vfs::set_clock(clock);
        let sectors = image.len() as u64 / 512;
        let disk = mem::arc(RamDisk::with_blocks(sectors, false, block)).unwrap();
        disk.load(0, image).unwrap();
        let device: DeviceRef = disk.clone();
        let fs = Ext4Fs::mount(device, read_only)?;
        Ok(Volume { disk, fs: Some(fs) })
    }

    fn fs(&self) -> &Ext4Fs {
        self.fs.as_ref().unwrap()
    }

    fn root(&self) -> NodeRef {
        self.fs().root()
    }

    /// The disk's bytes as they are.
    fn raw(&self) -> Vec<u8> {
        let mut image = vec![0u8; (self.disk.info().sectors * 512) as usize];
        self.disk.dump(0, &mut image);
        image
    }

    /// The disk's bytes, after a sync: the volume is still mounted.
    fn synced(&self) -> Vec<u8> {
        self.fs().sync().unwrap();
        self.raw()
    }

    /// The disk's bytes after an unmount.
    fn unmount(mut self) -> Vec<u8> {
        drop(self.fs.take());
        self.raw()
    }

    /// The volume, unmounted and mounted again from its bytes.
    fn remount(self) -> Volume {
        let image = self.unmount();
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

/// The directory `path` names the parent of, and its last name.
fn parent<'a>(root: &NodeRef, path: &'a str) -> (NodeRef, &'a str) {
    match path.rsplit_once('/') {
        Some((dir, name)) => (walk(root, dir).unwrap(), name),
        None => (root.clone(), path),
    }
}

fn write_all(node: &NodeRef, offset: u64, data: &[u8]) -> Result<(), Error> {
    let n = node.write(offset, &Slice(data))?;
    assert_eq!(n, data.len());
    Ok(())
}

fn read_all(node: &NodeRef) -> Vec<u8> {
    let size = node.stat().size as usize;
    let mut out = vec![0u8; size + 100];
    let n = node.read(0, &mut SliceMut(&mut out)).unwrap();
    assert_eq!(n, size);
    out.truncate(n);
    out
}

fn link_of(node: &NodeRef) -> String {
    let mut out = vec![0u8; 5000];
    let n = node.read_link(&mut SliceMut(&mut out)).unwrap();
    out.truncate(n);
    String::from_utf8(out).unwrap()
}

/// A file `name` in `dir` holding `data`.
fn make_file(dir: &NodeRef, name: &str, data: &[u8]) -> NodeRef {
    let node = dir.create(name, Kind::File).unwrap();
    if !data.is_empty() {
        write_all(&node, 0, data).unwrap();
    }
    node
}

/// Puts `tree` under `root` with Radix.
fn put_tree(root: &NodeRef, tree: &Tree) {
    for (path, thing) in tree {
        let (dir, name) = parent(root, path);
        match thing {
            Thing::Dir => drop(dir.create(name, Kind::Directory).unwrap()),
            Thing::File(data) => drop(make_file(&dir, name, data)),
            Thing::Link(target) => drop(dir.make_link(name, target).unwrap()),
        }
    }
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

/// What Radix reads under `root`, but `lost+found`.
fn radix_tree(root: &NodeRef) -> Tree {
    fn walk(dir: &NodeRef, prefix: &str, out: &mut Tree) {
        for (name, kind) in names(dir) {
            let path = format!("{prefix}{name}");
            let node = dir.lookup(&name).unwrap();
            assert_eq!(node.kind(), kind, "{path}: its entry's kind");
            match kind {
                Kind::Directory => {
                    out.insert(path.clone(), Thing::Dir);
                    walk(&node, &format!("{path}/"), out);
                }
                Kind::Symlink => {
                    out.insert(path, Thing::Link(link_of(&node)));
                }
                _ => {
                    out.insert(path, Thing::File(read_all(&node)));
                }
            }
        }
    }
    let mut out = Tree::new();
    walk(root, "", &mut out);
    out.retain(|path, _| !path.starts_with("lost+found"));
    out
}

#[track_caller]
fn same_trees(seen: &Tree, want: &Tree, who: &str) {
    let seen_paths: Vec<_> = seen.keys().collect();
    let want_paths: Vec<_> = want.keys().collect();
    assert_eq!(seen_paths, want_paths, "the paths {who} sees");
    for (path, thing) in want {
        assert!(seen.get(path) == Some(thing), "{path}, as {who} reads it");
    }
}

/// Names of every kind: short, long, of 255 bytes, non-ASCII, and with
/// spaces.
fn sample_names() -> Vec<String> {
    vec![
        String::from("README"),
        String::from("notes.txt"),
        String::from(".profile"),
        String::from("A long file name with spaces.markdown"),
        String::from("Ünïcödé ✓.txt"),
        String::from("x"),
        String::from("emoji \u{1f600}\u{1f680}.bin"),
        "n".repeat(255),
    ]
}

/// File sizes around block, page and extent edges.
const SIZES: [usize; 10] = [0, 1, 1023, 1024, 4095, 4096, 4097, 33_000, 300_001, 1_100_000];

/// A tree of files, directories and links, of every size and name.
fn sample_tree(seed: u32) -> Tree {
    let mut tree = Tree::new();
    tree.insert(String::from("Documents"), Thing::Dir);
    tree.insert(String::from("Documents/deeper"), Thing::Dir);
    tree.insert(String::from("Documents/deeper/still"), Thing::Dir);
    tree.insert(String::from("empty"), Thing::Dir);
    let dirs = ["", "Documents/", "Documents/deeper/", "Documents/deeper/still/"];
    for (i, name) in sample_names().iter().enumerate() {
        for (k, size) in SIZES.iter().enumerate() {
            if (i + k) % 3 != 0 {
                continue;
            }
            let path = format!("{}{k}-{}", dirs[(i + k) % 4], &name[..name.len().min(250)]);
            tree.insert(path, Thing::File(pattern(seed + (i * 16 + k) as u32, *size)));
        }
    }
    tree.insert(String::from("short link"), Thing::Link(String::from("Documents/deeper")));
    tree.insert(String::from("Documents/long link"), Thing::Link("../".repeat(30) + "far/away"));
    tree.insert(String::from("dangling"), Thing::Link(String::from("nowhere")));
    tree
}

#[test]
fn volumes_mount() {
    for (what, mib, args) in VOLUMES {
        let volume = Volume::new(&mkfs(mib, args));
        let mut label = String::new();
        volume.fs().label(&mut label).unwrap();
        assert_eq!(label, "RADIXTEST", "{what}");
        let info = volume.fs().info();
        assert!(info.free > 0 && info.free < info.blocks, "{what}: {info:?}");
        assert!(info.free_files > 0 && info.free_files < info.files, "{what}: {info:?}");
        assert_eq!(
            names(&volume.root()),
            [(String::from("lost+found"), Kind::Directory)],
            "{what}"
        );
        assert_eq!(volume.fs().kind().name(), "ext4");
        let image = volume.unmount();
        fsck(&image);
    }
}

#[test]
fn what_linux_made_reads_back() {
    let tree = sample_tree(1);
    let dir = host_dir(&tree);
    for (what, mib, args) in VOLUMES {
        let image = mkfs_from(mib, args, Some(&dir));
        let volume = Volume::mount(&image, true).unwrap();
        same_trees(&radix_tree(&volume.root()), &tree, &format!("Radix, on {what}"));
    }
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn what_radix_writes_passes_e2fsck_and_reads_back() {
    for (what, mib, args) in VOLUMES {
        let tree = sample_tree(7);
        let volume = Volume::new(&mkfs(mib, args));
        put_tree(&volume.root(), &tree);
        let image = volume.unmount();
        let (clean, text) = fsck_report(&image);
        assert!(clean, "{what}: e2fsck found problems:\n{text}");
        same_trees(&debugfs_tree(&image), &tree, &format!("debugfs, on {what}"));
        let volume = Volume::new(&image);
        same_trees(&radix_tree(&volume.root()), &tree, &format!("Radix again, on {what}"));
    }
}

#[test]
fn changes_to_what_linux_made_pass_e2fsck() {
    let tree = sample_tree(3);
    let dir = host_dir(&tree);
    for (what, mib, args) in VOLUMES {
        let image = mkfs_from(mib, args, Some(&dir));
        let volume = Volume::new(&image);
        let root = volume.root();
        let mut want = tree.clone();
        // Files grow, shrink, are overwritten, and go.
        let paths: Vec<String> = want.keys().cloned().collect();
        for (i, path) in paths.iter().enumerate() {
            let Some(Thing::File(data)) = want.get(path).cloned() else { continue };
            let node = walk(&root, path).unwrap();
            let mut data = data;
            match i % 5 {
                0 => {
                    let more = pattern(i as u32, 5000);
                    write_all(&node, data.len() as u64, &more).unwrap();
                    data.extend_from_slice(&more);
                }
                1 => {
                    let keep = data.len() / 3;
                    node.set_size(keep as u64).unwrap();
                    data.truncate(keep);
                }
                2 if data.len() > 10 => {
                    let at = data.len() / 2;
                    let new = pattern(1000 + i as u32, data.len() - at + 77);
                    write_all(&node, at as u64, &new).unwrap();
                    data.truncate(at);
                    data.extend_from_slice(&new);
                }
                3 => {
                    let (dir, name) = parent(&root, path);
                    dir.remove(name).unwrap();
                    want.remove(path);
                    continue;
                }
                _ => {}
            }
            want.insert(path.clone(), Thing::File(data));
        }
        // A directory moves, a file is renamed over another, and an empty
        // directory goes.
        let docs = walk(&root, "Documents").unwrap();
        docs.rename("deeper", &*root, "moved").unwrap();
        let moved: Vec<(String, Thing)> = want
            .iter()
            .filter(|(p, _)| p.starts_with("Documents/deeper"))
            .map(|(p, t)| (p.replacen("Documents/deeper", "moved", 1), t.clone()))
            .collect();
        want.retain(|p, _| !p.starts_with("Documents/deeper"));
        want.extend(moved);
        root.remove("empty").unwrap();
        want.remove("empty");
        make_file(&root, "over", b"old");
        make_file(&docs, "new", b"the new one");
        docs.rename("new", &*root, "over").unwrap();
        want.insert(String::from("over"), Thing::File(b"the new one".to_vec()));
        drop((root, docs));
        let image = volume.unmount();
        let (clean, text) = fsck_report(&image);
        assert!(clean, "{what}: e2fsck found problems:\n{text}");
        same_trees(&debugfs_tree(&image), &want, &format!("debugfs, on {what}"));
        let volume = Volume::new(&image);
        same_trees(&radix_tree(&volume.root()), &want, &format!("Radix again, on {what}"));
    }
    let _ = std::fs::remove_dir_all(&dir);
}

/// A volume of 16 MiB in 1 KiB blocks that Linux 6.18's ext4 wrote,
/// labelled `LINUXJNL`, with transactions committed to its journal but not
/// to their homes: `mkfs.ext4 -b 1024` made it, and Linux mounted it
/// through a loop device, filled it, synced, changed it more (removing a
/// directory of 30 files, and growing one file), `fsync`ed what it
/// changed, and was shut down with `EXT4_IOC_SHUTDOWN` (`LOGFLUSH`). The
/// disk was copied then: unmounting would still have written the
/// metadata's homes from the block device's cache. [`linux_journal_files`]
/// are what the journal holds; without it the volume is as the sync left
/// it. Its sectors that are not all zeros: a count of sectors, then each
/// such sector's number and bytes, the numbers as little-endian `u32`s.
const LINUX_JOURNAL: &[u8] = include_bytes!("fixtures/linux-journal.sectors");

/// [`LINUX_JOURNAL`]'s volume.
fn linux_journal() -> Vec<u8> {
    let word = |at: &[u8]| u32::from_le_bytes(at[..4].try_into().unwrap()) as usize;
    let mut image = vec![0u8; word(LINUX_JOURNAL) * 512];
    for record in LINUX_JOURNAL[4..].as_chunks::<{ 4 + 512 }>().0 {
        let sector = word(record);
        image[sector * 512..(sector + 1) * 512].copy_from_slice(&record[4..]);
    }
    image
}

/// What Linux had written to [`LINUX_JOURNAL`]'s volume when it stopped.
fn linux_journal_files() -> Tree {
    let mut tree = Tree::new();
    tree.insert(String::from("hello.txt"), Thing::File(b"hello from Linux\nand more\n".to_vec()));
    tree.insert(String::from("dir"), Thing::Dir);
    for i in 0..60u32 {
        tree.insert(
            format!("dir/file {i:02}.txt"),
            Thing::File(pattern(100 + i, (7 * i + 5) as usize)),
        );
    }
    tree.insert(String::from("big.bin"), Thing::File(pattern(1, 20001)));
    tree.insert(String::from("link"), Thing::Link(String::from("dir/file 07.txt")));
    tree.insert(String::from("longlink"), Thing::Link(format!("dir/{}", "x".repeat(120))));
    tree.insert(String::from("after.txt"), Thing::File(pattern(9, 3000)));
    tree.insert(String::from("empty dir"), Thing::Dir);
    tree
}

#[test]
fn linux_journal_needs_replay() {
    let image = linux_journal();
    let incompat = le32(&image, sb::OFFSET as usize + at::INCOMPAT);
    assert_ne!(incompat & incompat::RECOVER, 0, "the journal holds transactions");
    // Without them, the volume is as the sync left it: the last changes
    // are in the journal only.
    let before = debugfs_tree(&image);
    assert!(
        before.contains_key("gone/f29") && !before.contains_key("after.txt"),
        "{:?}",
        before.keys()
    );
    // e2fsprogs' own replay gives the files.
    let replayed = e2fsck_replay(&image);
    fsck(&replayed);
    same_trees(&debugfs_tree(&replayed), &linux_journal_files(), "debugfs, after e2fsck's replay");
}

#[test]
fn linux_journal_replays_read_only_in_memory() {
    let image = linux_journal();
    let volume = Volume::mount(&image, true).unwrap();
    same_trees(&radix_tree(&volume.root()), &linux_journal_files(), "Radix, read-only");
    let mut label = String::new();
    volume.fs().label(&mut label).unwrap();
    assert_eq!(label, "LINUXJNL");
    // Nothing reached the disk.
    assert!(volume.raw() == image, "a read-only mount wrote to the disk");
    assert_eq!(volume.root().create("new", Kind::File).err(), Some(Error::ReadOnly));
}

#[test]
fn linux_journal_replays_writable() {
    let image = linux_journal();
    let volume = Volume::new(&image);
    let root = volume.root();
    same_trees(&radix_tree(&root), &linux_journal_files(), "Radix, writable");
    make_file(&root, "radix.txt", b"written after the replay");
    drop(root);
    let image = volume.unmount();
    let incompat = le32(&image, sb::OFFSET as usize + at::INCOMPAT);
    assert_eq!(incompat & incompat::RECOVER, 0, "the unmount left the journal empty");
    fsck(&image);
    let mut want = linux_journal_files();
    want.insert(String::from("radix.txt"), Thing::File(b"written after the replay".to_vec()));
    same_trees(&debugfs_tree(&image), &want, "debugfs, after Radix's replay");
}

/// Changes a volume in steps, syncing after some, from what `before` it
/// held: a power cut may come anywhere in it. What each sync left goes
/// into `synced`, in order.
fn workload(
    root: &NodeRef,
    fs: &Ext4Fs,
    before: &Tree,
    synced: &mut Vec<Tree>,
) -> Result<(), Error> {
    let mut tree = before.clone();
    let dir = root.create("work", Kind::Directory)?;
    tree.insert(String::from("work"), Thing::Dir);
    for i in 0..12u32 {
        let data = pattern(i, 1000 + 3000 * i as usize);
        let node = dir.create(&format!("file {i}"), Kind::File)?;
        write_all(&node, 0, &data)?;
        tree.insert(format!("work/file {i}"), Thing::File(data));
        if i % 4 == 3 {
            fs.sync()?;
            synced.push(tree.clone());
        }
    }
    for i in (0..12u32).step_by(3) {
        dir.remove(&format!("file {i}"))?;
        tree.remove(&format!("work/file {i}"));
    }
    dir.rename("file 1", &**root, "moved")?;
    let moved = tree.remove("work/file 1").unwrap();
    tree.insert(String::from("moved"), moved);
    dir.lookup("file 2")?.set_size(10)?;
    if let Some(Thing::File(data)) = tree.get_mut("work/file 2") {
        data.truncate(10);
    }
    fs.sync()?;
    synced.push(tree.clone());
    let sub = dir.create("sub", Kind::Directory)?;
    tree.insert(String::from("work/sub"), Thing::Dir);
    sub.make_link("to moved", "../../moved")?;
    tree.insert(String::from("work/sub/to moved"), Thing::Link(String::from("../../moved")));
    let data = pattern(77, 70_000);
    let node = sub.create("last", Kind::File)?;
    write_all(&node, 0, &data)?;
    tree.insert(String::from("work/sub/last"), Thing::File(data));
    // Overwritten in place after a sync: the data goes straight to its
    // blocks, as on Linux (`data=ordered`), so either is seen.
    fs.sync()?;
    synced.push(tree.clone());
    let node = dir.lookup("file 5")?;
    node.set_size(0)?;
    write_all(&node, 0, b"rewritten")?;
    tree.insert(String::from("work/file 5"), Thing::File(b"rewritten".to_vec()));
    fs.sync()?;
    synced.push(tree);
    Ok(())
}

/// A power cut at points of [`workload`] on `image`, every `step` requests
/// to the disk. What the disk holds then mounts, its journal replayed,
/// with what the last sync before the cut left, or the next one; `e2fsck`
/// finds nothing wrong, after Radix's replay and after its own, which
/// gives the same files.
fn power_cuts(image: &[u8], what: &str, cuts: u64) {
    let before = radix_tree(&Volume::mount(image, true).unwrap().root());
    let (total, all) = {
        let volume = Volume::new(image);
        let start = volume.disk.requests();
        let mut synced = Vec::new();
        workload(&volume.root(), volume.fs(), &before, &mut synced).unwrap();
        (volume.disk.requests() - start, synced)
    };
    let step = (total / cuts).max(1);
    let mut cut = 0;
    while cut < total {
        let volume = Volume::new(image);
        volume.disk.fail_after(cut as u32);
        let mut synced = Vec::new();
        let done = workload(&volume.root(), volume.fs(), &before, &mut synced);
        assert!(
            done.is_err(),
            "{what}: the workload finished with the disk failing after {cut} requests"
        );
        let left = volume.raw();
        // Once the disk is back, a volume that gave up writes nothing more,
        // even as it unmounts; one whose failure changed nothing (a read)
        // unmounts as it would have.
        volume.disk.fail_after(u32::MAX);
        let broken = volume.fs().broken();
        let disk = volume.disk.clone();
        drop(volume);
        let mut after = vec![0u8; left.len()];
        disk.dump(0, &mut after);
        if broken {
            assert!(after == left, "{what}, cut after {cut} requests: the broken volume wrote on");
        } else {
            let (clean, text) = fsck_report(&after);
            assert!(clean, "{what}, failing after {cut} requests, then unmounted:\n{text}");
        }
        let replayed = e2fsck_replay(&left);
        let (clean, text) = fsck_report(&replayed);
        assert!(clean, "{what}, cut after {cut} requests, replayed by e2fsck:\n{text}");
        let volume = Volume::new(&left);
        let seen = radix_tree(&volume.root());
        let image = volume.unmount();
        let (clean, text) = fsck_report(&image);
        assert!(clean, "{what}, cut after {cut} requests, replayed by Radix:\n{text}");
        same_trees(
            &debugfs_tree(&replayed),
            &seen,
            &format!("debugfs after e2fsck's replay, {what}, cut {cut}"),
        );
        let k = synced.len();
        let last = if k == 0 { &before } else { &all[k - 1] };
        let ok = seen == *last || all.get(k).is_some_and(|next| seen == *next);
        assert!(
            ok,
            "{what}, cut after {cut} requests, {k} syncs done: Radix sees {:?}",
            seen.keys()
        );
        cut += step;
    }
}

#[test]
fn power_cuts_leave_a_whole_volume() {
    for (what, _, args) in [VOLUMES[0], VOLUMES[1], VOLUMES[3], VOLUMES[10]] {
        let image = Volume::new(&mkfs(12, args)).unmount();
        power_cuts(&image, what, 25);
    }
}

/// Without a journal nothing keeps the volume whole through a power cut,
/// as on Linux: it is marked as not cleanly unmounted while it is
/// mounted, so that `e2fsck` checks it before it is used again.
#[test]
fn without_a_journal_a_power_cut_leaves_it_marked() {
    let image = Volume::new(&mkfs(12, &["-O", "^has_journal"])).unmount();
    let state = |image: &[u8]| le16(image, sb::OFFSET as usize + at::STATE) & sb::VALID_FS;
    assert_eq!(state(&image), sb::VALID_FS);
    let volume = Volume::new(&image);
    let before = radix_tree(&volume.root());
    volume.disk.fail_after(30);
    assert!(workload(&volume.root(), volume.fs(), &before, &mut Vec::new()).is_err());
    assert_eq!(state(&volume.raw()), 0);
}

/// A directory of `n` files under `big`, and a few files elsewhere.
fn big_dir_tree(n: u32) -> Tree {
    let mut tree = Tree::new();
    tree.insert(String::from("big"), Thing::Dir);
    for i in 0..n {
        tree.insert(
            format!("big/a file with a longer name {i}"),
            Thing::File(pattern(i, (i % 7) as usize * 100)),
        );
    }
    tree.insert(String::from("small"), Thing::Dir);
    tree.insert(String::from("small/one"), Thing::File(b"one".to_vec()));
    tree
}

/// Runs `e2fsck -fyD` on `image`, which indexes its directories (htree).
fn index_dirs(image: &mut Vec<u8>) {
    let path = scratch("index");
    std::fs::write(&path, &*image).unwrap();
    let (_, text) = run(Command::new(tool("e2fsck")).arg("-fyD").arg(&path));
    *image = std::fs::read(&path).unwrap();
    let _ = std::fs::remove_file(&path);
    assert!(!text.contains("rror"), "e2fsck -fyD: {text}");
}

/// The inode flags of `path`, as `debugfs` says.
fn debugfs_flags(image: &[u8], path: &str) -> u32 {
    let mut image = image.to_vec();
    let text = debugfs(&mut image, false, &format!("stat \"/{path}\""));
    let at = text.find("Flags: 0x").unwrap() + 9;
    let hex: String = text[at..].chars().take_while(char::is_ascii_hexdigit).collect();
    u32::from_str_radix(&hex, 16).unwrap()
}

#[test]
fn indexed_directories_are_read_and_changed() {
    for (what, mib, args) in [VOLUMES[0], VOLUMES[1], VOLUMES[4]] {
        let mut tree = big_dir_tree(700);
        let dir = host_dir(&tree);
        let mut image = mkfs_from(mib, args, Some(&dir));
        let _ = std::fs::remove_dir_all(&dir);
        index_dirs(&mut image);
        assert_ne!(debugfs_flags(&image, "big") & flags::INDEX, 0, "{what}: e2fsck indexed it");
        let volume = Volume::new(&image);
        let root = volume.root();
        same_trees(&radix_tree(&root), &tree, &format!("Radix, {what}"));
        // Names leave it with its index kept.
        let big = walk(&root, "big").unwrap();
        for i in (0..700).step_by(5) {
            big.remove(&format!("a file with a longer name {i}")).unwrap();
            tree.remove(&format!("big/a file with a longer name {i}"));
        }
        let image = volume.synced();
        assert_ne!(
            debugfs_flags(&image, "big") & flags::INDEX,
            0,
            "{what}: removing kept the index"
        );
        // A name added drops the index, as for an old kernel's change.
        make_file(&big, "new", b"new");
        tree.insert(String::from("big/new"), Thing::File(b"new".to_vec()));
        walk(&root, "small").unwrap().rename("one", &*big, "a file with a longer name 1").unwrap();
        tree.remove("small/one");
        tree.insert(String::from("big/a file with a longer name 1"), Thing::File(b"one".to_vec()));
        drop((root, big));
        let image = volume.unmount();
        fsck(&image);
        assert_eq!(debugfs_flags(&image, "big") & flags::INDEX, 0, "{what}");
        same_trees(&debugfs_tree(&image), &tree, &format!("debugfs, {what}"));
        // Linux indexes it again, and Radix reads that too.
        let mut image = image;
        index_dirs(&mut image);
        fsck(&image);
        same_trees(
            &radix_tree(&Volume::new(&image).root()),
            &tree,
            &format!("Radix again, {what}"),
        );
    }
}

#[test]
fn ext2_and_ext3_volumes_read_and_ext3_with_extents_writes() {
    let tree = sample_tree(11);
    let dir = host_dir(&tree);
    for kind in ["ext2", "ext3"] {
        let image = mkfs_from(32, &["-t", kind], Some(&dir));
        // Without extents Radix only reads it.
        assert_eq!(Volume::mount(&image, false).err(), Some(Error::NotSupported), "{kind}");
        let volume = Volume::mount(&image, true).unwrap();
        same_trees(&radix_tree(&volume.root()), &tree, &format!("Radix, {kind}"));
    }
    // `tune2fs -O extents`, as for an ext3 volume turned ext4: its old files
    // keep their block maps, which Radix turns into extents as it changes
    // them.
    let mut image = mkfs_from(32, &["-t", "ext3"], Some(&dir));
    let path = scratch("tune");
    std::fs::write(&path, &image).unwrap();
    let (ok, text) = run(Command::new(tool("tune2fs")).args(["-O", "extents"]).arg(&path));
    assert!(ok, "tune2fs: {text}");
    image = std::fs::read(&path).unwrap();
    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_dir_all(&dir);
    let volume = Volume::new(&image);
    let root = volume.root();
    let mut want = tree.clone();
    for (i, (path, thing)) in tree.iter().enumerate() {
        let Thing::File(data) = thing else { continue };
        let node = walk(&root, path).unwrap();
        let mut data = data.clone();
        match i % 3 {
            0 => {
                let more = pattern(i as u32 + 50, 70_000);
                write_all(&node, data.len() as u64 + 5000, &more).unwrap();
                data.resize(data.len() + 5000, 0);
                data.extend_from_slice(&more);
            }
            1 => {
                node.set_size(data.len() as u64 / 2).unwrap();
                data.truncate(data.len() / 2);
            }
            _ => {}
        }
        want.insert(path.clone(), Thing::File(data));
    }
    let docs = walk(&root, "Documents").unwrap();
    make_file(&docs, "added", &pattern(5, 9000));
    want.insert(String::from("Documents/added"), Thing::File(pattern(5, 9000)));
    drop((root, docs));
    let image = volume.unmount();
    fsck(&image);
    same_trees(&debugfs_tree(&image), &want, "debugfs, ext3 with extents");
    let big =
        want.iter().find(|(_, t)| matches!(t, Thing::File(d) if d.len() > 100_000)).unwrap().0;
    assert_ne!(debugfs_flags(&image, big) & flags::EXTENTS, 0, "{big} has extents now");
}

#[test]
fn read_only_features_mount_read_only() {
    let tree = sample_tree(13);
    let dir = host_dir(&tree);
    // Clusters of several blocks (`bigalloc`) Radix does not read at all.
    let image = mkfs_from(64, &["-O", "bigalloc", "-C", "16384"], Some(&dir));
    assert_eq!(Volume::mount(&image, true).err(), Some(Error::NotSupported), "bigalloc");
    for (feature, readable) in [("quota", true), ("inline_data", false), ("^extent,^64bit", true)] {
        let image = mkfs_from(64, &["-O", feature], Some(&dir));
        assert_eq!(
            Volume::mount(&image, false).err(),
            Some(Error::NotSupported),
            "a writable mount of {feature}"
        );
        let volume = Volume::mount(&image, true).unwrap();
        if readable {
            same_trees(&radix_tree(&volume.root()), &tree, &format!("Radix, {feature}"));
        } else {
            // Data kept in the inode Radix does not read, and says so.
            let small =
                tree.iter().find(|(_, t)| matches!(t, Thing::File(d) if d.len() == 1)).unwrap().0;
            let node = walk(&volume.root(), small).unwrap();
            assert_eq!(node.read(0, &mut SliceMut(&mut [0; 4])).err(), Some(Error::NotSupported));
        }
        assert!(volume.raw() == image, "{feature}: a read-only mount wrote");
    }
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn disks_of_larger_sectors() {
    // 4 KiB blocks on a disk of 4 KiB sectors work.
    let image = mkfs(32, &[]);
    let volume = Volume::mount_in(4096, &image, false).unwrap();
    put_tree(&volume.root(), &sample_tree(17));
    let image = volume.unmount();
    fsck(&image);
    same_trees(&debugfs_tree(&image), &sample_tree(17), "debugfs, 4 KiB sectors");
    // 1 KiB blocks cannot be written whole on them.
    let image = mkfs(16, &["-b", "1024"]);
    assert_eq!(Volume::mount_in(4096, &image, true).err(), Some(Error::NotSupported));
}

#[test]
fn a_full_volume_fails_writes_not_syncs() {
    for (what, mib, args) in [VOLUMES[0], VOLUMES[1]] {
        let volume = Volume::new(&mkfs(mib, args));
        let root = volume.root();
        let mut kept = Vec::new();
        let mut i = 0u32;
        let full = loop {
            let name = format!("f{i}");
            let node = match root.create(&name, Kind::File) {
                Ok(node) => node,
                Err(err) => break err,
            };
            let data = pattern(i, 900_000);
            match node.write(0, &Slice(&data)) {
                Ok(n) if n == data.len() => kept.push((name, data)),
                Ok(n) => {
                    kept.push((name, data[..n].to_vec()));
                    break Error::NoSpace;
                }
                Err(err) => {
                    kept.push((name, Vec::new()));
                    break err;
                }
            }
            i += 1;
        };
        assert_eq!(full, Error::NoSpace, "{what}");
        // What was written fits: the sync finds room for all of it.
        volume.fs().sync().unwrap();
        let free = volume.fs().info().free;
        assert!(free < volume.fs().info().blocks / 20, "{what}: {free} blocks left");
        // Room comes back as files go.
        for (name, _) in kept.drain(..kept.len() / 2) {
            root.remove(&name).unwrap();
        }
        volume.fs().sync().unwrap();
        assert!(volume.fs().info().free > free * 2, "{what}");
        make_file(&root, "after", &pattern(1, 500_000));
        kept.push((String::from("after"), pattern(1, 500_000)));
        drop(root);
        let image = volume.unmount();
        fsck(&image);
        let volume = Volume::new(&image);
        for (name, data) in &kept {
            assert!(read_all(&walk(&volume.root(), name).unwrap()) == *data, "{what}: {name}");
        }
    }
}

#[test]
fn removed_open_files_live_until_closed() {
    for (what, mib, args) in [VOLUMES[0], VOLUMES[10]] {
        let volume = Volume::new(&mkfs(mib, args));
        let root = volume.root();
        let free = volume.fs().info().free;
        let node = make_file(&root, "open", &pattern(3, 2_000_000));
        let dir = root.create("dir", Kind::Directory).unwrap();
        volume.fs().sync().unwrap();
        root.remove("open").unwrap();
        root.remove("dir").unwrap();
        assert_eq!(root.lookup("open").err(), Some(Error::NotFound));
        // Still there for those who have it.
        assert!(read_all(&node) == pattern(3, 2_000_000));
        write_all(&node, 100, b"still writable").unwrap();
        assert_eq!(dir.create("x", Kind::File).err(), Some(Error::NotFound));
        // A power cut now leaves it on the orphan list: the next mount
        // frees it.
        let cut = volume.synced();
        let image_free = {
            let volume = Volume::new(&cut);
            let free = volume.fs().info().free;
            let image = volume.unmount();
            fsck(&image);
            free
        };
        assert!(
            image_free >= free - 16,
            "{what}: the next mount freed it ({image_free} of {free})"
        );
        // Closed, it goes at the next commit.
        drop((node, dir));
        volume.fs().sync().unwrap();
        assert!(volume.fs().info().free >= free - 16, "{what}");
        drop(root);
        fsck(&volume.unmount());
    }
}

#[test]
fn fragmented_files_grow_deep_trees() {
    for (what, mib, args) in [VOLUMES[0], VOLUMES[1]] {
        let volume = Volume::new(&mkfs(mib, args));
        let root = volume.root();
        let bs = volume.fs().info().block_size as usize;
        // A block written every other block: an extent each, more than a
        // tree of one level holds.
        let node = root.create("sparse", Kind::File).unwrap();
        let mut want = Vec::new();
        for k in 0..1500usize {
            let data = pattern(k as u32, bs);
            write_all(&node, (2 * k * bs) as u64, &data).unwrap();
            want.resize(2 * k * bs, 0);
            want.extend_from_slice(&data);
        }
        drop(node);
        let image = volume.synced();
        fsck(&image);
        let tree = debugfs_tree(&image);
        assert!(tree.get("sparse") == Some(&Thing::File(want.clone())), "{what}: debugfs reads it");
        let mut image_copy = image.clone();
        let extents = debugfs(&mut image_copy, false, "ex /sparse");
        let depth = extents
            .lines()
            .nth(1)
            .and_then(|l| l.trim().split('/').nth(1))
            .unwrap()
            .trim()
            .to_string();
        assert!(
            depth != "0",
            "{what}: a tree of more than one level:\n{}",
            &extents[..extents.len().min(400)]
        );
        // The holes are filled, and the extents merge; the tree shrinks.
        let node = walk(&root, "sparse").unwrap();
        for k in 0..1500usize {
            let data = pattern(9000 + k as u32, bs);
            let at = (2 * k + 1) * bs;
            write_all(&node, at as u64, &data).unwrap();
            if want.len() < at + bs {
                want.resize(at + bs, 0);
            }
            want[at..at + bs].copy_from_slice(&data);
        }
        // And it is cut, part way through a block.
        let keep = 1234 * bs + 77;
        node.set_size(keep as u64).unwrap();
        want.truncate(keep);
        drop((node, root));
        let image = volume.unmount();
        fsck(&image);
        assert!(debugfs_tree(&image).get("sparse") == Some(&Thing::File(want.clone())), "{what}");
        let volume = Volume::new(&image);
        assert!(read_all(&walk(&volume.root(), "sparse").unwrap()) == want, "{what}");
    }
}

#[test]
fn unwritten_extents_read_as_zeros_and_are_written() {
    let mut image = mkfs(32, &[]);
    let mut put = image.clone();
    let host = scratch("data");
    std::fs::write(&host, pattern(1, 10_000)).unwrap();
    debugfs(&mut put, true, &format!("write {} f", host.display()));
    let _ = std::fs::remove_file(&host);
    // Blocks 3 to 40 taken but not written, past the end too.
    debugfs(&mut put, true, "fallocate /f 3 40");
    image = put;
    fsck(&image);
    let volume = Volume::new(&image);
    let node = walk(&volume.root(), "f").unwrap();
    assert!(read_all(&node) == pattern(1, 10_000));
    node.set_size(100_000).unwrap();
    let mut want = pattern(1, 10_000);
    want.resize(100_000, 0);
    assert!(read_all(&node) == want, "unwritten blocks read as zeros");
    // Written in the middle of the unwritten run, which splits.
    write_all(&node, 20_000, &pattern(2, 9000)).unwrap();
    want[20_000..29_000].copy_from_slice(&pattern(2, 9000));
    drop(node);
    let image = volume.unmount();
    fsck(&image);
    assert_eq!(debugfs_tree(&image).get("f"), Some(&Thing::File(want.clone())));
    let volume = Volume::new(&image);
    assert!(read_all(&walk(&volume.root(), "f").unwrap()) == want);
}

#[test]
fn renames_follow_posix() {
    let volume = Volume::new(&mkfs(16, &["-b", "1024"]));
    let root = volume.root();
    let a = root.create("a", Kind::Directory).unwrap();
    let b = a.create("b", Kind::Directory).unwrap();
    let full = root.create("full", Kind::Directory).unwrap();
    make_file(&full, "x", b"x");
    root.create("empty", Kind::Directory).unwrap();
    make_file(&root, "file", b"file");
    make_file(&root, "other", b"other");
    assert_eq!(root.rename("a", &*b, "inside").err(), Some(Error::BadName));
    assert_eq!(root.rename("a", &*a, "self").err(), Some(Error::BadName));
    assert_eq!(root.rename("a", &*root, "full").err(), Some(Error::NotEmpty));
    assert_eq!(root.rename("file", &*root, "empty").err(), Some(Error::IsDirectory));
    assert_eq!(root.rename("a", &*root, "file").err(), Some(Error::NotDirectory));
    assert_eq!(root.rename("missing", &*root, "x").err(), Some(Error::NotFound));
    root.rename("file", &*root, "file").unwrap();
    // A directory over an empty one, and a file over a file.
    root.rename("a", &*root, "empty").unwrap();
    root.rename("other", &*b, "moved").unwrap();
    make_file(&root, "new", b"new");
    root.rename("new", &*root, "file").unwrap();
    // Up and down the tree.
    b.rename("moved", &*root, "back").unwrap();
    full.rename("x", &*b, "x").unwrap();
    root.rename("full", &*b, "full").unwrap();
    drop((a, b, full, root));
    let image = volume.unmount();
    fsck(&image);
    let mut want = Tree::new();
    want.insert(String::from("empty"), Thing::Dir);
    want.insert(String::from("empty/b"), Thing::Dir);
    want.insert(String::from("empty/b/x"), Thing::File(b"x".to_vec()));
    want.insert(String::from("empty/b/full"), Thing::Dir);
    want.insert(String::from("back"), Thing::File(b"other".to_vec()));
    want.insert(String::from("file"), Thing::File(b"new".to_vec()));
    same_trees(&debugfs_tree(&image), &want, "debugfs");
}

#[test]
fn many_names_in_one_directory_list_from_any_cursor() {
    let volume = Volume::new(&mkfs(32, &["-b", "1024", "-N", "8192"]));
    let root = volume.root();
    let dir = root.create("many", Kind::Directory).unwrap();
    for i in 0..5000 {
        drop(dir.create(&format!("name number {i}"), Kind::File).unwrap());
    }
    // Listing resumes where it stopped.
    let mut seen = Vec::new();
    let mut cursor = 0;
    loop {
        let mut got = 0;
        dir.list(cursor, &mut |next, name, _| {
            seen.push(String::from(name));
            cursor = next;
            got += 1;
            got < 37
        })
        .unwrap();
        if got < 37 {
            break;
        }
    }
    seen.sort();
    let mut want: Vec<String> = (0..5000).map(|i| format!("name number {i}")).collect();
    want.sort();
    assert_eq!(seen, want);
    for i in (0..5000).step_by(2) {
        dir.remove(&format!("name number {i}")).unwrap();
    }
    drop((dir, root));
    let image = volume.unmount();
    fsck(&image);
    let tree = debugfs_tree(&image);
    assert_eq!(tree.len(), 1 + 2500);
}

#[test]
fn hard_links_linux_made_keep_their_count() {
    let mut image = mkfs(16, &[]);
    let host = scratch("data");
    std::fs::write(&host, b"shared").unwrap();
    debugfs(&mut image, true, &format!("write {} one", host.display()));
    let _ = std::fs::remove_file(&host);
    debugfs(&mut image, true, "ln one two");
    debugfs(&mut image, true, "sif one links_count 2");
    fsck(&image);
    let volume = Volume::new(&image);
    let root = volume.root();
    assert_eq!(walk(&root, "one").unwrap().stat().id, walk(&root, "two").unwrap().stat().id);
    root.remove("one").unwrap();
    drop(root);
    let image = volume.unmount();
    fsck(&image);
    let tree = debugfs_tree(&image);
    assert_eq!(tree.get("two"), Some(&Thing::File(b"shared".to_vec())));
    assert_eq!(tree.len(), 1);
}

#[test]
fn times_are_kept() {
    let volume = Volume::new(&mkfs(16, &[]));
    let root = volume.root();
    let node = make_file(&root, "f", b"data");
    let stat = node.stat();
    assert_eq!(stat.created, clock());
    assert_eq!(stat.modified, clock());
    let (accessed, modified) = (1_000_000_000_123_456_789, 1_500_000_000_987_654_321);
    node.set_times(Some(accessed), Some(modified)).unwrap();
    drop((node, root));
    let volume = volume.remount();
    let stat = walk(&volume.root(), "f").unwrap().stat();
    assert_eq!((stat.accessed, stat.modified), (accessed, modified));
    // 128-byte inodes keep seconds.
    let volume = Volume::new(&mkfs(16, &["-I", "128"]));
    let node = make_file(&volume.root(), "f", b"data");
    node.set_times(Some(accessed), Some(modified)).unwrap();
    drop(node);
    let volume = volume.remount();
    let stat = walk(&volume.root(), "f").unwrap().stat();
    assert_eq!(
        (stat.accessed, stat.modified),
        (1_000_000_000_000_000_000, 1_500_000_000_000_000_000)
    );
    fsck(&volume.unmount());
}

#[test]
fn large_changes_commit_in_parts() {
    // More changed blocks than a quarter of a small journal: the
    // transaction is committed as it grows.
    let volume = Volume::new(&mkfs(64, &["-b", "1024", "-J", "size=1", "-N", "16384"]));
    let root = volume.root();
    let mut want = Tree::new();
    for d in 0..40 {
        let dir = root.create(&format!("d{d}"), Kind::Directory).unwrap();
        want.insert(format!("d{d}"), Thing::Dir);
        for f in 0..100 {
            drop(make_file(&dir, &format!("f{f}"), &pattern(d * 100 + f, 50)));
            want.insert(format!("d{d}/f{f}"), Thing::File(pattern(d * 100 + f, 50)));
        }
    }
    drop(root);
    let image = volume.unmount();
    fsck(&image);
    same_trees(&debugfs_tree(&image), &want, "debugfs");
}

/// Where `debugfs` says inode `ino` is: its byte offset.
fn inode_at(image: &[u8], ino: u32) -> usize {
    let mut image = image.to_vec();
    let text = debugfs(&mut image, false, &format!("imap <{ino}>"));
    // "Inode 2 is part of block group 0\n\tlocated at block 34, offset 0x0100"
    let block: usize =
        text.split("located at block ").nth(1).unwrap().split(',').next().unwrap().parse().unwrap();
    let offset = text.split("offset 0x").nth(1).unwrap().split_whitespace().next().unwrap();
    let bs = le32(&image, sb::OFFSET as usize + at::LOG_BLOCK_SIZE);
    block * (1024 << bs) + usize::from_str_radix(offset, 16).unwrap()
}

/// The byte offset of block `l` of `path`, as `debugfs` maps it.
fn block_at(image: &[u8], path: &str, l: u32) -> usize {
    let mut image = image.to_vec();
    let text = debugfs(&mut image, false, &format!("bmap \"/{path}\" {l}"));
    let block: usize = text.lines().find_map(|l| l.trim().parse().ok()).unwrap();
    let bs = le32(&image, sb::OFFSET as usize + at::LOG_BLOCK_SIZE);
    block * (1024 << bs)
}

#[test]
fn damage_is_found() {
    let mut tree = sample_tree(19);
    tree.insert(String::from("sparse"), Thing::File(Vec::new()));
    let dir = host_dir(&tree);
    let image = mkfs_from(32, &[], Some(&dir));
    let _ = std::fs::remove_dir_all(&dir);
    // A file with a tree of extents.
    let volume = Volume::new(&image);
    let node = walk(&volume.root(), "sparse").unwrap();
    for k in 0..800u64 {
        write_all(&node, k * 8192, &pattern(k as u32, 4096)).unwrap();
    }
    drop(node);
    let image = volume.unmount();
    fsck(&image);
    let flip = |at: usize| {
        let mut bad = image.clone();
        bad[at] ^= 0x10;
        bad
    };
    // The superblock's checksum.
    let bad = flip(sb::OFFSET as usize + at::VOLUME_NAME);
    assert_eq!(Volume::mount(&bad, true).err(), Some(Error::Corrupt), "superblock");
    // A group descriptor's.
    let bs = 4096;
    let bad = flip(bs + 0x10);
    assert_eq!(Volume::mount(&bad, true).err(), Some(Error::Corrupt), "group descriptor");
    // The root's inode.
    let bad = flip(inode_at(&image, 2) + 0x10);
    assert_eq!(Volume::mount(&bad, true).err(), Some(Error::Corrupt), "root inode");
    // A file's inode: found when it is looked up.
    let file = tree
        .keys()
        .find(|p| p.starts_with("Documents/") && matches!(tree[*p], Thing::File(_)))
        .unwrap();
    let mut image_copy = image.clone();
    let ino: u32 = debugfs(&mut image_copy, false, &format!("stat \"/{file}\""))
        .split("Inode: ")
        .nth(1)
        .unwrap()
        .split_whitespace()
        .next()
        .unwrap()
        .parse()
        .unwrap();
    let bad = flip(inode_at(&image, ino) + 0x10);
    let volume = Volume::mount(&bad, true).unwrap();
    assert_eq!(walk(&volume.root(), file).err(), Some(Error::Corrupt), "file inode");
    // A directory block.
    let bad = flip(block_at(&image, "Documents", 0) + 40);
    let volume = Volume::mount(&bad, true).unwrap();
    assert_eq!(
        volume.root().lookup("Documents").unwrap().list(0, &mut |_, _, _| true).err(),
        Some(Error::Corrupt)
    );
    // A leaf of the extent tree.
    let mut image_copy = image.clone();
    let text = debugfs(&mut image_copy, false, "ex /sparse");
    // " 0/ 1   1/  3     0 - 1023  1695           1024": the root's first
    // entry names its leaf, after the logical range.
    let tokens: Vec<&str> = text
        .lines()
        .find(|l| l.trim_start().starts_with("0/"))
        .unwrap()
        .split_whitespace()
        .collect();
    let dash = tokens.iter().position(|t| *t == "-").unwrap();
    let leaf: usize = tokens[dash + 2].parse().unwrap_or_else(|_| panic!("{text}"));
    let bad = flip(leaf * bs + 20);
    let volume = Volume::mount(&bad, true).unwrap();
    assert_eq!(walk(&volume.root(), "sparse").err(), Some(Error::Corrupt), "extent leaf");
    // Writes to a damaged volume fail, and leave it as it was.
    let volume = Volume::mount(&bad, false).unwrap();
    let node = walk(&volume.root(), "Documents").unwrap();
    make_file(&node, "fine", b"fine");
    drop(node);
    assert_eq!(walk(&volume.root(), "sparse").err(), Some(Error::Corrupt));
    let left = volume.unmount();
    let (_, text) = fsck_report(&left);
    assert!(
        text.contains("sparse") || text.contains("xtent") || text.contains("checksum"),
        "{text}"
    );
}

/// Metadata of a small volume: its first blocks, where `mkfs.ext4` puts
/// the group's bitmaps, inode table and directories with `flex_bg`.
fn fuzz_volume() -> Vec<u8> {
    static IMAGE: std::sync::OnceLock<Vec<u8>> = std::sync::OnceLock::new();
    IMAGE
        .get_or_init(|| {
            let mut tree = sample_tree(23);
            tree.retain(|_, t| !matches!(t, Thing::File(d) if d.len() > 40_000));
            tree.extend(big_dir_tree(150));
            let dir = host_dir(&tree);
            let mut image = mkfs_from(8, &["-b", "1024", "-N", "512"], Some(&dir));
            let _ = std::fs::remove_dir_all(&dir);
            index_dirs(&mut image);
            image
        })
        .clone()
}

/// Reads everything under `dir`, ignoring what fails.
fn read_everything(dir: &NodeRef, depth: u32) {
    let mut names = Vec::new();
    let _ = dir.list(0, &mut |_, name, _| {
        names.push(String::from(name));
        names.len() < 1000
    });
    for name in names {
        let Ok(node) = dir.lookup(&name) else { continue };
        let _ = node.stat();
        match node.kind() {
            Kind::Directory if depth < 8 => read_everything(&node, depth + 1),
            Kind::Symlink => {
                let _ = node.read_link(&mut SliceMut(&mut [0; 300]));
            }
            _ => {
                let _ = node.read(0, &mut SliceMut(&mut vec![0; 70_000]));
            }
        }
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(96))]

    /// Damaged metadata never panics the driver: reads and writes fail,
    /// or succeed, but it stays up.
    #[test]
    fn damaged_metadata_never_panics(flips in prop::collection::vec((0usize..300 * 1024, any::<u8>()), 1..24)) {
        let mut image = fuzz_volume();
        for (at, bits) in flips {
            image[at] ^= bits | 1;
        }
        if let Ok(volume) = Volume::mount(&image, true) {
            read_everything(&volume.root(), 0);
        }
        if let Ok(volume) = Volume::mount(&image, false) {
            let root = volume.root();
            read_everything(&root, 0);
            if let Ok(node) = root.create("new", Kind::File) {
                let _ = node.write(0, &Slice(&pattern(1, 20_000)));
            }
            let _ = root.remove("small");
            let _ = root.create("d", Kind::Directory);
            let _ = volume.fs().sync();
        }
    }
}

/// A step of [`changes_match_a_model`].
#[derive(Clone, Debug)]
enum Op {
    /// Writes `len` bytes of `seed` at `at` of file `f` of directory `d`,
    /// making it if it is not there.
    Write {
        d: usize,
        f: usize,
        at: u32,
        len: u32,
        seed: u32,
    },
    SetSize {
        d: usize,
        f: usize,
        size: u32,
    },
    Remove {
        d: usize,
        f: usize,
    },
    Rename {
        d: usize,
        f: usize,
        to_d: usize,
        to_f: usize,
    },
    MakeDir {
        d: usize,
    },
    RemoveDir {
        d: usize,
    },
    Link {
        d: usize,
        f: usize,
        to: usize,
    },
    Sync,
    Remount,
}

/// The directories of [`changes_match_a_model`]: the root, two under it,
/// and one under one of those.
const MODEL_DIRS: [&str; 4] = ["", "one", "two", "one/three"];

fn op() -> impl Strategy<Value = Op> {
    let d = 0usize..4;
    let f = 0usize..5;
    prop_oneof![
        6 => (d.clone(), f.clone(), 0u32..300_000, 1u32..70_000, any::<u32>())
            .prop_map(|(d, f, at, len, seed)| Op::Write { d, f, at, len, seed }),
        2 => (d.clone(), f.clone(), 0u32..400_000).prop_map(|(d, f, size)| Op::SetSize { d, f, size }),
        2 => (d.clone(), f.clone()).prop_map(|(d, f)| Op::Remove { d, f }),
        2 => (d.clone(), f.clone(), d.clone(), f.clone())
            .prop_map(|(d, f, to_d, to_f)| Op::Rename { d, f, to_d, to_f }),
        1 => (1usize..4).prop_map(|d| Op::MakeDir { d }),
        1 => (1usize..4).prop_map(|d| Op::RemoveDir { d }),
        1 => (d.clone(), f.clone(), 0usize..5).prop_map(|(d, f, to)| Op::Link { d, f, to }),
        1 => Just(Op::Sync),
        1 => Just(Op::Remount),
    ]
}

fn model_path(d: usize, name: &str) -> String {
    if MODEL_DIRS[d].is_empty() { String::from(name) } else { format!("{}/{name}", MODEL_DIRS[d]) }
}

/// Runs `ops` on a volume and on a model of it: Radix, `debugfs` and the
/// model agree at the end, and `e2fsck` finds nothing wrong.
fn run_model(image: &[u8], ops: &[Op]) {
    let mut volume = Volume::new(image);
    let mut model = Tree::new();
    for dir in &MODEL_DIRS[1..] {
        model.insert(String::from(*dir), Thing::Dir);
    }
    put_tree(&volume.root(), &model);
    let dir_of = |root: &NodeRef, model: &Tree, d: usize| -> Option<NodeRef> {
        (d == 0 || model.get(MODEL_DIRS[d]) == Some(&Thing::Dir))
            .then(|| walk(root, MODEL_DIRS[d]).unwrap())
    };
    for op in ops {
        let root = volume.root();
        match *op {
            Op::Write { d, f, at, len, seed } => {
                let Some(dir) = dir_of(&root, &model, d) else { continue };
                let path = model_path(d, &format!("f{f}"));
                let node = match dir.lookup(&format!("f{f}")) {
                    Ok(node) => node,
                    Err(_) => {
                        model.insert(path.clone(), Thing::File(Vec::new()));
                        dir.create(&format!("f{f}"), Kind::File).unwrap()
                    }
                };
                if node.kind() != Kind::File {
                    continue;
                }
                let data = pattern(seed, len as usize);
                write_all(&node, u64::from(at), &data).unwrap();
                let Some(Thing::File(bytes)) = model.get_mut(&path) else { panic!("{path}") };
                let end = at as usize + data.len();
                if bytes.len() < end {
                    bytes.resize(end, 0);
                }
                bytes[at as usize..end].copy_from_slice(&data);
            }
            Op::SetSize { d, f, size } => {
                let path = model_path(d, &format!("f{f}"));
                if let Some(Thing::File(bytes)) = model.get_mut(&path) {
                    walk(&root, &path).unwrap().set_size(u64::from(size)).unwrap();
                    bytes.resize(size as usize, 0);
                }
            }
            Op::Remove { d, f } => {
                let path = model_path(d, &format!("f{f}"));
                if model.contains_key(&path) {
                    dir_of(&root, &model, d).unwrap().remove(&format!("f{f}")).unwrap();
                    model.remove(&path);
                }
            }
            Op::Rename { d, f, to_d, to_f } => {
                let (from, to) =
                    (model_path(d, &format!("f{f}")), model_path(to_d, &format!("f{to_f}")));
                let (Some(src), Some(dst)) =
                    (dir_of(&root, &model, d), dir_of(&root, &model, to_d))
                else {
                    continue;
                };
                if !model.contains_key(&from) {
                    continue;
                }
                src.rename(&format!("f{f}"), &*dst, &format!("f{to_f}")).unwrap();
                let thing = model.remove(&from).unwrap();
                model.insert(to, thing);
            }
            Op::MakeDir { d } => {
                let parent = if d == 3 { 1 } else { 0 };
                if !model.contains_key(MODEL_DIRS[d]) && dir_of(&root, &model, parent).is_some() {
                    let (dir, name) = super::tests::parent(&root, MODEL_DIRS[d]);
                    dir.create(name, Kind::Directory).unwrap();
                    model.insert(String::from(MODEL_DIRS[d]), Thing::Dir);
                }
            }
            Op::RemoveDir { d } => {
                if !model.contains_key(MODEL_DIRS[d]) {
                    continue;
                }
                let empty = !model.keys().any(|p| p.starts_with(&format!("{}/", MODEL_DIRS[d])));
                let (dir, name) = super::tests::parent(&root, MODEL_DIRS[d]);
                match dir.remove(name) {
                    Ok(()) => {
                        assert!(empty, "{} removed with names in it", MODEL_DIRS[d]);
                        model.remove(MODEL_DIRS[d]);
                    }
                    Err(err) => {
                        assert!(!empty && err == Error::NotEmpty, "{}: {err:?}", MODEL_DIRS[d]);
                    }
                }
            }
            Op::Link { d, f, to } => {
                let path = model_path(d, &format!("l{f}"));
                let Some(dir) = dir_of(&root, &model, d) else { continue };
                if model.contains_key(&path) {
                    continue;
                }
                let target = format!("f{to}{}", "/x".repeat(to * 20));
                dir.make_link(&format!("l{f}"), &target).unwrap();
                model.insert(path, Thing::Link(target));
            }
            Op::Sync => volume.fs().sync().unwrap(),
            Op::Remount => {
                drop(root);
                volume = volume.remount();
            }
        }
    }
    same_trees(&radix_tree(&volume.root()), &model, "Radix");
    let image = volume.unmount();
    let (clean, text) = fsck_report(&image);
    assert!(clean, "e2fsck found problems after {ops:?}:\n{text}");
    same_trees(&debugfs_tree(&image), &model, "debugfs");
}

fn model_volume(i: usize) -> Vec<u8> {
    static IMAGES: std::sync::OnceLock<Vec<Vec<u8>>> = std::sync::OnceLock::new();
    IMAGES.get_or_init(|| {
        vec![
            mkfs(16, &["-b", "1024"]),
            mkfs(24, &[]),
            mkfs(16, &["-O", "^metadata_csum,^64bit,uninit_bg", "-b", "2048"]),
        ]
    })[i]
        .clone()
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(48))]

    #[test]
    fn changes_match_a_model(volume in 0usize..3, ops in prop::collection::vec(op(), 1..40)) {
        run_model(&model_volume(volume), &ops);
    }
}
