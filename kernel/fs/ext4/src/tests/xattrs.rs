// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Extended attributes and access lists (ADR 0256): read from what
//! `mkfs.ext4 -d` copies of a Linux file's, and written as Linux writes
//! them, in the inode and in a block of their own, checked by `e2fsck`
//! and read back by `debugfs`.

use std::fmt::Write as _;
use std::format;
use std::path::Path;
use std::process::Command;
use std::string::String;
use std::vec::Vec;

use radix_vfs::acl::{self, Acl, tag};
use radix_vfs::{Error, FileSystem, Kind, Node, Setting};

use super::{
    DIR, FILE, LINK, VOLUMES, Volume, debugfs, fsck, fsck_report, mkfs, mkfs_from, scratch,
};

/// An access list's attribute value, in Linux's `posix_acl_xattr` form:
/// the owner's, user 1001's, the group's, the mask and the others'
/// entries.
fn bobs_list() -> Vec<u8> {
    let mut value = 2u32.to_le_bytes().to_vec();
    for (tag, perm, id) in [
        (tag::USER_OBJ, 6u16, u32::MAX),
        (tag::USER, 6, 1001),
        (tag::GROUP_OBJ, 4, u32::MAX),
        (tag::MASK, 6, u32::MAX),
        (tag::OTHER, 4, u32::MAX),
    ] {
        value.extend_from_slice(&tag.to_le_bytes());
        value.extend_from_slice(&perm.to_le_bytes());
        value.extend_from_slice(&id.to_le_bytes());
    }
    value
}

/// The same list as ext4 keeps it, as `debugfs` shows it.
const BOBS_LIST_ON_DISK: &str =
    "01 00 00 00 01 00 06 00 02 00 06 00 e9 03 00 00 04 00 04 00 10 00 06 00 20 00 04 00";

/// Sets the attribute `name` of the host's file `path` to `value`, with
/// Python's os.setxattr, which every host that builds Dukovo has (Meson
/// needs it).
fn set_on_host(path: &Path, name: &str, value: &[u8]) {
    let hex = value.iter().fold(String::new(), |mut hex, b| {
        let _ = write!(hex, "{b:02x}");
        hex
    });
    let out = Command::new("python3")
        .args([
            "-c",
            "import os, sys; os.setxattr(sys.argv[1], sys.argv[2], bytes.fromhex(sys.argv[3]))",
        ])
        .arg(path)
        .args([name, &hex])
        .output()
        .unwrap();
    assert!(out.status.success(), "setxattr: {}", String::from_utf8_lossy(&out.stderr));
}

/// The names `node` lists, sorted.
fn names_of(node: &dyn Node) -> Vec<Vec<u8>> {
    let names = node.list_attributes().unwrap();
    let mut names: Vec<Vec<u8>> =
        names.split(|&b| b == 0).filter(|n| !n.is_empty()).map(<[u8]>::to_vec).collect();
    names.sort();
    names
}

/// The block of attributes of the file at `path`, as `debugfs` says.
fn file_acl(image: &mut Vec<u8>, path: &str) -> u64 {
    let stat = debugfs(image, false, &format!("stat {path}"));
    let at = stat.find("File ACL: ").unwrap_or_else(|| panic!("{stat}"));
    stat[at + 10..].split(|c: char| !c.is_ascii_digit()).next().unwrap().parse().unwrap()
}

