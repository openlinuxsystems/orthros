// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! The VFS on the host: paths, resolution beneath a directory, mounts,
//! tmpfs, the page cache against a model disk, synthetic files and the
//! boot archive.

use std::collections::BTreeMap;
use std::string::String;
use std::sync::Mutex as StdMutex;
use std::vec::Vec;

use proptest::prelude::*;

use crate::bootfs::BootFs;
use crate::cache::{Backing, FileCache};
use crate::mount::{Location, OpenFlags, Vfs};
use crate::path::{self, Component};
use crate::synthetic::{FixedDir, Generated, Null, SyntheticFs, Zero};
use crate::tmpfs::Tmpfs;
use crate::{
    APPEND, Error, FileSystem, FsKind, FsRef, Kind, Node, NodeRef, Result, Slice, SliceMut, mem,
};

fn tmpfs(max: u64) -> FsRef {
    Tmpfs::new(max, u64::MAX).unwrap()
}

fn vfs() -> Vfs {
    Vfs::new(tmpfs(1 << 24), "tmpfs").unwrap()
}

fn create() -> OpenFlags {
    OpenFlags { create: true, ..OpenFlags::default() }
}

fn write(vfs: &Vfs, base: &Location, path: &str, data: &[u8]) -> Result<()> {
    let file = vfs.open(base, path, create())?;
    assert_eq!(file.node.write(0, &Slice(data))?, data.len());
    Ok(())
}

fn read_all(node: &NodeRef) -> Vec<u8> {
    let mut out = std::vec![0u8; 1 << 16];
    let n = node.read(0, &mut SliceMut(&mut out[..])).unwrap();
    out.truncate(n);
    out
}

fn names(vfs: &Vfs, at: &Location) -> Vec<(String, Kind)> {
    let mut found = Vec::new();
    vfs.list(at, 0, &mut |_, name, kind| {
        found.push((String::from(name), kind));
        true
    })
    .unwrap();
    found
}

#[test]
fn paths_are_checked() {
    assert_eq!(path::parse(b"a/b"), Ok("a/b"));
    assert_eq!(path::parse(b"/etc"), Err(Error::Outside));
    assert_eq!(path::parse(b"a\0b"), Err(Error::BadName));
    assert_eq!(path::parse(&[0xff]), Err(Error::BadName));
    assert_eq!(path::parse(&[b'a'; 4097]), Err(Error::NameTooLong));
    let parts: Vec<_> = path::components("a//./../b/").collect::<Result<_>>().unwrap();
    assert_eq!(
        parts,
        [Component::Name("a"), Component::Current, Component::Parent, Component::Name("b")]
    );
    assert_eq!(path::components("").count(), 0);
    assert_eq!(path::components("//").count(), 0);
    let parts: Vec<_> = path::components("é/x.y/.../").collect::<Result<_>>().unwrap();
    assert_eq!(parts, [Component::Name("é"), Component::Name("x.y"), Component::Name("...")]);
    let long = std::format!("a/{}/b", "x".repeat(256));
    assert_eq!(path::components(&long).nth(1), Some(Err(Error::NameTooLong)));
    assert_eq!(path::split_last("a/b/c"), Ok(("a/b", "c")));
    assert_eq!(path::split_last("c/"), Ok(("", "c")));
    assert_eq!(path::split_last("a/.."), Err(Error::BadName));
    assert_eq!(path::split_last(""), Err(Error::BadName));
    assert_eq!(path::check_name(&"x".repeat(256)), Err(Error::NameTooLong));
    assert!(path::check_name(&"x".repeat(255)).is_ok());
}

#[test]
fn tmpfs_files_and_directories() {
    let vfs = vfs();
    let root = vfs.root();
    vfs.make_directory(&root, "etc").unwrap();
    assert_eq!(vfs.make_directory(&root, "etc"), Err(Error::Exists));
    write(&vfs, &root, "etc/hostname", b"dukovo\n").unwrap();
    let file = vfs.open(&root, "etc/hostname", OpenFlags::default()).unwrap();
    assert_eq!(read_all(&file.node), b"dukovo\n");
    assert_eq!(file.node.stat().size, 7);
    assert_eq!(file.node.write(APPEND, &Slice(b"more")).unwrap(), 4);
    assert_eq!(read_all(&file.node), b"dukovo\nmore");
    // A hole reads as zeros.
    file.node.write(10_000, &Slice(b"!")).unwrap();
    let mut out = [1u8; 4];
    assert_eq!(file.node.read(5000, &mut SliceMut(&mut out[..])).unwrap(), 4);
    assert_eq!(out, [0; 4]);
    file.node.set_size(3).unwrap();
    assert_eq!(read_all(&file.node), b"duk");
    let excl = OpenFlags { create: true, exclusive: true, ..OpenFlags::default() };
    assert_eq!(vfs.open(&root, "etc/hostname", excl).err(), Some(Error::Exists));
    let trunc = OpenFlags { truncate: true, ..OpenFlags::default() };
    let file = vfs.open(&root, "etc/hostname", trunc).unwrap();
    assert_eq!(file.node.stat().size, 0);
    assert_eq!(vfs.remove(&root, "etc"), Err(Error::NotEmpty));
    assert_eq!(
        vfs.open(&root, "etc/hostname/x", OpenFlags::default()).err(),
        Some(Error::NotDirectory)
    );
    let dir = OpenFlags { directory: true, ..OpenFlags::default() };
    assert_eq!(vfs.open(&root, "etc/hostname", dir).err(), Some(Error::NotDirectory));
    vfs.remove(&root, "etc/hostname").unwrap();
    // The handle still works after the name is gone.
    file.node.write(0, &Slice(b"kept")).unwrap();
    assert_eq!(read_all(&file.node), b"kept");
    vfs.remove(&root, "etc").unwrap();
    assert!(names(&vfs, &root).is_empty());
}

