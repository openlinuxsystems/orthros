// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Hard links and special files (ADR 0135), and owners and modes (ADR
//! 0213), checked by `btrfs check` and by what Linux's `btrfs
//! inspect-internal dump-tree` reads of them.

use std::format;
use std::os::unix::fs::MetadataExt;
use std::process::Command;
use std::string::String;

use radix_block::DeviceRef;
use radix_block::ram::RamDisk;
use radix_vfs::mem::{self, KArc};
use radix_vfs::{Access, AccessChange, Error, FileSystem, Kind, NodeRef, Slice, device_number};

use super::files::{contents, mount};
use super::write::{SIZE, assert_checks, image_of, mkfs_empty, writable, write_sparse};
use super::{DIR, FILE, LINK};
use super::{mkfs, scratch, tool};
use crate::fs::BtrfsFs;

/// The filesystem on `disk`, mounted to write.
fn mount_rw(disk: &KArc<RamDisk>) -> KArc<BtrfsFs> {
    let device: DeviceRef = disk.clone();
    BtrfsFs::mount_writable(device).unwrap()
}

/// What `btrfs inspect-internal dump-tree` prints of the top subvolume's
/// tree in `image`.
pub(super) fn dump(image: &[u8]) -> String {
    let path = scratch("dump");
    write_sparse(&path, image);
    let out = Command::new(tool("btrfs"))
        .args(["inspect-internal", "dump-tree", "-t", "5"])
        .arg(&path)
        .stdin(std::process::Stdio::null())
        .output()
        .unwrap();
    let _ = std::fs::remove_file(&path);
    assert!(out.status.success(), "dump-tree: {}", String::from_utf8_lossy(&out.stderr));
    String::from_utf8_lossy(&out.stdout).into_owned()
}

/// A field of inode `ino`'s item, as `dump-tree` prints it.
fn inode_field(dump: &str, ino: u64, field: &str) -> u64 {
    // Not a directory entry's "location key".
    let key = format!("key ({ino} INODE_ITEM 0) itemoff");
    let at = dump.find(&key).unwrap_or_else(|| panic!("no inode {ino}"));
    let mut words = dump[at..].split_whitespace();
    while let Some(word) = words.next() {
        if word == field {
            return words.next().unwrap().parse().unwrap();
        }
    }
    panic!("inode {ino} has no {field}");
}

/// How many items of kind `kind` inode `ino` has, as `dump-tree` prints
/// them.
pub(super) fn items(dump: &str, ino: u64, kind: &str) -> usize {
    let key = format!("key ({ino} {kind} ");
    dump.lines()
        .filter(|line| line.trim_start().starts_with("item ") && line.contains(&key))
        .count()
}

fn put(file: &NodeRef, data: &[u8]) {
    assert_eq!(file.write(0, &Slice(data)).unwrap(), data.len());
}

#[test]
fn hard_links_are_names_of_one_file_that_btrfs_check_counts() {
    let disk = writable(&mkfs_empty(&[]));
    let fs = mount_rw(&disk);
    let root = fs.root();
    let file = root.create("a", Kind::File, FILE).unwrap();
    put(&file, b"one inode");
    let sub = root.create("sub", Kind::Directory, DIR).unwrap();
    sub.link("b", &file).unwrap();
    root.link("c", &file).unwrap();
    assert_eq!(file.stat().links, 3);
    let c = root.lookup("c").unwrap();
    assert_eq!(c.stat().id, file.stat().id);
    put(&c, b"ONE");
    assert_eq!(contents(&*sub.lookup("b").unwrap(), 4096).unwrap(), b"ONE inode");
    assert_eq!(root.link("a", &file), Err(Error::Exists));
    assert_eq!(root.link("d", &sub), Err(Error::IsDirectory));
    // A directory has one name, as Btrfs counts it.
    assert_eq!(sub.stat().links, 1);
    fs.sync().unwrap();
    let image = image_of(&disk, SIZE as usize);
    assert_checks(&image);
    let ino = file.stat().id;
    assert_eq!(inode_field(&dump(&image), ino, "links"), 3);
    let ro = mount(&image).unwrap();
    assert_eq!(ro.root().lookup("c").unwrap().stat().links, 3);
    assert_eq!(ro.root().link("e", &ro.root().lookup("a").unwrap()), Err(Error::ReadOnly));

    // Names go, and move, one at a time; the file goes with the last.
    root.remove("a").unwrap();
    sub.rename("b", &*root, "d").unwrap();
    // Renaming a name onto another of the same file changes nothing.
    root.rename("c", &*root, "d").unwrap();
    assert_eq!(c.stat().links, 2);
    fs.sync().unwrap();
    let image = image_of(&disk, SIZE as usize);
    assert_checks(&image);
    assert_eq!(inode_field(&dump(&image), ino, "links"), 2);
    root.remove("c").unwrap();
    root.remove("d").unwrap();
    drop((file, c));
    fs.sync().unwrap();
    let image = image_of(&disk, SIZE as usize);
    assert_checks(&image);
    assert_eq!(items(&dump(&image), ino, "INODE_ITEM"), 0);
}

