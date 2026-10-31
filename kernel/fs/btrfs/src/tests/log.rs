// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Host tests of replaying the log of fsyncs Linux leaves (ADR 0151): a
//! disk Linux 6.8 powered off with changes in its log alone, made by
//! `ob fixture btrfs-log`, lists after Radix's mount as Linux lists it
//! after its own, and passes `btrfs check`.

use core::fmt::Write as _;

use radix_vfs::{Error, FileSystem, Kind, NodeRef, SliceMut};

use super::FILE;
use super::files::{contents, mount};
use super::rw::mount_rw;
use super::write::{assert_checks, image_of, writable};
use crate::format::{Key, item, tree, u16_at};
use crate::fs::BtrfsFs;
use crate::inode::Inode;
use crate::tree::{Cursor, Root};

/// The disk, as `ob fixture btrfs-log` keeps it.
const SPARSE: &[u8] = include_bytes!("fixtures/btrfs-log.sparse");

/// Linux's lists of the disk, before and after its replay.
const LISTS: &str = include_str!("fixtures/btrfs-log.txt");

/// The disk the fixture keeps.
fn disk_image() -> Vec<u8> {
    unsparse(SPARSE)
}

/// The disk a fixture keeps as `sparse`: `radix-sparse 1\n`, its size,
/// then runs of blocks, each where it starts, how long it is, and its
/// bytes.
pub(super) fn unsparse(sparse: &[u8]) -> Vec<u8> {
    let word = |bytes: &[u8], at: usize| {
        u64::from_le_bytes(bytes[at..at + 8].try_into().unwrap()) as usize
    };
    let rest = sparse.strip_prefix(b"radix-sparse 1\n".as_slice()).unwrap();
    let mut image = vec![0; word(rest, 0)];
    let mut at = 8;
    while at < rest.len() {
        let (start, len) = (word(rest, at), word(rest, at + 8));
        image[start..start + len].copy_from_slice(&rest[at + 16..at + 16 + len]);
        at += 16 + len;
    }
    image
}

/// Linux's list of the disk after it replays the log.
fn linux_list() -> Vec<String> {
    replayed_list(LISTS)
}

/// Linux's list of a disk after it replays its log, from a fixture's
/// `lists`.
pub(super) fn replayed_list(lists: &str) -> Vec<String> {
    lists
        .lines()
        .skip_while(|line| *line != "LIST replayed")
        .skip(1)
        .take_while(|line| *line != "LIST end")
        .map(|line| line.strip_prefix("LIST ").unwrap().to_string())
        .collect()
}

/// The log root the superblock of `image` names.
pub(super) fn log_root(image: &[u8]) -> u64 {
    u64::from_le_bytes(image[0x1_0060..0x1_0068].try_into().unwrap())
}

/// The files of `fs`, in the lines of Linux's list.
pub(super) fn list(fs: &BtrfsFs) -> Vec<String> {
    let subvol = fs.volume().tree(tree::FS).unwrap().unwrap();
    let mut out = Vec::new();
    walk(fs, &subvol, &fs.root(), "", &mut out);
    out
}

fn walk(fs: &BtrfsFs, subvol: &Root, dir: &NodeRef, rel: &str, out: &mut Vec<String>) {
    line(fs, subvol, dir, if rel.is_empty() { "/" } else { rel }, out);
    let mut names = Vec::new();
    dir.list(0, &mut |_, name, _| {
        names.push(name.to_string());
        true
    })
    .unwrap();
    names.sort();
    for name in names {
        let node = dir.lookup(&name).unwrap();
        let rel = format!("{rel}/{name}");
        if node.kind() == Kind::Directory {
            walk(fs, subvol, &node, &rel, out);
        } else {
            line(fs, subvol, &node, &rel, out);
        }
    }
}

/// FNV-1a, 64 bits, as `linux-bench --btrfs-log list` hashes a file.
fn fnv(bytes: &[u8]) -> u64 {
    bytes
        .iter()
        .fold(0xcbf2_9ce4_8422_2325, |h, &b| (h ^ u64::from(b)).wrapping_mul(0x100_0000_01b3))
}