#[test]
fn tmpfs_times_are_set() {
    let vfs = vfs();
    let root = vfs.root();
    write(&vfs, &root, "file", b"x").unwrap();
    vfs.make_directory(&root, "dir").unwrap();
    let file = vfs.resolve(&root, "file").unwrap().node;
    let dir = vfs.resolve(&root, "dir").unwrap().node;
    let made = file.stat();
    assert_eq!(made.accessed, made.created);
    // Reading leaves the read time.
    read_all(&file);
    assert_eq!(file.stat().accessed, made.created);
    file.set_times(Some(1), Some(1 << 62)).unwrap();
    assert_eq!((file.stat().accessed, file.stat().modified), (1, 1 << 62));
    file.set_times(None, Some(7)).unwrap();
    assert_eq!((file.stat().accessed, file.stat().modified), (1, 7));
    dir.set_times(Some(5), Some(6)).unwrap();
    assert_eq!((dir.stat().accessed, dir.stat().modified), (5, 6));
    // Devices keep no times.
    assert_eq!(Null(9).set_times(Some(1), None), Err(Error::NotSupported));
}

#[test]
fn tmpfs_has_a_file_limit() {
    let vfs = Vfs::new(Tmpfs::new(1 << 20, 3).unwrap(), "tmpfs").unwrap();
    let root = vfs.root();
    let info = || root.mount.fs.info();
    assert_eq!((info().files, info().free_files), (3, 2));
    write(&vfs, &root, "a", b"x").unwrap();
    vfs.make_directory(&root, "b").unwrap();
    assert_eq!(info().free_files, 0);
    assert_eq!(write(&vfs, &root, "c", b"x"), Err(Error::NoSpace));
    assert_eq!(vfs.make_directory(&root, "c"), Err(Error::NoSpace));
    // A file counts until its last handle goes.
    let a = vfs.resolve(&root, "a").unwrap();
    vfs.remove(&root, "a").unwrap();
    assert_eq!(info().free_files, 0);
    drop(a);
    assert_eq!(info().free_files, 1);
    write(&vfs, &root, "c", b"x").unwrap();
}

#[test]
fn tmpfs_has_a_size_limit() {
    let vfs = Vfs::new(tmpfs(8192), "tmpfs").unwrap();
    let root = vfs.root();
    let a = vfs.open(&root, "a", create()).unwrap();
    a.node.write(0, &Slice(&[1u8; 8000])).unwrap();
    let b = vfs.open(&root, "b", create()).unwrap();
    assert_eq!(b.node.write(0, &Slice(&[1u8; 1000])), Err(Error::NoSpace));
    a.node.set_size(0).unwrap();
    assert_eq!(b.node.write(0, &Slice(&[1u8; 1000])), Ok(1000));
    drop(a);
    vfs.remove(&root, "a").unwrap();
    assert_eq!(vfs.root().mount.fs.info().free, 1);
}

#[test]
fn paths_stay_beneath_their_directory() {
    let vfs = vfs();
    let root = vfs.root();
    vfs.make_directory(&root, "home").unwrap();
    vfs.make_directory(&root, "home/user").unwrap();
    write(&vfs, &root, "secret", b"key").unwrap();
    let home = vfs.open(&root, "home/user", OpenFlags::default()).unwrap();
    write(&vfs, &home, "notes", b"hi").unwrap();
    assert_eq!(vfs.stat(&home, "notes").unwrap().size, 2);
    assert_eq!(vfs.stat(&home, "./notes/").unwrap().size, 2);
    assert_eq!(vfs.stat(&home, "../user/notes").err(), Some(Error::Outside));
    assert_eq!(vfs.stat(&home, "..").err(), Some(Error::Outside));
    assert_eq!(vfs.stat(&home, "a/../..").err(), Some(Error::NotFound));
    assert_eq!(vfs.open(&home, "../../secret", OpenFlags::default()).err(), Some(Error::Outside));
    assert_eq!(vfs.stat(&home, "").unwrap().kind, Kind::Directory);
    // From the root, `..` inside the path is fine.
    assert_eq!(vfs.stat(&root, "home/user/../user/notes").unwrap().size, 2);
}

fn link_text(vfs: &Vfs, base: &Location, path: &str) -> Result<String> {
    let mut out = [0u8; 64];
    let n = vfs.read_link(base, path, &mut SliceMut(&mut out[..]))?;
    Ok(String::from(core::str::from_utf8(&out[..n]).unwrap()))
}

