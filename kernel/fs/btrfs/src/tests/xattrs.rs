// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Extended attributes and access lists (ADR 0256): read from what
//! `mkfs.btrfs --rootdir` copies of a Linux file's, and written as Linux
//! writes them, checked by `btrfs check` and `dump-tree`.

use std::fmt::Write as _;
use std::format;
use std::process::Command;
use std::string::String;
use std::vec::Vec;

use radix_vfs::acl::{self, Acl, tag};
use radix_vfs::{Error, FileSystem, Kind, Setting};

use super::files::mount;
use super::links::{dump, items};
use super::rw::mount_rw;
use super::write::{SIZE, assert_checks, image_of, mkfs_empty, writable};
use super::{DIR, FILE, mkfs, scratch};

/// An access list's attribute value: the owner's, user 1001's, the
/// group's, the mask and the others' entries.
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

/// Sets the attribute `name` of the host's file `path` to `value`, with
/// Python's os.setxattr, which every host that builds Dukovo has (Meson
/// needs it).
fn set_on_host(path: &std::path::Path, name: &str, value: &[u8]) {
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

#[test]
fn reads_the_attributes_linux_keeps() {
    let dir = scratch("xattr-tree");
    std::fs::create_dir_all(&dir).unwrap();
    let file = dir.join("noted");
    std::fs::write(&file, b"tea party").unwrap();
    set_on_host(&file, "user.origin", b"wonderland");
    set_on_host(&file, "system.posix_acl_access", &bobs_list());
    let image = mkfs(&[], &dir);
    let _ = std::fs::remove_dir_all(&dir);
    let fs = mount(&image).unwrap();
    let noted = fs.root().lookup("noted").unwrap();
    assert_eq!(noted.get_attribute(b"user.origin").unwrap(), b"wonderland");
    assert_eq!(noted.get_attribute(acl::ACCESS).unwrap(), bobs_list());
    assert_eq!(noted.get_attribute(b"user.none"), Err(Error::NoAttribute));
    let names = noted.list_attributes().unwrap();
    let mut names: Vec<&[u8]> = names.split(|&b| b == 0).filter(|n| !n.is_empty()).collect();
    names.sort();
    assert_eq!(names, [&b"system.posix_acl_access"[..], b"user.origin"]);
    let list = noted.access_list().unwrap().unwrap();
    assert!(list.entries().contains(&acl::Entry { tag: tag::USER, perm: 6, id: 1001 }));
    // A read-only mount changes none.
    assert_eq!(noted.set_attribute(b"user.origin", b"x", Setting::Either), Err(Error::ReadOnly));
}

#[test]
fn writes_attributes_as_linux_writes_them() {
    let disk = writable(&mkfs_empty(&[]));
    let fs = mount_rw(&disk);
    let root = fs.root();
    let file = root.create("noted", Kind::File, FILE).unwrap();
    let made = file.stat().changed;
    file.set_attribute(b"user.origin", b"wonderland", Setting::Create).unwrap();
    assert!(file.stat().changed >= made);
    assert_eq!(file.set_attribute(b"user.origin", b"x", Setting::Create), Err(Error::Exists));
    assert_eq!(file.set_attribute(b"user.none", b"x", Setting::Replace), Err(Error::NoAttribute));
    // Many, long and short; one replaced, one removed.
    let long: Vec<u8> = (0..4000u32).map(|i| i as u8).collect();
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
    // One that would not fit in a tree block.
    let huge = std::vec![1u8; 17_000];
    assert_eq!(file.set_attribute(b"user.huge", &huge, Setting::Either), Err(Error::NoSpace));
    // A directory and a link have them too.
    let dir = root.create("dir", Kind::Directory, FILE).unwrap();
    dir.set_attribute(acl::DEFAULT, &bobs_list(), Setting::Either).unwrap();
    let link = root.make_link("link", "noted", super::LINK).unwrap();
    link.set_attribute(b"trusted.mark", b"1", Setting::Either).unwrap();
    // A file removed takes its attributes with it.
    let gone = root.create("gone", Kind::File, FILE).unwrap();
    gone.set_attribute(b"user.origin", b"nowhere", Setting::Either).unwrap();
    let gone_id = gone.stat().id;
    drop(gone);
    root.remove("gone").unwrap();
    fs.sync().unwrap();

    let image = image_of(&disk, SIZE as usize);
    assert_checks(&image);
    let tree = dump(&image);
    assert!(tree.contains("name: user.origin\n\t\tdata wonderland"), "{tree}");
    assert!(tree.contains("name: user.n3\n\t\tdata three"));
    assert!(!tree.contains("name: user.n4\n"));
    assert!(tree.contains("name: system.posix_acl_default"));
    assert_eq!(items(&tree, gone_id, "XATTR_ITEM"), 0);
    let id = file.stat().id;
    assert_eq!(items(&tree, id, "XATTR_ITEM"), 22);

    // Read back as written.
    let again = mount(&image).unwrap();
    let file = again.root().lookup("noted").unwrap();
    assert_eq!(file.get_attribute(b"user.long").unwrap(), long);
    assert_eq!(file.get_attribute(b"user.n3").unwrap(), b"three");
    assert_eq!(
        Acl::parse(&file.get_attribute(acl::ACCESS).unwrap()).unwrap().unwrap().mode_bits(),
        0o664
    );
    let names = file.list_attributes().unwrap();
    assert_eq!(names.iter().filter(|&&b| b == 0).count(), 22);
}

/// A directory keeps its default list once read, for each node made in
/// it, and reads it again after it changes.
#[test]
fn a_kept_default_list_follows_its_changes() {
    let disk = writable(&mkfs_empty(&[]));
    let fs = mount_rw(&disk);
    let root = fs.root();
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