/// The line of `node`, `rel` beneath the top: its inode item's mode,
/// owner, group, links and size, what it holds, and its extended
/// attributes.
fn line(fs: &BtrfsFs, subvol: &Root, node: &NodeRef, rel: &str, out: &mut Vec<String>) {
    let volume = fs.volume();
    // The default subvolume's nodes are numbered as its inodes.
    let ino = node.stat().id;
    let key = Key::new(ino, item::INODE_ITEM, 0);
    let inode = Inode::read(&crate::tree::find(volume, subvol, &key).unwrap().unwrap()).unwrap();
    let (kind, what) = match node.kind() {
        Kind::Directory => ("dir", "-".to_string()),
        Kind::Symlink => {
            let mut buf = vec![0; 4096];
            let n = node.read_link(&mut SliceMut(&mut buf)).unwrap();
            ("link", String::from_utf8(buf[..n].to_vec()).unwrap())
        }
        _ => ("file", format!("{:016x}", fnv(&contents(&**node, 1 << 16).unwrap()))),
    };
    out.push(format!(
        "{rel} {kind} {:o} {} {} {} {} {what} {}",
        inode.mode & 0o7777,
        inode.uid,
        inode.gid,
        inode.nlink,
        inode.size,
        xattrs(fs, subvol, ino)
    ));
}

/// The extended attributes of inode `ino`, `name=hex` in order, or `-`.
fn xattrs(fs: &BtrfsFs, subvol: &Root, ino: u64) -> String {
    let mut out = Vec::new();
    let mut cursor = Cursor::seek(fs.volume(), subvol, &Key::first(ino, item::XATTR_ITEM)).unwrap();
    while let Some((key, data)) = cursor.get() {
        if !key.is(ino, item::XATTR_ITEM) {
            break;
        }
        // Each one as a directory entry: a header, its name, its value.
        let mut at = 0;
        while at < data.len() {
            let value_len = usize::from(u16_at(data, at + 25));
            let name_len = usize::from(u16_at(data, at + 27));
            let name = &data[at + 30..at + 30 + name_len];
            let value = &data[at + 30 + name_len..at + 30 + name_len + value_len];
            let mut entry = format!("{}=", String::from_utf8_lossy(name));
            for byte in value {
                let _ = write!(entry, "{byte:02x}");
            }
            out.push(entry);
            at += 30 + name_len + value_len;
        }
        cursor.advance().unwrap();
    }
    out.sort();
    if out.is_empty() { "-".into() } else { out.join(",") }
}

#[test]
fn a_log_linux_left_replays_as_linux_replays_it() {
    let image = disk_image();
    assert_ne!(log_root(&image), 0, "the fixture has no log");
    let disk = writable(&image);
    let fs = mount_rw(&disk);
    assert_eq!(list(&fs), linux_list());
    // The mount committed the replay, with no log.
    let replayed = image_of(&disk, image.len());
    assert_eq!(log_root(&replayed), 0);
    assert_checks(&replayed);
    // What the replay made changes like anything else.
    fs.root().lookup("new").unwrap().set_size(0).unwrap();
    fs.root().remove("pointer").unwrap();
    fs.sync().unwrap();
    assert_checks(&image_of(&disk, image.len()));
    drop(fs);
    let again = mount_rw(&writable(&replayed));
    assert_eq!(list(&again), linux_list());
}

#[test]
fn a_read_only_mount_replays_the_log_and_a_read_only_disk_cannot() {
    let image = disk_image();
    let disk = writable(&image);
    let fs = BtrfsFs::mount(disk.clone()).unwrap();
    assert_eq!(list(&fs), linux_list());
    assert_eq!(fs.root().create("more", Kind::File, FILE).err(), Some(Error::ReadOnly));
    assert_checks(&image_of(&disk, image.len()));
    // Linux refuses too: the log must be replayed, which writes.
    assert_eq!(mount(&image).err(), Some(Error::ReadOnly));
}