#[test]
fn symbolic_links_are_followed() {
    let vfs = vfs();
    let root = vfs.root();
    vfs.make_directory(&root, "a").unwrap();
    vfs.make_directory(&root, "a/b").unwrap();
    write(&vfs, &root, "a/b/file", b"data").unwrap();
    // Relative targets go on from the link's directory.
    vfs.make_link(&root, "rel", "a/b").unwrap();
    vfs.make_link(&root, "a/up", "../a/b/file").unwrap();
    assert_eq!(vfs.stat(&root, "rel/file").unwrap().size, 4);
    assert_eq!(vfs.stat(&root, "a/up").unwrap().size, 4);
    assert_eq!(vfs.stat(&root, "rel").unwrap().kind, Kind::Directory);
    let link = vfs.stat_link(&root, "rel").unwrap();
    assert_eq!((link.kind, link.size), (Kind::Symlink, 3));
    assert_eq!(link_text(&vfs, &root, "rel").unwrap(), "a/b");
    assert_eq!(link_text(&vfs, &root, "a/b/file").err(), Some(Error::NotLink));
    assert!(names(&vfs, &root).contains(&(String::from("rel"), Kind::Symlink)));
    // A `/` after the last name follows it.
    assert_eq!(vfs.resolve_link(&root, "rel/").unwrap().node.kind(), Kind::Directory);
    // Absolute targets start again from where the walk did.
    vfs.make_link(&root, "abs", "/a/b/file").unwrap();
    assert_eq!(read_all(&vfs.open(&root, "abs", OpenFlags::default()).unwrap().node), b"data");
    let a = vfs.resolve(&root, "a").unwrap();
    vfs.make_link(&a, "inside", "/b/file").unwrap();
    assert_eq!(vfs.stat(&a, "inside").unwrap().size, 4);
    assert_eq!(vfs.stat(&root, "a/inside").err(), Some(Error::NotFound));
    // `..` at the root is the root, as `/..` is `/`.
    assert_eq!(vfs.stat(&root, "..").unwrap().kind, Kind::Directory);
    assert_eq!(vfs.stat(&root, "../a/b/file").unwrap().size, 4);
    assert_eq!(vfs.stat(&a, "..").err(), Some(Error::Outside));
    // Not following the last name.
    let no_follow = OpenFlags { no_follow: true, ..OpenFlags::default() };
    assert_eq!(vfs.open(&root, "abs", no_follow).err(), Some(Error::Loop));
    assert!(vfs.open(&root, "rel/file", no_follow).is_ok());
    // Dangling links and loops.
    vfs.make_link(&root, "dangling", "nowhere/x").unwrap();
    assert_eq!(vfs.stat(&root, "dangling").err(), Some(Error::NotFound));
    assert_eq!(vfs.stat_link(&root, "dangling").unwrap().kind, Kind::Symlink);
    vfs.make_link(&root, "loop1", "loop2").unwrap();
    vfs.make_link(&root, "loop2", "./loop1").unwrap();
    assert_eq!(vfs.stat(&root, "loop1").err(), Some(Error::Loop));
    assert_eq!(vfs.stat_link(&root, "loop1").unwrap().kind, Kind::Symlink);
    // Where paths really lead.
    let real = |base: &Location, path: &str| {
        let mut out = [0u8; 64];
        let n = vfs.real_path(base, path, &mut SliceMut(&mut out[..])).unwrap();
        String::from(core::str::from_utf8(&out[..n]).unwrap())
    };
    assert_eq!(real(&root, "rel/file"), "a/b/file");
    assert_eq!(real(&root, "a/up/"), "a/b/file");
    assert_eq!(real(&root, "rel/.."), "a");
    assert_eq!(real(&root, "abs/../../b/.."), "a");
    assert_eq!(real(&root, ".."), ".");
    assert_eq!(real(&a, "inside"), "b/file");
    let mut short = [0u8; 3];
    assert_eq!(vfs.real_path(&root, "rel", &mut SliceMut(&mut short[..])), Ok(3));
    assert_eq!(&short, b"a/b");
    let mut shorter = [0u8; 2];
    assert_eq!(vfs.real_path(&root, "rel/file", &mut SliceMut(&mut shorter[..])), Ok(8));
    assert_eq!(&shorter, b"a/");
    // Making links.
    assert_eq!(vfs.make_link(&root, "rel", "x").err(), Some(Error::Exists));
    assert_eq!(vfs.make_link(&root, "empty", "").err(), Some(Error::BadName));
    assert_eq!(vfs.make_link(&root, "a/b/file/x", "y").err(), Some(Error::NotDirectory));
}

#[test]
fn at_most_forty_links_are_followed() {
    let vfs = vfs();
    let root = vfs.root();
    write(&vfs, &root, "file", b"x").unwrap();
    for i in 0..=crate::MAX_LINKS {
        let next =
            if i == crate::MAX_LINKS { String::from("file") } else { std::format!("l{}", i + 1) };
        vfs.make_link(&root, &std::format!("l{i}"), &next).unwrap();
    }
    assert_eq!(vfs.stat(&root, "l1").unwrap().size, 1);
    assert_eq!(vfs.stat(&root, "l0").err(), Some(Error::Loop));
}

#[test]
fn files_are_made_and_names_changed_through_links() {
    let vfs = vfs();
    let root = vfs.root();
    vfs.make_directory(&root, "dir").unwrap();
    // Creating through a link makes the file where it points.
    vfs.make_link(&root, "new", "dir/made").unwrap();
    write(&vfs, &root, "new", b"xy").unwrap();
    assert_eq!(vfs.stat(&root, "dir/made").unwrap().size, 2);
    assert_eq!(vfs.stat_link(&root, "new").unwrap().kind, Kind::Symlink);
    let exclusive = OpenFlags { create: true, exclusive: true, ..OpenFlags::default() };
    assert_eq!(vfs.open(&root, "new", exclusive).err(), Some(Error::Exists));
    let no_follow = OpenFlags { create: true, no_follow: true, ..OpenFlags::default() };
    assert_eq!(vfs.open(&root, "new", no_follow).err(), Some(Error::Loop));
    // Renaming and removing take the link, not what it points to.
    vfs.make_link(&root, "to_dir", "dir").unwrap();
    vfs.rename(&root, "to_dir", &root, "dir2").unwrap();
    assert_eq!(vfs.stat(&root, "dir2/made").unwrap().size, 2);
    vfs.remove(&root, "dir2").unwrap();
    assert_eq!(vfs.stat(&root, "dir").unwrap().kind, Kind::Directory);
    assert_eq!(vfs.stat_link(&root, "dir2").err(), Some(Error::NotFound));
    // A link replaces a file, but not a directory.
    vfs.rename(&root, "new", &root, "dir/made").unwrap();
    assert_eq!(vfs.stat_link(&root, "dir/made").unwrap().kind, Kind::Symlink);
    vfs.make_link(&root, "other", "x").unwrap();
    assert_eq!(vfs.rename(&root, "other", &root, "dir").err(), Some(Error::IsDirectory));
}

#[test]
fn links_count_against_the_limits() {
    let fs = Tmpfs::new(8, 3).unwrap();
    let vfs = Vfs::new(fs.clone(), "tmpfs").unwrap();
    let root = vfs.root();
    assert_eq!(vfs.make_link(&root, "long", "123456789").err(), Some(Error::NoSpace));
    vfs.make_link(&root, "a", "1234").unwrap();
    vfs.make_link(&root, "b", "5678").unwrap();
    assert_eq!(fs.info().free_files, 0);
    assert_eq!(vfs.make_link(&root, "c", "9").err(), Some(Error::NoSpace));
    vfs.remove(&root, "a").unwrap();
    vfs.remove(&root, "b").unwrap();
    assert_eq!(fs.info().free_files, 2);
    vfs.make_link(&root, "c", "12345678").unwrap();
}