#[test]
fn reads_the_attributes_linux_keeps() {
    let dir = scratch("xattr-tree");
    std::fs::create_dir_all(&dir).unwrap();
    let file = dir.join("noted");
    std::fs::write(&file, b"tea party").unwrap();
    set_on_host(&file, "user.origin", b"wonderland");
    set_on_host(&file, "system.posix_acl_access", &bobs_list());
    // Too large for the inode: in a block.
    let big: Vec<u8> = (0..1500u32).map(|i| i as u8).collect();
    set_on_host(&file, "user.big", &big);
    for (what, mib, args) in [VOLUMES[0], VOLUMES[6]] {
        let mut image = mkfs_from(mib, args, Some(&dir));
        assert_ne!(file_acl(&mut image, "/noted"), 0, "{what}");
        let volume = Volume::mount(&image, true).unwrap();
        let noted = volume.root().lookup("noted").unwrap();
        assert_eq!(noted.get_attribute(b"user.origin").unwrap(), b"wonderland", "{what}");
        assert_eq!(noted.get_attribute(b"user.big").unwrap(), big, "{what}");
        assert_eq!(noted.get_attribute(acl::ACCESS).unwrap(), bobs_list(), "{what}");
        assert_eq!(noted.get_attribute(b"user.none"), Err(Error::NoAttribute));
        assert_eq!(noted.get_attribute(acl::DEFAULT), Err(Error::NoAttribute));
        assert_eq!(noted.get_attribute(b"other.name"), Err(Error::NotSupported));
        assert_eq!(
            names_of(&*noted),
            [&b"system.posix_acl_access"[..], b"user.big", b"user.origin"]
        );
        let list = noted.access_list().unwrap().unwrap();
        assert!(list.entries().contains(&acl::Entry { tag: tag::USER, perm: 6, id: 1001 }));
        // A read-only mount changes none.
        assert_eq!(
            noted.set_attribute(b"user.origin", b"x", Setting::Either),
            Err(Error::ReadOnly)
        );
    }
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn writes_attributes_as_linux_writes_them() {
    for (what, mib, args) in VOLUMES {
        let volume = Volume::new(&mkfs(mib, args));
        let root = volume.root();
        let block = volume.fs().info().block_size as usize;
        let file = root.create("noted", Kind::File, FILE).unwrap();
        file.set_attribute(b"user.origin", b"wonderland", Setting::Create).unwrap();
        assert_eq!(
            file.set_attribute(b"user.origin", b"x", Setting::Create),
            Err(Error::Exists),
            "{what}"
        );
        assert_eq!(
            file.set_attribute(b"user.none", b"x", Setting::Replace),
            Err(Error::NoAttribute)
        );
        // Many, long and short, past the inode's room; one replaced, one
        // removed.
        let long: Vec<u8> = (0..300u32).map(|i| i as u8).collect();
        file.set_attribute(b"user.long", &long, Setting::Either).unwrap();
        for i in 0..20 {
            let name = format!("user.n{i}");
            file.set_attribute(name.as_bytes(), name.as_bytes(), Setting::Either).unwrap();
        }
        file.set_attribute(b"user.n3", b"three", Setting::Replace).unwrap();
        file.remove_attribute(b"user.n4").unwrap();
        assert_eq!(file.remove_attribute(b"user.n4"), Err(Error::NoAttribute));
        file.set_attribute(acl::ACCESS, &bobs_list(), Setting::Either).unwrap();
        assert!(file.access_list().unwrap().is_some());
        // One that would not fit in a block.
        let huge = std::vec![1u8; block];
        assert_eq!(
            file.set_attribute(b"user.huge", &huge, Setting::Either),
            Err(Error::NoSpace),
            "{what}"
        );
        // A directory and a link have them too: a short link's block is
        // its only one.
        let dir = root.create("dir", Kind::Directory, DIR).unwrap();
        dir.set_attribute(acl::DEFAULT, &bobs_list(), Setting::Either).unwrap();
        let link = root.make_link("link", "noted", LINK).unwrap();
        let mark = std::vec![b'm'; 200];
        link.set_attribute(b"trusted.mark", &mark, Setting::Either).unwrap();
        // A file removed takes its attributes' block with it.
        let gone = root.create("gone", Kind::File, FILE).unwrap();
        gone.set_attribute(b"user.origin", &long, Setting::Either).unwrap();
        drop(gone);
        root.remove("gone").unwrap();
        drop((file, dir, link, root));

        let mut image = volume.unmount();
        let (clean, text) = fsck_report(&image);
        assert!(clean, "{what}: e2fsck found problems:\n{text}");
        let listed = debugfs(&mut image, false, "ea_list /noted");
        assert!(listed.contains("user.origin (10) = \"wonderland\""), "{what}: {listed}");
        assert!(listed.contains("user.n3 (5) = \"three\""), "{what}: {listed}");
        assert!(listed.contains("user.n5 (7) = \"user.n5\""), "{what}: {listed}");
        assert!(!listed.contains("user.n4 "), "{what}: {listed}");
        assert!(listed.contains(&format!("system.posix_acl_access (28) = {BOBS_LIST_ON_DISK}")));
        let listed = debugfs(&mut image, false, "ea_list /dir");
        assert!(listed.contains(&format!("system.posix_acl_default (28) = {BOBS_LIST_ON_DISK}")));
        let listed = debugfs(&mut image, false, "ea_list /link");
        assert!(listed.contains("trusted.mark (200)"), "{what}: {listed}");
        let stat = debugfs(&mut image, false, "stat /link");
        assert!(stat.contains("Fast link dest: \"noted\""), "{what}: {stat}");

        // Read back as written.
        let again = Volume::new(&image);
        let file = again.root().lookup("noted").unwrap();
        assert_eq!(file.get_attribute(b"user.long").unwrap(), long);
        assert_eq!(file.get_attribute(b"user.n3").unwrap(), b"three");
        assert_eq!(file.get_attribute(acl::ACCESS).unwrap(), bobs_list());
        let list = Acl::parse(&file.get_attribute(acl::ACCESS).unwrap()).unwrap().unwrap();
        assert_eq!(list.mode_bits(), 0o664);
        assert_eq!(names_of(&*file).len(), 22, "{what}");
    }
}

#[test]
fn attributes_move_between_the_inode_and_its_block() {
    let volume = Volume::new(&mkfs(32, &[]));
    let root = volume.root();
    let file = root.create("noted", Kind::File, FILE).unwrap();
    // A 256-byte inode keeps 88 bytes of attributes: one of these.
    file.set_attribute(b"user.a", &[b'a'; 40], Setting::Either).unwrap();
    file.set_attribute(b"user.b", &[b'b'; 40], Setting::Either).unwrap();
    let mut image = volume.synced();
    fsck(&image);
    assert_ne!(file_acl(&mut image, "/noted"), 0);
    // Room in the inode again: b moves there, and its block goes.
    file.remove_attribute(b"user.a").unwrap();
    file.set_attribute(b"user.b", &[b'B'; 41], Setting::Either).unwrap();
    // The same value changes nothing.
    file.set_attribute(b"user.b", &[b'B'; 41], Setting::Either).unwrap();
    let mut image = volume.synced();
    fsck(&image);
    assert_eq!(file_acl(&mut image, "/noted"), 0);
    let listed = debugfs(&mut image, false, "ea_list /noted");
    assert!(listed.contains("user.b (41)"), "{listed}");
    assert!(!listed.contains("user.a"), "{listed}");
    // Removing the last leaves an inode with none.
    file.remove_attribute(b"user.b").unwrap();
    let mut image = volume.synced();
    fsck(&image);
    assert!(!debugfs(&mut image, false, "ea_list /noted").contains("user."));
    assert_eq!(names_of(&*file).len(), 0);
}

#[test]
fn a_shared_block_is_copied_before_it_changes() {
    // Linux shares a block between inodes with the same attributes: here
    // b is given a's, as Linux would, with two references.
    let dir = scratch("xattr-shared");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("a"), b"").unwrap();
    std::fs::write(dir.join("b"), b"").unwrap();
    set_on_host(&dir.join("a"), "user.shared", b"both");
    let mut image = mkfs_from(16, &["-I", "128", "-O", "^metadata_csum,^uninit_bg"], Some(&dir));
    let _ = std::fs::remove_dir_all(&dir);
    let shared = file_acl(&mut image, "/a");
    debugfs(&mut image, true, &format!("set_inode_field /b file_acl {shared}"));
    debugfs(&mut image, true, "set_inode_field /b blocks 8");
    debugfs(&mut image, true, &format!("zap_block -o 4 -l 1 -p 2 {shared}"));
    fsck(&image);

    let volume = Volume::new(&image);
    let root = volume.root();
    let b = root.lookup("b").unwrap();
    assert_eq!(b.get_attribute(b"user.shared").unwrap(), b"both");
    b.set_attribute(b"user.mine", b"bee", Setting::Either).unwrap();
    // a's block is its own now, and changes in place.
    let a = root.lookup("a").unwrap();
    a.set_attribute(b"user.shared", b"ayy", Setting::Either).unwrap();
    drop((a, b, root));
    let mut image = volume.unmount();
    fsck(&image);
    assert_eq!(file_acl(&mut image, "/a"), shared);
    assert_ne!(file_acl(&mut image, "/b"), shared);
    let listed = debugfs(&mut image, false, "ea_list /a");
    assert!(listed.contains("user.shared (3) = \"ayy\""), "{listed}");
    let listed = debugfs(&mut image, false, "ea_list /b");
    assert!(listed.contains("user.shared (4) = \"both\""), "{listed}");
    assert!(listed.contains("user.mine (3) = \"bee\""), "{listed}");
}