/// A name of `len` bytes, the `n`th.
fn long_name(n: usize, len: usize) -> String {
    let mut name = format!("name-{n:04}-");
    while name.len() < len {
        name.push('x');
    }
    name
}

#[test]
fn names_past_a_full_reference_item_go_to_extended_references() {
    let disk = writable(&mkfs_empty(&[]));
    let fs = mount_rw(&disk);
    let root = fs.root();
    let dir = root.create("dir", Kind::Directory, DIR).unwrap();
    let file = dir.create(&long_name(0, 200), Kind::File, FILE).unwrap();
    put(&file, b"many names");
    let ino = file.stat().id;
    // A 16 KiB block's item holds 77 names of 200 bytes.
    for n in 1..300 {
        dir.link(&long_name(n, 200), &file).unwrap();
    }
    assert_eq!(file.stat().links, 300);
    fs.sync().unwrap();
    let image = image_of(&disk, SIZE as usize);
    assert_checks(&image);
    let tree = dump(&image);
    assert_eq!(items(&tree, ino, "INODE_REF"), 1);
    assert!(items(&tree, ino, "INODE_EXTREF") >= 200, "{}", items(&tree, ino, "INODE_EXTREF"));
    // Names go from either kind of reference.
    for n in (0..300).step_by(3) {
        dir.remove(&long_name(n, 200)).unwrap();
    }
    dir.rename(&long_name(1, 200), &*root, "moved").unwrap();
    dir.rename(&long_name(299, 200), &*root, "moved-too").unwrap();
    assert_eq!(file.stat().links, 200);
    fs.sync().unwrap();
    let image = image_of(&disk, SIZE as usize);
    assert_checks(&image);
    // And from a mount that did not make them.
    drop((file, dir, root));
    drop(fs);
    let again = writable(&image);
    let fs = mount_rw(&again);
    let dir = fs.root().lookup("dir").unwrap();
    for n in (0..300).filter(|n| n % 3 != 0 && *n != 1 && *n != 299) {
        dir.remove(&long_name(n, 200)).unwrap();
    }
    fs.root().remove("moved").unwrap();
    assert_eq!(fs.root().lookup("moved-too").unwrap().stat().links, 1);
    fs.sync().unwrap();
    let image = image_of(&again, SIZE as usize);
    assert_checks(&image);
    let tree = dump(&image);
    assert_eq!(items(&tree, ino, "INODE_EXTREF") + items(&tree, ino, "INODE_REF"), 1);
}

#[test]
fn without_extended_references_a_full_item_refuses_more_names() {
    let disk = writable(&mkfs_empty(&["-O", "^extref"]));
    let fs = mount_rw(&disk);
    let root = fs.root();
    let file = root.create(&long_name(0, 200), Kind::File, FILE).unwrap();
    let mut n = 1;
    let refused = loop {
        match root.link(&long_name(n, 200), &file) {
            Ok(()) => n += 1,
            Err(e) => break e,
        }
    };
    assert_eq!(refused, Error::TooManyNames);
    assert_eq!(n, 77);
    // The filesystem goes on.
    root.create("after", Kind::File, FILE).unwrap();
    fs.sync().unwrap();
    let image = image_of(&disk, SIZE as usize);
    assert_checks(&image);
}