#[test]
fn a_tree_of_mounts_goes_at_once() {
    let vfs = vfs();
    let root = vfs.root();
    vfs.make_directory(&root, "fuzz").unwrap();
    vfs.mount(&root, "fuzz", tmpfs(1 << 20), "tmpfs", false).unwrap();
    for dir in ["fuzz/a", "fuzz/b", "fuzz/d"] {
        vfs.make_directory(&root, dir).unwrap();
    }
    vfs.mount(&root, "fuzz/a", tmpfs(1 << 20), "tmpfs", false).unwrap();
    vfs.make_directory(&root, "fuzz/a/c").unwrap();
    vfs.mount(&root, "fuzz/a/c", tmpfs(1 << 20), "tmpfs", false).unwrap();
    vfs.mount(&root, "fuzz/b", tmpfs(1 << 20), "tmpfs", false).unwrap();
    // Stacked on the same directory, too.
    vfs.mount(&root, "fuzz/b", tmpfs(1 << 20), "tmpfs", false).unwrap();
    vfs.make_directory(&root, "other").unwrap();
    vfs.mount(&root, "other", tmpfs(1 << 20), "tmpfs", false).unwrap();
    let count = |vfs: &Vfs| {
        let mut n = 0;
        vfs.mounts(|_| n += 1);
        n
    };
    assert_eq!(count(&vfs), 7);
    // Only the root of a mount will do, and a handle anywhere keeps them.
    assert_eq!(vfs.unmount_tree(&root, "fuzz/d"), Err(Error::NotFound));
    let handle = vfs.open(&root, "fuzz/a/c", OpenFlags::default()).unwrap();
    assert_eq!(vfs.unmount_tree(&root, "fuzz"), Err(Error::InUse));
    assert_eq!(count(&vfs), 7);
    drop(handle);
    vfs.unmount_tree(&root, "fuzz").unwrap();
    assert_eq!(count(&vfs), 2);
    assert!(vfs.stat(&root, "fuzz/a").is_err());
    vfs.remove(&root, "fuzz").unwrap();
    vfs.unmount(&root, "other").unwrap();
}

#[test]
fn mounts_join_filesystems() {
    let vfs = vfs();
    let root = vfs.root();
    vfs.make_directory(&root, "mnt").unwrap();
    vfs.make_directory(&root, "mnt/disk").unwrap();
    write(&vfs, &root, "mnt/disk/hidden", b"under").unwrap();
    let other = tmpfs(1 << 20);
    let mount = vfs.mount(&root, "mnt/disk", other.clone(), "test", false).unwrap();
    assert_eq!(mount.path(), "mnt/disk");
    // Mounting there again stacks on top, and unmounting takes it off.
    vfs.mount(&root, "mnt/disk", tmpfs(4096), "again", false).unwrap();
    assert_eq!(vfs.unmount(&root, "mnt"), Err(Error::NotFound));
    vfs.unmount(&root, "mnt/disk").unwrap();
    // The mounted filesystem covers what was there.
    assert_eq!(vfs.stat(&root, "mnt/disk/hidden").err(), Some(Error::NotFound));
    write(&vfs, &root, "mnt/disk/new", b"on top").unwrap();
    assert_eq!(read_all(&other.root().lookup("new").unwrap()), b"on top");
    // `..` from the mounted root comes back to the directory above.
    write(&vfs, &root, "mnt/top", b"t").unwrap();
    assert_eq!(vfs.stat(&root, "mnt/disk/../top").unwrap().size, 1);
    // Moving between filesystems is refused; removing a mount point too.
    assert_eq!(vfs.rename(&root, "mnt/disk/new", &root, "mnt/new"), Err(Error::CrossMount));
    assert_eq!(vfs.remove(&root, "mnt/disk"), Err(Error::InUse));
    // A handle on it keeps it mounted.
    let handle = vfs.open(&root, "mnt/disk", OpenFlags::default()).unwrap();
    drop(mount);
    assert_eq!(vfs.unmount(&root, "mnt/disk"), Err(Error::InUse));
    drop(handle);
    assert_eq!(vfs.unmount(&root, "mnt"), Err(Error::NotFound));
    vfs.unmount(&root, "mnt/disk").unwrap();
    assert_eq!(
        read_all(&vfs.open(&root, "mnt/disk/hidden", OpenFlags::default()).unwrap().node),
        b"under"
    );
    let mut count = 0;
    vfs.mounts(|_| count += 1);
    assert_eq!(count, 1);
}

/// Many mount points on one filesystem share bits of its filter: each
/// must still be crossed, nothing else must be, and unmounting some must
/// leave the others found.
#[test]
fn mount_points_are_found_through_their_filter() {
    let vfs = vfs();
    let root = vfs.root();
    let dirs: Vec<String> = (0..200).map(|i| std::format!("d{i}")).collect();
    let mounted = |i: usize| i.is_multiple_of(3);
    for (i, dir) in dirs.iter().enumerate() {
        vfs.make_directory(&root, dir).unwrap();
        write(&vfs, &root, &std::format!("{dir}/under"), b"u").unwrap();
        if mounted(i) {
            vfs.mount(&root, dir, tmpfs(1 << 16), "tmpfs", false).unwrap();
        }
    }
    let covered = |vfs: &Vfs, dir: &str| vfs.stat(&root, &std::format!("{dir}/under")).is_err();
    for (i, dir) in dirs.iter().enumerate() {
        assert_eq!(covered(&vfs, dir), mounted(i), "{dir}");
        // The walk with a chain, for `..`, finds the same.
        let around = std::format!("{dir}/../{dir}/under");
        assert_eq!(vfs.stat(&root, &around).is_err(), mounted(i), "{around}");
    }
    // Half the mounts go; the filter must still lead to the rest.
    for (i, dir) in dirs.iter().enumerate() {
        if mounted(i) && i.is_multiple_of(2) {
            vfs.unmount(&root, dir).unwrap();
        }
    }
    for (i, dir) in dirs.iter().enumerate() {
        assert_eq!(covered(&vfs, dir), mounted(i) && i % 2 == 1, "{dir}");
    }
    for (i, dir) in dirs.iter().enumerate() {
        if mounted(i) && i % 2 == 1 {
            assert_eq!(vfs.remove(&root, dir), Err(Error::InUse));
            vfs.unmount(&root, dir).unwrap();
        }
    }
    assert!(dirs.iter().all(|dir| !covered(&vfs, dir)));
}