/// A directory keeps its default list once read, for each node made in
/// it, and reads it again after it changes.
#[test]
fn a_kept_default_list_follows_its_changes() {
    let volume = Volume::new(&mkfs(32, &[]));
    let root = volume.root();
    let dir = root.create("shared", Kind::Directory, DIR).unwrap();
    assert!(dir.default_list().unwrap().is_none());
    dir.set_attribute(acl::DEFAULT, &bobs_list(), Setting::Either).unwrap();
    let bob = acl::Entry { tag: tag::USER, perm: 6, id: 1001 };
    assert!(dir.default_list().unwrap().unwrap().entries().contains(&bob));
    // User 1002 in place of 1001: read again, not the kept list.
    let mut carols = bobs_list();
    carols[16..20].copy_from_slice(&1002u32.to_le_bytes());
    dir.set_attribute(acl::DEFAULT, &carols, Setting::Replace).unwrap();
    let list = dir.default_list().unwrap().unwrap();
    assert!(list.entries().contains(&acl::Entry { id: 1002, ..bob }));
    assert!(!list.entries().contains(&bob));
    dir.remove_attribute(acl::DEFAULT).unwrap();
    assert!(dir.default_list().unwrap().is_none());
    // A file has no default list.
    let file = root.create("noted", Kind::File, FILE).unwrap();
    assert!(file.default_list().unwrap().is_none());
}