#[test]
fn files_linux_made_with_extended_references_lose_their_names_whole() {
    let dir = scratch("extrefs");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join(long_name(0, 200)), b"many names").unwrap();
    for n in 1..150 {
        std::fs::hard_link(dir.join(long_name(0, 200)), dir.join(long_name(n, 200))).unwrap();
    }
    let image = super::rw::mkfs_with(&[], &dir);
    let ino = mount(&image).unwrap().root().lookup(&long_name(0, 200)).unwrap().stat().id;
    assert!(items(&dump(&image), ino, "INODE_EXTREF") > 0);
    let disk = writable(&image);
    let fs = mount_rw(&disk);
    let root = fs.root();
    assert_eq!(root.lookup(&long_name(7, 200)).unwrap().stat().links, 150);
    for n in (0..150).step_by(2) {
        root.remove(&long_name(n, 200)).unwrap();
    }
    root.rename(&long_name(149, 200), &*root, "last").unwrap();
    fs.sync().unwrap();
    let image = image_of(&disk, SIZE as usize);
    assert_checks(&image);
    assert_eq!(inode_field(&dump(&image), ino, "links"), 75);
}

#[test]
fn special_files_are_kept_as_linux_keeps_them() {
    let disk = writable(&mkfs_empty(&[]));
    let fs = mount_rw(&disk);
    let root = fs.root();
    let null = device_number(1, 3);
    // A minor number past 8 bits, and the most Linux keeps.
    let block = device_number(259, 70_000);
    let widest = device_number(0xfff, 0xf_ffff);
    let made = [
        ("fifo", Kind::Pipe, 0),
        ("null", Kind::CharDevice, null),
        ("nvme", Kind::BlockDevice, block),
        ("widest", Kind::CharDevice, widest),
        ("socket", Kind::Socket, 0),
        ("file", Kind::File, 0),
    ];
    for (name, kind, device) in made {
        let node = root.make_node(name, kind, device, Access::default_for(kind)).unwrap();
        assert_eq!((node.kind(), node.stat().device), (kind, device), "{name}");
    }
    assert_eq!(
        root.make_node("fifo", Kind::Pipe, 0, Access::default_for(Kind::Pipe)).err(),
        Some(Error::Exists)
    );
    assert_eq!(root.make_node("dir", Kind::Directory, 0, DIR).err(), Some(Error::NotSupported));
    // A socket's name binds while the filesystem is mounted.
    let binding: radix_vfs::SocketBinding = mem::arc(7u32).unwrap();
    let bound = root.make_socket("bound", binding, Access::default_for(Kind::Socket)).unwrap();
    assert_eq!(bound.socket_binding().unwrap().downcast_ref::<u32>(), Some(&7));
    assert!(root.lookup("socket").unwrap().socket_binding().is_none());
    // Special files hold nothing to read.
    let fifo = root.lookup("fifo").unwrap();
    assert_eq!(fifo.read(0, &mut radix_vfs::SliceMut(&mut [0; 8])), Err(Error::NotSupported));
    fs.sync().unwrap();
    let image = image_of(&disk, SIZE as usize);
    assert_checks(&image);
    let tree = dump(&image);
    let rdev = |name: &str| inode_field(&tree, root.lookup(name).unwrap().stat().id, "rdev");
    assert_eq!(rdev("null"), (1 << 20) | 3);
    assert_eq!(rdev("nvme"), (259 << 20) + 70_000);
    assert_eq!(rdev("widest"), 0xffff_ffff);
    assert_eq!(rdev("fifo"), 0);
    let mode = |name: &str| inode_field(&tree, root.lookup(name).unwrap().stat().id, "mode");
    assert_eq!(
        [mode("fifo"), mode("null"), mode("nvme"), mode("socket")],
        [10600, 20666, 60660, 140777]
    );
    let ro = mount(&image).unwrap();
    for (name, kind, device) in made {
        let node = ro.root().lookup(name).unwrap();
        assert_eq!(
            (node.kind(), node.stat().device, node.stat().links),
            (kind, device, 1),
            "{name}"
        );
    }
    assert_eq!(
        ro.root().make_node("x", Kind::Pipe, 0, Access::default_for(Kind::Pipe)).err(),
        Some(Error::ReadOnly)
    );
    // What was bound is gone at the next mount; the name stays.
    assert!(ro.root().lookup("bound").unwrap().socket_binding().is_none());
    // The numbers beyond Linux's 32 bits are dropped, as Linux drops them.
    let wide = root
        .make_node(
            "wide",
            Kind::CharDevice,
            device_number(0x1001, 0x10_0001),
            Access::default_for(Kind::CharDevice),
        )
        .unwrap();
    assert_eq!(wide.stat().device, device_number(1, 1));
    for name in ["fifo", "null", "nvme", "widest", "wide", "socket", "bound", "file"] {
        root.remove(name).unwrap();
    }
    drop((bound, fifo, wide));
    fs.sync().unwrap();
    let image = image_of(&disk, SIZE as usize);
    assert_checks(&image);
}