#[test]
fn renames() {
    let vfs = vfs();
    let root = vfs.root();
    for dir in ["a", "a/b", "c"] {
        vfs.make_directory(&root, dir).unwrap();
    }
    write(&vfs, &root, "a/f", b"one").unwrap();
    write(&vfs, &root, "c/g", b"two").unwrap();
    vfs.rename(&root, "a/f", &root, "c/f").unwrap();
    assert_eq!(names(&vfs, &vfs.resolve(&root, "c").unwrap()).len(), 2);
    // A file replaces a file.
    vfs.rename(&root, "c/f", &root, "c/g").unwrap();
    assert_eq!(read_all(&vfs.open(&root, "c/g", OpenFlags::default()).unwrap().node), b"one");
    // Within one directory.
    vfs.rename(&root, "c/g", &root, "c/h").unwrap();
    assert_eq!(vfs.stat(&root, "c/g").err(), Some(Error::NotFound));
    // A directory cannot move under itself, nor replace a full one.
    assert_eq!(vfs.rename(&root, "a", &root, "a/b/a"), Err(Error::BadName));
    assert_eq!(vfs.rename(&root, "a", &root, "c"), Err(Error::NotEmpty));
    assert_eq!(vfs.rename(&root, "c/h", &root, "a/b"), Err(Error::IsDirectory));
    vfs.rename(&root, "a/b", &root, "c/b").unwrap();
    assert_eq!(vfs.stat(&root, "c/b").unwrap().kind, Kind::Directory);
    // The moved directory's `..` is its new parent: a loop check still
    // sees it.
    assert_eq!(vfs.rename(&root, "c", &root, "c/b/c"), Err(Error::BadName));
}

/// Lists `dir` one entry at a time, calling `each` with every name.
fn list_one_by_one(vfs: &Vfs, dir: &Location, mut each: impl FnMut(&str)) -> Vec<String> {
    let mut seen = Vec::new();
    let mut cursor = 0;
    loop {
        let mut got = None;
        vfs.list(dir, cursor, &mut |next, name, _| {
            got = Some((next, String::from(name)));
            false
        })
        .unwrap();
        match got {
            Some((next, name)) => {
                each(&name);
                seen.push(name);
                cursor = next;
            }
            None => break,
        }
    }
    seen
}

#[test]
fn listing_with_cursors() {
    let vfs = vfs();
    let root = vfs.root();
    for name in ["b", "a", "c", "d"] {
        write(&vfs, &root, name, b"").unwrap();
    }
    // A tmpfs lists its entries in the order they were made.
    assert_eq!(list_one_by_one(&vfs, &root, |_| {}), ["b", "a", "c", "d"]);
    // Renamed in place, an entry keeps its place; replacing one, it takes
    // the place of the one it replaces.
    vfs.rename(&root, "a", &root, "e").unwrap();
    assert_eq!(list_one_by_one(&vfs, &root, |_| {}), ["b", "e", "c", "d"]);
    vfs.rename(&root, "d", &root, "b").unwrap();
    assert_eq!(list_one_by_one(&vfs, &root, |_| {}), ["b", "e", "c"]);
    assert_eq!(vfs.stat(&root, "d").err(), Some(Error::NotFound));
}

#[test]
fn removing_while_listing_skips_nothing() {
    let vfs = vfs();
    let root = vfs.root();
    vfs.make_directory(&root, "dir").unwrap();
    let dir = vfs.resolve(&root, "dir").unwrap();
    let names: Vec<String> = (0..50).map(|i| format!("f{i:02}")).collect();
    for name in names.iter().rev() {
        write(&vfs, &dir, name, b"x").unwrap();
    }
    // As `rm -r` does: each entry is removed as soon as it is listed.
    let seen = list_one_by_one(&vfs, &dir, |name| vfs.remove(&dir, name).unwrap());
    assert_eq!(seen.len(), 50);
    vfs.remove(&root, "dir").unwrap();
}

#[test]
fn generated_files_are_snapshots() {
    use core::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let dir = FixedDir::new(1).unwrap();
    let generate: crate::synthetic::Generator = mem::boxed(|text: &mut crate::Text| {
        use core::fmt::Write;
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        write!(text, "count: {n}\n{}", "x".repeat(5000))
    })
    .unwrap();
    dir.add("counter", Generated::make(2, generate).unwrap()).unwrap();
    dir.add("null", mem::arc(Null(3)).unwrap()).unwrap();
    dir.add("zero", mem::arc(Zero(4)).unwrap()).unwrap();
    let fs = SyntheticFs::make(FsKind::Proc, dir.clone()).unwrap();
    let vfs = Vfs::new(fs, "proc").unwrap();
    let root = vfs.root();
    let file = vfs.open(&root, "counter", OpenFlags::default()).unwrap();
    let mut first = [0u8; 9];
    file.node.read(0, &mut SliceMut(&mut first[..])).unwrap();
    let mut again = [0u8; 9];
    file.node.read(0, &mut SliceMut(&mut again[..])).unwrap();
    assert_eq!(first, again, "one handle sees one snapshot");
    assert_eq!(file.node.stat().size, 5009);
    let other = vfs.open(&root, "counter", OpenFlags::default()).unwrap();
    let mut second = [0u8; 9];
    other.node.read(0, &mut SliceMut(&mut second[..])).unwrap();
    assert_ne!(first, second);
    assert_eq!(vfs.open(&root, "new", create()).err(), Some(Error::ReadOnly));
    let zero = vfs.open(&root, "zero", OpenFlags::default()).unwrap();
    let mut out = [9u8; 300];
    assert_eq!(zero.node.read(0, &mut SliceMut(&mut out[..])).unwrap(), 300);
    assert!(out.iter().all(|&b| b == 0));
    let null = vfs.open(&root, "null", OpenFlags::default()).unwrap();
    assert_eq!(null.node.read(0, &mut SliceMut(&mut out[..])).unwrap(), 0);
    assert_eq!(null.node.write(0, &Slice(&out)).unwrap(), 300);
    assert_eq!(names(&vfs, &root).len(), 3);
}

#[test]
fn the_boot_archive_as_a_filesystem() {
    let mut writer = radix_bootfs::write::Writer::new();
    writer.directory("system", 0o755, 1).unwrap();
    writer.directory("system/bin", 0o755, 1).unwrap();
    writer.symlink("system/bin/sh", "spark", 1).unwrap();
    writer.file("system/bin/spark", 0o755, 2, b"\x7fELF spark").unwrap();
    writer.directory("system/etc", 0o755, 1).unwrap();
    writer.file("system/etc/motd", 0o644, 3, b"hello\n").unwrap();
    let bytes: &'static [u8] = writer.finish().leak();
    let fs = BootFs::new(bytes).unwrap();
    let vfs = Vfs::new(fs, "bootfs").unwrap();
    let root = vfs.root();
    let spark = vfs.open(&root, "system/bin/spark", OpenFlags::default()).unwrap();
    assert_eq!(read_all(&spark.node), b"\x7fELF spark");
    assert_eq!(spark.node.stat().modified, 2_000_000_000);
    let system = vfs.resolve(&root, "system").unwrap();
    assert_eq!(
        names(&vfs, &system),
        [(String::from("bin"), Kind::Directory), (String::from("etc"), Kind::Directory)]
    );
    let bin = vfs.resolve(&root, "system/bin").unwrap();
    assert_eq!(names(&vfs, &bin)[0], (String::from("sh"), Kind::Symlink));
    let sh = vfs.open(&root, "system/bin/sh", OpenFlags::default()).unwrap();
    assert_eq!(read_all(&sh.node), b"\x7fELF spark");
    let link = vfs.stat_link(&root, "system/bin/sh").unwrap();
    assert_eq!((link.kind, link.size), (Kind::Symlink, 5));
    assert_eq!(link_text(&vfs, &root, "system/bin/sh").unwrap(), "spark");
    assert_eq!(vfs.make_link(&root, "system/bin/ls", "x").err(), Some(Error::ReadOnly));
    assert_eq!(vfs.open(&root, "system/new", create()).err(), Some(Error::ReadOnly));
    assert_eq!(spark.node.write(0, &Slice(b"x")), Err(Error::ReadOnly));
    // Each file is one node, so a tmpfs mounts over a directory of it.
    vfs.mount(&root, "system/etc", tmpfs(1 << 20), "tmpfs", false).unwrap();
    write(&vfs, &root, "system/etc/hostname", b"dukovo").unwrap();
    assert_eq!(vfs.stat(&root, "system/etc/motd").err(), Some(Error::NotFound));
}

/// A disk for one file: page-sized blocks in a map, with the cache's
/// calls recorded.
#[derive(Default)]
struct FakeDisk {
    pages: StdMutex<BTreeMap<u64, Vec<u8>>>,
    reserved: StdMutex<u64>,
    limit: u64,
}

impl Backing for FakeDisk {
    fn persistent(&self) -> bool {
        true
    }

    fn read_page(&self, index: u64, page: &mut radix_block::Page, len: usize) -> Result<()> {
        let pages = self.pages.lock().unwrap();
        match pages.get(&index) {
            Some(data) => page.bytes_mut()[..len].copy_from_slice(&data[..len]),
            // Never written: the disk holds garbage there.
            None => page.bytes_mut()[..len].fill(0xee),
        }
        Ok(())
    }

    fn write_page(&self, index: u64, page: &radix_block::Page, len: usize) -> Result<()> {
        let mut data = std::vec![0xeeu8; 4096];
        data[..len].copy_from_slice(&page.bytes()[..len]);
        self.pages.lock().unwrap().insert(index, data);
        Ok(())
    }

    fn reserve(&self, size: u64) -> Result<()> {
        if size > self.limit {
            return Err(Error::NoSpace);
        }
        let mut reserved = self.reserved.lock().unwrap();
        *reserved = (*reserved).max(size);
        Ok(())
    }

    fn zero(&self, from: u64, to: u64) -> Result<()> {
        let mut pages = self.pages.lock().unwrap();
        for index in from / 4096..to / 4096 {
            pages.insert(index, std::vec![0; 4096]);
        }
        Ok(())
    }

    fn truncate(&self, size: u64) -> Result<()> {
        *self.reserved.lock().unwrap() = size;
        // Data past the end is garbage on the disk afterwards.
        let mut pages = self.pages.lock().unwrap();
        pages.retain(|&index, _| index * 4096 < size);
        Ok(())
    }
}

#[derive(Clone, Debug)]
enum FileOp {
    Write(u64, Vec<u8>),
    Read(u64, usize),
    SetSize(u64),
    Sync,
    Shrink,
}

proptest! {
    /// A file's cache over a disk whose unwritten blocks hold garbage
    /// matches a plain vector, whatever is synced or dropped when.
    #[test]
    fn file_cache_matches_a_model(ops in proptest::collection::vec(prop_oneof![
        4 => (0u64..40_000, proptest::collection::vec(any::<u8>(), 0..9000))
            .prop_map(|(o, d)| FileOp::Write(o, d)),
        3 => (0u64..45_000, 0usize..10_000).prop_map(|(o, l)| FileOp::Read(o, l)),
        2 => (0u64..45_000).prop_map(FileOp::SetSize),
        1 => Just(FileOp::Sync),
        1 => Just(FileOp::Shrink),
    ], 1..50)) {
        let disk = FakeDisk { limit: 50_000, ..FakeDisk::default() };
        let cache = FileCache::new(0, 0);
        let mut model: Vec<u8> = Vec::new();
        for op in ops {
            match op {
                FileOp::Write(offset, data) => {
                    let end = offset as usize + data.len();
                    let result = cache.write(offset, &Slice(&data), &disk);
                    if data.is_empty() {
                        prop_assert_eq!(result, Ok(0));
                    } else if end as u64 > disk.limit {
                        prop_assert_eq!(result, Err(Error::NoSpace));
                    } else {
                        prop_assert_eq!(result, Ok(data.len()));
                        if model.len() < end {
                            model.resize(end, 0);
                        }
                        model[offset as usize..end].copy_from_slice(&data);
                    }
                }
                FileOp::Read(offset, len) => {
                    let mut out = std::vec![0u8; len];
                    let n = cache.read(offset, &mut SliceMut(&mut out[..]), &disk).unwrap();
                    let start = (offset as usize).min(model.len());
                    let end = (offset as usize + len).min(model.len());
                    prop_assert_eq!(&out[..n], &model[start..end]);
                }
                FileOp::SetSize(size) => {
                    let result = cache.set_size(size, &disk);
                    if size > disk.limit {
                        prop_assert_eq!(result, Err(Error::NoSpace));
                    } else {
                        prop_assert_eq!(result, Ok(()));
                        model.resize(size as usize, 0);
                    }
                }
                FileOp::Sync => {
                    cache.sync(&disk).unwrap();
                    prop_assert!(!cache.is_dirty());
                }
                FileOp::Shrink => {
                    cache.shrink(usize::MAX, &disk);
                }
            }
            prop_assert_eq!(cache.size(), model.len() as u64);
        }
        // After a sync and dropping every page, the disk alone has it.
        cache.sync(&disk).unwrap();
        cache.shrink(usize::MAX, &disk);
        prop_assert_eq!(cache.pages(), 0);
        let mut out = std::vec![0u8; model.len()];
        let n = cache.read(0, &mut SliceMut(&mut out[..]), &disk).unwrap();
        prop_assert_eq!(n, model.len());
        prop_assert_eq!(out, model);
    }
}