#[test]
fn special_files_linux_made_read_as_linux_made_them() {
    let dir = scratch("special");
    std::fs::create_dir_all(&dir).unwrap();
    let fifo = Command::new("mkfifo").arg(dir.join("fifo")).status().unwrap();
    assert!(fifo.success());
    std::fs::write(dir.join("a"), b"two names").unwrap();
    std::fs::hard_link(dir.join("a"), dir.join("b")).unwrap();
    let image = mkfs(&[], &dir);
    let fs = mount(&image).unwrap();
    let root = fs.root();
    assert_eq!(root.lookup("fifo").unwrap().kind(), Kind::Pipe);
    let meta = std::fs::metadata(dir.join("a")).unwrap();
    assert_eq!(u64::from(root.lookup("b").unwrap().stat().links), meta.nlink());
    assert_eq!(root.lookup("a").unwrap().stat().id, root.lookup("b").unwrap().stat().id);
    // Owners and modes are Linux's.
    for name in ["a", "fifo"] {
        let meta = std::fs::metadata(dir.join(name)).unwrap();
        let mode = (meta.mode() & 0o7777) as u16;
        let access = Access { owner: meta.uid(), group: meta.gid(), mode };
        assert_eq!(root.lookup(name).unwrap().access(), access, "{name}");
    }
}

#[test]
fn owners_and_modes_are_kept_as_linux_keeps_them() {
    let disk = writable(&mkfs_empty(&[]));
    let fs = mount_rw(&disk);
    let root = fs.root();
    let chmod = |mode| AccessChange { mode: Some(mode), ..AccessChange::default() };
    let mine = Access { owner: 1000, group: 100, mode: 0o640 };
    let file = root.create("file", Kind::File, mine).unwrap();
    assert_eq!(file.access(), mine);
    root.create("dir", Kind::Directory, Access { mode: 0o2775, ..mine }).unwrap();
    let link = root.make_link("link", "file", LINK).unwrap();
    // A chown past 16 bits takes the set-user-ID bit away.
    file.set_access(&chmod(0o4755)).unwrap();
    file.set_access(&AccessChange::chown(Kind::File, Some(70_000), None)).unwrap();
    let chowned = Access { owner: 70_000, group: 100, mode: 0o755 };
    assert_eq!(file.access(), chowned);
    assert_eq!(link.set_access(&chmod(0o700)), Err(Error::NotSupported));
    link.set_access(&AccessChange::chown(Kind::Symlink, None, Some(5))).unwrap();
    // A file still open without a name changes, as on Linux.
    let gone = root.create("gone", Kind::File, FILE).unwrap();
    root.remove("gone").unwrap();
    gone.set_access(&chmod(0o600)).unwrap();
    assert_eq!(gone.access(), Access { mode: 0o600, ..FILE });
    drop(gone);
    fs.sync().unwrap();
    let image = image_of(&disk, SIZE as usize);
    assert_checks(&image);
    let tree = dump(&image);
    let field = |name: &str, field| inode_field(&tree, root.lookup(name).unwrap().stat().id, field);
    let fields = |name| [field(name, "uid"), field(name, "gid"), field(name, "mode")];
    assert_eq!(fields("file"), [70_000, 100, 100_755]);
    assert_eq!(fields("dir"), [1000, 100, 42_775]);
    assert_eq!(fields("link"), [0, 5, 120_777]);
    let ro = mount(&image).unwrap();
    let file = ro.root().lookup("file").unwrap();
    assert_eq!(file.access(), chowned);
    assert_eq!(file.set_access(&chmod(0o600)), Err(Error::ReadOnly));
}