#[derive(Clone, Debug)]
enum TreeOp {
    MakeDir(u8, u8),
    Write(u8, u8, u8),
    Remove(u8, u8),
    Rename(u8, u8, u8, u8),
}

proptest! {
    /// tmpfs against a model of directories and files: every operation
    /// succeeds or fails as the model says, and the trees match.
    #[test]
    fn tmpfs_matches_a_model(ops in proptest::collection::vec(prop_oneof![
        (0u8..4, 0u8..4).prop_map(|(d, n)| TreeOp::MakeDir(d, n)),
        (0u8..4, 0u8..4, any::<u8>()).prop_map(|(d, n, b)| TreeOp::Write(d, n, b)),
        (0u8..4, 0u8..4).prop_map(|(d, n)| TreeOp::Remove(d, n)),
        (0u8..4, 0u8..4, 0u8..4, 0u8..4).prop_map(|(a, b, c, d)| TreeOp::Rename(a, b, c, d)),
    ], 1..80)) {
        // Directories "", "d1", "d2", "d3" may exist at the top; names
        // n0..n3 in each. The model maps a path to None (a directory) or
        // Some(byte) (a one-byte file).
        let vfs = vfs();
        let root = vfs.root();
        let mut model: BTreeMap<String, Option<u8>> = BTreeMap::new();
        let dir = |d: u8| if d == 0 { String::new() } else { std::format!("n{d}") };
        let path = |d: u8, n: u8| if d == 0 { std::format!("n{n}") } else { std::format!("n{d}/n{n}") };
        let parent_ok = |model: &BTreeMap<String, Option<u8>>, d: u8| {
            d == 0 || model.get(&dir(d)) == Some(&None)
        };
        for op in ops {
            match op {
                TreeOp::MakeDir(d, n) => {
                    let p = path(d, n);
                    let expected = if parent_ok(&model, d) && !model.contains_key(&p) { Ok(()) } else { Err(()) };
                    let got = vfs.make_directory(&root, &p).map_err(|_| ());
                    prop_assert_eq!(got, expected);
                    if got.is_ok() {
                        model.insert(p, None);
                    }
                }
                TreeOp::Write(d, n, byte) => {
                    let p = path(d, n);
                    let ok = parent_ok(&model, d) && model.get(&p) != Some(&None);
                    let got = write(&vfs, &root, &p, &[byte]).is_ok();
                    prop_assert_eq!(got, ok);
                    if ok {
                        model.insert(p, Some(byte));
                    }
                }
                TreeOp::Remove(d, n) => {
                    let p = path(d, n);
                    let prefix = std::format!("{p}/");
                    let ok = model.contains_key(&p) && !model.keys().any(|k| k.starts_with(&prefix));
                    let got = vfs.remove(&root, &p).is_ok();
                    prop_assert_eq!(got, ok);
                    if ok {
                        model.remove(&p);
                    }
                }
                TreeOp::Rename(a, b, c, d) => {
                    let (from, to) = (path(a, b), path(c, d));
                    let got = vfs.rename(&root, &from, &root, &to);
                    let from_kind = model.get(&from).copied();
                    let to_kind = model.get(&to).copied();
                    let from_prefix = std::format!("{from}/");
                    let to_prefix = std::format!("{to}/");
                    let expected_ok = match from_kind {
                        None => false,
                        Some(kind) => {
                            parent_ok(&model, c)
                                && !to.starts_with(&from_prefix)
                                && match (kind, to_kind) {
                                    (_, None) if !model.contains_key(&to) => true,
                                    (Some(_), Some(Some(_))) => true,
                                    (None, Some(None)) => {
                                        from == to || !model.keys().any(|k| k.starts_with(&to_prefix))
                                    }
                                    _ => from == to,
                                }
                        }
                    };
                    prop_assert_eq!(got.is_ok(), expected_ok, "rename {} -> {}: {:?}", from, to, got);
                    if got.is_ok() && from != to {
                        let moved: Vec<(String, Option<u8>)> = model
                            .iter()
                            .filter(|(k, _)| **k == from || k.starts_with(&from_prefix))
                            .map(|(k, v)| (std::format!("{to}{}", &k[from.len()..]), *v))
                            .collect();
                        model.retain(|k, _| *k != from && !k.starts_with(&from_prefix) && *k != to);
                        for (k, v) in moved {
                            model.insert(k, v);
                        }
                    }
                }
            }
        }
        for (p, kind) in &model {
            let stat = vfs.stat(&root, p).unwrap();
            match kind {
                None => prop_assert_eq!(stat.kind, Kind::Directory),
                Some(byte) => {
                    let file = vfs.open(&root, p, OpenFlags::default()).unwrap();
                    prop_assert_eq!(read_all(&file.node), std::vec![*byte]);
                }
            }
        }
    }
}

// File locks: the list a lock's owner leaves, and what stands in whose way.

mod lock_lists {
    use std::vec::Vec;

    use proptest::prelude::*;

    use crate::locks::{Kind, Lock, Owner, Space, blocker, set};

    const ONE: Owner = Owner::Process(1);
    const TWO: Owner = Owner::Process(2);

    fn lock(owner: Owner, kind: Kind, start: u64, end: u64) -> Lock {
        Lock { owner, space: Space::Range, kind, start, end }
    }

    /// The list sorted, to compare with what is expected.
    fn sorted(locks: &[Lock]) -> Vec<Lock> {
        let mut locks = locks.to_vec();
        locks.sort_by_key(|lock| (lock.start, lock.end));
        locks
    }

    fn take(locks: &mut Vec<Lock>, owner: Owner, kind: Option<Kind>, start: u64, end: u64) -> bool {
        set(locks, owner, Space::Range, kind, (start, end)).unwrap()
    }

    #[test]
    fn locks_of_a_kind_join_and_others_split() {
        let mut locks = Vec::new();
        take(&mut locks, ONE, Some(Kind::Exclusive), 0, 9);
        // Bordering, of the same kind: one lock.
        take(&mut locks, ONE, Some(Kind::Exclusive), 10, 19);
        assert_eq!(locks, [lock(ONE, Kind::Exclusive, 0, 19)]);
        // A shared lock in the middle leaves the exclusive ends.
        assert!(take(&mut locks, ONE, Some(Kind::Shared), 5, 9));
        assert_eq!(
            sorted(&locks),
            [
                lock(ONE, Kind::Exclusive, 0, 4),
                lock(ONE, Kind::Shared, 5, 9),
                lock(ONE, Kind::Exclusive, 10, 19)
            ]
        );
        // Turned exclusive again, it joins both: nothing was freed.
        assert!(!take(&mut locks, ONE, Some(Kind::Exclusive), 5, 9));
        assert_eq!(locks, [lock(ONE, Kind::Exclusive, 0, 19)]);
        // Letting go of the middle splits it.
        assert!(take(&mut locks, ONE, None, 8, 11));
        assert_eq!(
            sorted(&locks),
            [lock(ONE, Kind::Exclusive, 0, 7), lock(ONE, Kind::Exclusive, 12, 19)]
        );
        // To the end of the file, however far it grows.
        take(&mut locks, ONE, Some(Kind::Shared), 15, u64::MAX);
        assert_eq!(
            sorted(&locks),
            [
                lock(ONE, Kind::Exclusive, 0, 7),
                lock(ONE, Kind::Exclusive, 12, 14),
                lock(ONE, Kind::Shared, 15, u64::MAX)
            ]
        );
        assert!(take(&mut locks, ONE, None, 0, u64::MAX));
        assert!(locks.is_empty());
    }

    #[test]
    fn owners_and_spaces_stay_apart() {
        let mut locks = Vec::new();
        take(&mut locks, ONE, Some(Kind::Shared), 0, 9);
        take(&mut locks, TWO, Some(Kind::Shared), 5, 14);
        // Another owner's lock neither joins nor splits.
        assert_eq!(locks.len(), 2);
        let wants = |owner, kind, start, end| blocker(&locks, &lock(owner, kind, start, end));
        assert_eq!(wants(ONE, Kind::Shared, 0, 20), None);
        assert_eq!(wants(ONE, Kind::Exclusive, 0, 4), None);
        assert_eq!(wants(ONE, Kind::Exclusive, 0, 5), Some(lock(TWO, Kind::Shared, 5, 14)));
        assert_eq!(wants(TWO, Kind::Exclusive, 10, 20), None);
        assert_eq!(wants(Owner::Process(3), Kind::Exclusive, 14, 14), Some(locks[1]));
        // A lock on the whole file meets only those.
        let whole = Lock { space: Space::Whole, ..lock(TWO, Kind::Exclusive, 0, u64::MAX) };
        assert_eq!(blocker(&locks, &whole), None);
        set(&mut locks, Owner::Description(8), Space::Whole, Some(Kind::Exclusive), (0, u64::MAX))
            .unwrap();
        assert_eq!(blocker(&locks, &whole), Some(locks[2]));
        let ranged = lock(Owner::Description(8), Kind::Exclusive, 20, 30);
        assert_eq!(blocker(&locks, &ranged), None);
    }

    /// A step of the model test: an owner (of three), a kind (or none) and
    /// a range of the first 32 bytes.
    fn step() -> impl Strategy<Value = (u64, Option<bool>, u64, u64)> {
        (0..3u64, prop::option::of(any::<bool>()), 0..32u64, 0..32u64)
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(512))]

        /// Against a model that keeps each owner's kind for each byte:
        /// the list covers the same bytes with the same kinds, one owner's
        /// locks never overlap or border others of their kind, and a lock
        /// is in another's way just when the model says.
        #[test]
        fn lists_match_a_model(steps in prop::collection::vec(step(), 1..40)) {
            let mut locks = Vec::new();
            let mut model = [[None::<Kind>; 32]; 3];
            for (owner, kind, a, b) in steps {
                let (start, end) = (a.min(b), a.max(b));
                let kind = kind.map(|exclusive| if exclusive { Kind::Exclusive } else { Kind::Shared });
                let process = Owner::Process(owner);
                let wanted = lock(process, kind.unwrap_or(Kind::Shared), start, end);
                let in_way = (0..3).filter(|&other| other != owner).any(|other| {
                    (start..=end).any(|byte| {
                        let held = model[other as usize][byte as usize];
                        held.is_some() && kind.is_some()
                            && (held == Some(Kind::Exclusive) || kind == Some(Kind::Exclusive))
                    })
                });
                prop_assert_eq!(kind.is_some() && blocker(&locks, &wanted).is_some(), in_way);
                take(&mut locks, process, kind, start, end);
                for byte in start..=end {
                    model[owner as usize][byte as usize] = kind;
                }
                for owner in 0..3u64 {
                    let mine: Vec<&Lock> =
                        locks.iter().filter(|l| l.owner == Owner::Process(owner)).collect();
                    for byte in 0..32u64 {
                        let covering: Vec<&&Lock> =
                            mine.iter().filter(|l| l.start <= byte && byte <= l.end).collect();
                        prop_assert!(covering.len() <= 1);
                        prop_assert_eq!(covering.first().map(|l| l.kind), model[owner as usize][byte as usize]);
                    }
                    for (i, x) in mine.iter().enumerate() {
                        for y in &mine[i + 1..] {
                            let borders = x.end + 1 == y.start || y.end + 1 == x.start;
                            prop_assert!(!(borders && x.kind == y.kind), "{:?} and {:?} did not join", x, y);
                        }
                    }
                }
            }
        }
    }
}
