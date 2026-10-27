// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! The VFS on the host: paths, resolution beneath a directory, mounts,
//! tmpfs, the page cache against a model disk, synthetic files and the
//! boot archive.

use std::collections::BTreeMap;
use std::string::String;
use std::sync::Mutex as StdMutex;
use std::sync::atomic::{AtomicUsize, Ordering};
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
    assert_eq!(path::parse("é/ü".as_bytes()), Ok("é/ü"));
    assert_eq!(path::parse(b"\xc3"), Err(Error::BadName));
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
    // A directory is neither made again nor cut.
    assert_eq!(vfs.open(&root, "etc", create()).err(), Some(Error::IsDirectory));
    assert_eq!(vfs.open(&root, "etc", trunc).err(), Some(Error::IsDirectory));
    assert!(vfs.open(&root, "etc", dir).is_ok());
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
fn sockets_have_names() {
    let fs = Tmpfs::new(1 << 20, 3).unwrap();
    let vfs = Vfs::new(fs.clone(), "tmpfs").unwrap();
    let root = vfs.root();
    let binding: crate::SocketBinding = mem::arc(7u32).unwrap();
    vfs.make_socket(&root, "s", binding.clone()).unwrap();
    assert_eq!(vfs.make_socket(&root, "s", binding).err(), Some(Error::Exists));
    let named = vfs.resolve(&root, "s").unwrap();
    assert_eq!(named.node.kind(), Kind::Socket);
    assert_eq!(named.node.socket_binding().unwrap().downcast_ref::<u32>(), Some(&7));
    assert!(root.node.socket_binding().is_none());
    // A name is connected through, never opened.
    assert_eq!(vfs.open(&root, "s", OpenFlags::default()).err(), Some(Error::NoDevice));
    assert_eq!(names(&vfs, &root), [(String::from("s"), Kind::Socket)]);
    // It counts against the limit, and moves and goes as a file does.
    write(&vfs, &root, "f", b"x").unwrap();
    assert_eq!(fs.info().free_files, 0);
    vfs.rename(&root, "s", &root, "f").unwrap();
    assert_eq!(vfs.stat(&root, "f").unwrap().kind, Kind::Socket);
    assert_eq!(fs.info().free_files, 1);
    vfs.make_directory(&root, "d").unwrap();
    assert_eq!(vfs.rename(&root, "f", &root, "d").err(), Some(Error::IsDirectory));
    vfs.remove(&root, "f").unwrap();
    drop(named);
    assert_eq!(fs.info().free_files, 1);
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
    writer.directory("system/settings", 0o755, 1).unwrap();
    writer.file("system/settings/motd", 0o644, 3, b"hello\n").unwrap();
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
        [(String::from("bin"), Kind::Directory), (String::from("settings"), Kind::Directory)]
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
    vfs.mount(&root, "system/settings", tmpfs(1 << 20), "tmpfs", false).unwrap();
    write(&vfs, &root, "system/settings/hostname", b"dukovo").unwrap();
    assert_eq!(vfs.stat(&root, "system/settings/motd").err(), Some(Error::NotFound));
}

#[test]
fn boot_archive_lookups_find_every_entry() {
    let mut writer = radix_bootfs::write::Writer::new();
    writer.directory("bin", 0o755, 1).unwrap();
    let programs: Vec<String> = (0..300).map(|i| format!("p{i:03}")).collect();
    for name in &programs {
        writer.file(&format!("bin/{name}"), 0o755, 1, name.as_bytes()).unwrap();
    }
    // After everything in "bin", in the archive's order of names.
    writer.directory("bin-extra", 0o755, 1).unwrap();
    writer.file("bin-extra/x", 0o644, 1, b"x").unwrap();
    writer.directory("etc", 0o755, 1).unwrap();
    writer.directory("etc/a", 0o755, 1).unwrap();
    writer.file("etc/a/b", 0o644, 1, b"b").unwrap();
    writer.file("etc/c", 0o644, 1, b"c").unwrap();
    let bytes: &'static [u8] = writer.finish().leak();
    let vfs = Vfs::new(BootFs::new(bytes).unwrap(), "bootfs").unwrap();
    let root = vfs.root();
    for name in &programs {
        let file = vfs.open(&root, &format!("bin/{name}"), OpenFlags::default()).unwrap();
        assert_eq!(read_all(&file.node), name.as_bytes());
    }
    for missing in ["bin/p300", "bin/a", "bin/zzz", "bin-extra/y", "etc/a/c", "nothing"] {
        assert_eq!(vfs.stat(&root, missing).err(), Some(Error::NotFound), "{missing}");
    }
    assert_eq!(vfs.stat(&root, "bin/p000/x").err(), Some(Error::NotDirectory));
    let open = |path| read_all(&vfs.open(&root, path, OpenFlags::default()).unwrap().node);
    assert_eq!(open("bin-extra/x"), b"x");
    assert_eq!(open("etc/a/b"), b"b");
    assert_eq!(open("etc/c"), b"c");
    let listed = |path| names(&vfs, &vfs.resolve(&root, path).unwrap());
    let in_root: Vec<String> = names(&vfs, &root).into_iter().map(|(name, _)| name).collect();
    assert_eq!(in_root, ["bin", "bin-extra", "etc"]);
    let in_bin: Vec<String> = listed("bin").into_iter().map(|(name, _)| name).collect();
    assert_eq!(in_bin, programs);
    assert_eq!(
        listed("etc"),
        [(String::from("a"), Kind::Directory), (String::from("c"), Kind::File)]
    );
}

/// A disk for one file: page-sized blocks in a map, with the cache's
/// calls recorded.
#[derive(Default)]
struct FakeDisk {
    pages: StdMutex<BTreeMap<u64, Vec<u8>>>,
    reserved: StdMutex<u64>,
    limit: u64,
    /// The largest size a file may have, if not 0.
    max: u64,
    /// Whether zeroing fails, as a disk error would make it.
    zero_fails: bool,
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
        if self.zero_fails {
            return Err(Error::Io);
        }
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

    fn max_size(&self) -> u64 {
        if self.max == 0 { u64::MAX / 2 } else { self.max }
    }
}

#[test]
fn a_write_past_the_largest_size_writes_what_fits() {
    // As on Linux: a short write up to the limit, then EFBIG at it.
    let disk = FakeDisk { limit: 1 << 20, max: 10_000, ..FakeDisk::default() };
    let cache = FileCache::new(0, 0);
    assert_eq!(cache.write(9_000, &Slice(&[7; 3000]), &disk), Ok(1000));
    assert_eq!(cache.size(), 10_000);
    assert_eq!(*disk.reserved.lock().unwrap(), 10_000);
    assert_eq!(cache.write(10_000, &Slice(b"x"), &disk), Err(Error::TooBig));
    assert_eq!(cache.write(APPEND, &Slice(b"x"), &disk), Err(Error::TooBig));
    assert_eq!(cache.write(20_000, &Slice(b""), &disk), Ok(0));
    assert_eq!(cache.write(u64::MAX - 1, &Slice(b"xy"), &disk), Err(Error::TooBig));
}

#[test]
fn a_gap_that_cannot_be_zeroed_gives_its_room_back() {
    let disk = FakeDisk { limit: 1 << 20, zero_fails: true, ..FakeDisk::default() };
    let cache = FileCache::new(0, 0);
    assert_eq!(cache.write(100_000, &Slice(b"x"), &disk), Err(Error::Io));
    assert_eq!(*disk.reserved.lock().unwrap(), 0);
    assert_eq!(cache.size(), 0);
}

/// A disk for one file whose page `i` holds the byte `i` throughout, read
/// in runs of at most `most` pages, with the runs asked for recorded.
struct RunDisk {
    most: usize,
    runs: StdMutex<Vec<(u64, usize)>>,
}

impl Backing for RunDisk {
    fn persistent(&self) -> bool {
        true
    }

    fn read_page(&self, index: u64, page: &mut radix_block::Page, len: usize) -> Result<()> {
        page.bytes_mut()[..len].fill(index as u8);
        Ok(())
    }

    fn read_run(
        &self,
        index: u64,
        pages: &mut [mem::CachePage],
        size: u64,
        _more: &mut dyn FnMut(u64, &[u8]) -> bool,
    ) -> Result<usize> {
        self.runs.lock().unwrap().push((index, pages.len()));
        let n = pages.len().min(self.most);
        for (at, page) in (index..).zip(&mut pages[..n]) {
            let len = size.saturating_sub(at * 4096).min(4096) as usize;
            page.bytes_mut().fill(0);
            self.read_page(at, page, len)?;
        }
        Ok(n)
    }

    fn run_pages(&self) -> usize {
        usize::MAX
    }

    fn reserve(&self, _: u64) -> Result<()> {
        Ok(())
    }

    fn truncate(&self, _: u64) -> Result<()> {
        Ok(())
    }
}

/// Reads `len` bytes at `offset` through `cache` and checks them against
/// [`RunDisk`]'s pattern; returns the runs the read asked for.
fn read_runs(cache: &FileCache, disk: &RunDisk, offset: u64, len: usize) -> Vec<(u64, usize)> {
    let mut out = std::vec![0xaau8; len];
    let read = cache.read(offset, &mut SliceMut(&mut out[..]), disk).unwrap();
    let end = (offset + len as u64).min(cache.size());
    assert_eq!(read as u64, end.saturating_sub(offset));
    for (at, byte) in (offset..end).zip(&out) {
        assert_eq!(*byte, (at / 4096) as u8, "byte {at}");
    }
    core::mem::take(&mut *disk.runs.lock().unwrap())
}

#[test]
fn reads_go_to_the_disk_in_runs_and_read_ahead_when_sequential() {
    use crate::cache::{READ_AHEAD, READ_RUN};
    let disk = RunDisk { most: usize::MAX, runs: StdMutex::new(Vec::new()) };
    // 1200 pages, the last one short.
    let size = 1200 * 4096 - 100;
    let cache = FileCache::new(size, 0);
    let piece = 128 << 10;
    // The start of the file: the read's 32 pages and three times as many.
    assert_eq!(read_runs(&cache, &disk, 0, piece), [(0, 4 * 32)]);
    // Cached already.
    assert_eq!(read_runs(&cache, &disk, piece as u64, piece), []);
    // On from 128 cached pages: the read's 32 and 128 more.
    assert_eq!(read_runs(&cache, &disk, 4 * piece as u64, piece), [(128, 32 + 128)]);
    // A small read at the start reads READ_AHEAD more.
    let small = FileCache::new(size, 0);
    assert_eq!(read_runs(&small, &disk, 0, 100), [(0, 1 + READ_AHEAD)]);
    // Elsewhere, no read ahead: the page read alone.
    assert_eq!(read_runs(&cache, &disk, 300 * 4096 + 7, 10), [(300, 1)]);
    // A run stops at a cached page, and goes on after it with read ahead.
    assert_eq!(read_runs(&cache, &disk, 290 * 4096, 20 * 4096), [(290, 10), (301, 9 + READ_AHEAD)]);
    // At most READ_RUN pages a run.
    assert_eq!(read_runs(&cache, &disk, 400 * 4096, 2 << 20), [(400, READ_RUN), (656, READ_RUN)]);
    // None past the end.
    assert_eq!(read_runs(&cache, &disk, 1100 * 4096, 1 << 20), [(1100, 100)]);
}

#[test]
fn a_run_read_short_keeps_what_was_read() {
    let disk = RunDisk { most: 3, runs: StdMutex::new(Vec::new()) };
    let cache = FileCache::new(20 * 4096, 0);
    assert_eq!(read_runs(&cache, &disk, 0, 8 * 4096), [(0, 20), (3, 17), (6, 14)]);
    // Pages 0 to 8 are cached; reads past them go on from there.
    assert_eq!(read_runs(&cache, &disk, 0, 9 * 4096), []);
}

/// Which pages of a program's buffer of `len` bytes are there, `skew`
/// bytes into its first page: page `i` if bit `i % 64` of `mask` is set.
fn pages_made(len: usize, skew: usize, mask: u64) -> Vec<bool> {
    (0..(skew + len).div_ceil(4096).max(1)).map(|i| mask & (1 << (i % 64)) != 0).collect()
}

/// A buffer of a program's memory to write from, starting `skew` bytes
/// into a page, whose missing pages a copy stops at, having copied what
/// lay before, as a copy that faults partway does; `fault_in` makes them,
/// unless the buffer is bad.
struct LazyIn<'a> {
    data: &'a [u8],
    skew: usize,
    made: std::cell::RefCell<Vec<bool>>,
    bad: bool,
}

impl crate::In for LazyIn<'_> {
    fn len(&self) -> usize {
        self.data.len()
    }

    fn get(&self, _: usize, _: &mut [u8]) -> Result<()> {
        panic!("a copy under the cache's lock waited for a page");
    }

    fn get_now(&self, at: usize, out: &mut [u8]) -> Result<bool> {
        let made = self.made.borrow();
        for (i, byte) in out.iter_mut().enumerate() {
            if !made[(self.skew + at + i) / 4096] {
                return Ok(false);
            }
            *byte = self.data[at + i];
        }
        Ok(true)
    }

    fn fault_in(&self, at: usize, len: usize) -> Result<()> {
        if self.bad {
            return Err(Error::BadAddress);
        }
        let (start, end) = (self.skew + at, self.skew + at + len);
        self.made.borrow_mut()[start / 4096..end.div_ceil(4096)].fill(true);
        Ok(())
    }
}

/// [`LazyIn`] the other way: a buffer of a program's memory to read into.
struct LazyOut {
    bytes: Vec<u8>,
    skew: usize,
    made: Vec<bool>,
}

impl crate::Out for LazyOut {
    fn len(&self) -> usize {
        self.bytes.len()
    }

    fn put(&mut self, _: usize, _: &[u8]) -> Result<()> {
        panic!("a copy under the cache's lock waited for a page");
    }

    fn put_now(&mut self, at: usize, bytes: &[u8]) -> Result<bool> {
        for (i, &byte) in bytes.iter().enumerate() {
            if !self.made[(self.skew + at + i) / 4096] {
                return Ok(false);
            }
            self.bytes[at + i] = byte;
        }
        Ok(true)
    }

    fn fault_in(&mut self, at: usize, len: usize) -> Result<()> {
        let (start, end) = (self.skew + at, self.skew + at + len);
        self.made[start / 4096..end.div_ceil(4096)].fill(true);
        Ok(())
    }
}

#[derive(Clone, Debug)]
enum FileOp {
    /// Data at an offset, from a buffer with the pages a mask says,
    /// starting where in a page its low 12 bits say.
    Write(u64, Vec<u8>, u64),
    Read(u64, usize, u64),
    SetSize(u64),
    Sync,
    Shrink,
}

proptest! {
    /// A file's cache over a disk whose unwritten blocks hold garbage
    /// matches a plain vector, whatever is synced or dropped when, and
    /// whichever pages of the programs' buffers are not there yet.
    #[test]
    fn file_cache_matches_a_model(ops in proptest::collection::vec(prop_oneof![
        4 => (0u64..40_000, proptest::collection::vec(any::<u8>(), 0..9000), any::<u64>())
            .prop_map(|(o, d, m)| FileOp::Write(o, d, m)),
        3 => (0u64..45_000, 0usize..10_000, any::<u64>()).prop_map(|(o, l, m)| FileOp::Read(o, l, m)),
        2 => (0u64..45_000).prop_map(FileOp::SetSize),
        1 => Just(FileOp::Sync),
        1 => Just(FileOp::Shrink),
    ], 1..50)) {
        let disk = FakeDisk { limit: 50_000, ..FakeDisk::default() };
        let cache = FileCache::new(0, 0);
        let mut model: Vec<u8> = Vec::new();
        for op in ops {
            match op {
                FileOp::Write(offset, data, mask) => {
                    let end = offset as usize + data.len();
                    let skew = (mask & 0xfff) as usize;
                    let made = std::cell::RefCell::new(pages_made(data.len(), skew, mask >> 12));
                    let buffer = LazyIn { data: &data, skew, made, bad: false };
                    let result = cache.write(offset, &buffer, &disk);
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
                FileOp::Read(offset, len, mask) => {
                    let skew = (mask & 0xfff) as usize;
                    let made = pages_made(len, skew, mask >> 12);
                    let mut out = LazyOut { bytes: std::vec![0u8; len], skew, made };
                    let n = cache.read(offset, &mut out, &disk).unwrap();
                    let start = (offset as usize).min(model.len());
                    let end = (offset as usize + len).min(model.len());
                    prop_assert_eq!(&out.bytes[..n], &model[start..end]);
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

/// A write from a bad buffer, one whose pages cannot be made, keeps what
/// it wrote before it, makes up none of the file's other bytes, cached or
/// on the disk, and leaves zeros past the file's end.
#[test]
fn a_write_from_a_bad_buffer_makes_up_nothing() {
    let disk = FakeDisk { limit: 1 << 20, ..FakeDisk::default() };
    let cache = FileCache::new(0, 0);
    let old: Vec<u8> = (0..3 * 4096 - 50).map(|i| (i % 251) as u8).collect();
    assert_eq!(cache.write(0, &Slice(&old), &disk), Ok(old.len()));
    cache.sync(&disk).unwrap();
    cache.shrink(usize::MAX, &disk);
    let bad = |skew, made: Vec<bool>, data| LazyIn {
        data,
        skew,
        made: std::cell::RefCell::new(made),
        bad: true,
    };
    // Whole pages over the file, the second not there: the first is
    // written, and the second, not read from the disk, is not made up.
    let new = std::vec![7u8; 2 * 4096 + 200];
    assert_eq!(cache.write(0, &bad(0, std::vec![true, false, false], &new), &disk), Ok(4096));
    // Over the file's end, from a buffer whose second page is not there:
    // the copy stops 50 bytes in, past the end, and those bytes go.
    let at = 2 * 4096 + 4000;
    let result = cache.write(at, &bad(4096 - 50, std::vec![true, false, false], &new), &disk);
    assert_eq!(result, Err(Error::BadAddress));
    assert_eq!(cache.size(), old.len() as u64);
    let mut expected = old.clone();
    expected[..4096].fill(7);
    // What the copy did get to inside the file is there.
    expected[at as usize..].fill(7);
    let mut out = std::vec![0u8; 4 * 4096];
    let n = cache.read(0, &mut SliceMut(&mut out[..]), &disk).unwrap();
    assert_eq!(&out[..n], &expected[..]);
    // Growing the file shows zeros past the old end, in the cache and on
    // the disk.
    cache.set_size(old.len() as u64 + 50, &disk).unwrap();
    let n = cache.read(0, &mut SliceMut(&mut out[..]), &disk).unwrap();
    assert_eq!(&out[old.len()..n], &[0u8; 50][..]);
    cache.sync(&disk).unwrap();
    cache.shrink(usize::MAX, &disk);
    let n = cache.read(0, &mut SliceMut(&mut out[..]), &disk).unwrap();
    assert_eq!(&out[..old.len()], &expected[..]);
    assert_eq!(&out[old.len()..n], &[0u8; 50][..]);
    // Without a disk the cache's pages are the file, so the bytes the copy
    // left past the end are what growing it would show, unless zeroed.
    let memory = Memory;
    let cache = FileCache::new(0, 0);
    assert_eq!(cache.write(0, &Slice(&old), &memory), Ok(old.len()));
    let result = cache.write(at, &bad(4096 - 50, std::vec![true, false, false], &new), &memory);
    assert_eq!(result, Err(Error::BadAddress));
    cache.set_size(old.len() as u64 + 50, &memory).unwrap();
    let n = cache.read(0, &mut SliceMut(&mut out[..]), &memory).unwrap();
    assert_eq!(&out[..at as usize], &old[..at as usize]);
    assert_eq!(&out[old.len()..n], &[0u8; 50][..]);
}

/// A file with no disk below its cache, as tmpfs's are.
struct Memory;

impl Backing for Memory {
    fn persistent(&self) -> bool {
        false
    }

    fn reserve(&self, _: u64) -> Result<()> {
        Ok(())
    }

    fn truncate(&self, _: u64) -> Result<()> {
        Ok(())
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

/// A disk in memory behind a block device node, filled with a pattern:
/// byte `i` is `i % 251`.
fn raw_disk(sectors: u64, max_segments: usize) -> (std::sync::Arc<RawDisk>, NodeRef) {
    lending_disk(sectors, max_segments, None, 512)
}

/// [`raw_disk`], reaching `memory` straight if there is one, in logical
/// blocks of `block` bytes.
fn lending_disk(
    sectors: u64,
    max_segments: usize,
    memory: Option<std::sync::Arc<ProgramMemory>>,
    block: u32,
) -> (std::sync::Arc<RawDisk>, NodeRef) {
    let ram = radix_block::ram::RamDisk::with_blocks(sectors, false, block);
    let pattern: Vec<u8> = (0..sectors * 512).map(|i| (i % 251) as u8).collect();
    ram.load(0, &pattern).unwrap();
    let disk = std::sync::Arc::new(RawDisk {
        ram,
        max_segments,
        memory,
        lent: AtomicUsize::new(0),
        fail_lent_after: AtomicUsize::new(usize::MAX),
    });
    let device: radix_block::DeviceRef = disk.clone();
    (disk, crate::synthetic::BlockNode::make(7, device).unwrap())
}

/// A RAM disk that takes at most `max_segments` segments a request, and
/// reaches `memory` straight, if it has one.
struct RawDisk {
    ram: radix_block::ram::RamDisk,
    max_segments: usize,
    memory: Option<std::sync::Arc<ProgramMemory>>,
    /// How many requests it took to or from lent memory.
    lent: AtomicUsize,
    /// How many more of them it carries out before the rest fail.
    fail_lent_after: AtomicUsize,
}

impl RawDisk {
    /// The lent memory, and whether the next request to it fails.
    fn lent_memory(&self) -> core::result::Result<&ProgramMemory, radix_block::Error> {
        let memory = self.memory.as_deref().ok_or(radix_block::Error::Unsupported)?;
        let left = self.fail_lent_after.load(Ordering::Relaxed);
        if left == 0 {
            return Err(radix_block::Error::Io);
        }
        if left != usize::MAX {
            self.fail_lent_after.store(left - 1, Ordering::Relaxed);
        }
        self.lent.fetch_add(1, Ordering::Relaxed);
        Ok(memory)
    }
}

/// Memory a program lends to [`RawDisk`]: its bytes are at "physical"
/// addresses from [`BASE`], a page's start.
struct ProgramMemory {
    bytes: StdMutex<Vec<u8>>,
}

/// Where [`ProgramMemory`] starts.
const BASE: u64 = 0x10_0000_0000;

/// A program's buffer of `len` bytes, from byte `start` of [`ProgramMemory`]:
/// copied into and out of, or lent.
struct Lending {
    memory: std::sync::Arc<ProgramMemory>,
    start: usize,
    len: usize,
}

impl Lending {
    /// What [`Out::lend`] and [`In::lend`] do: as much as fits in
    /// `MAX_FRAMES` parts of pages, if the address suits `align`.
    fn lend_frames(
        &self,
        at: usize,
        len: usize,
        align: usize,
        with: crate::Borrower<'_>,
    ) -> Option<Result<usize>> {
        assert!(at + len <= self.len);
        let address = BASE + (self.start + at) as u64;
        if !address.is_multiple_of(align as u64) {
            return None;
        }
        let head = 4096 - (address % 4096) as usize;
        let n = len.min(head + (radix_block::MAX_FRAMES - 1) * 4096);
        let mut frames = Vec::new();
        let mut done = 0;
        while done < n {
            let phys = address + done as u64;
            let piece = (4096 - (phys % 4096) as usize).min(n - done);
            frames.push((phys, piece as u32));
            done += piece;
        }
        assert!(frames.len() <= radix_block::MAX_FRAMES);
        Some(with(&frames).map(|()| n))
    }

    fn bytes(&self) -> Vec<u8> {
        self.memory.bytes.lock().unwrap()[self.start..self.start + self.len].to_vec()
    }
}

impl crate::Out for Lending {
    fn len(&self) -> usize {
        self.len
    }

    fn put(&mut self, at: usize, bytes: &[u8]) -> Result<()> {
        let from = self.start + at;
        self.memory.bytes.lock().unwrap()[from..from + bytes.len()].copy_from_slice(bytes);
        Ok(())
    }

    fn lend(
        &mut self,
        at: usize,
        len: usize,
        align: usize,
        with: crate::Borrower<'_>,
    ) -> Option<Result<usize>> {
        self.lend_frames(at, len, align, with)
    }
}

impl crate::In for Lending {
    fn len(&self) -> usize {
        self.len
    }

    fn get(&self, at: usize, out: &mut [u8]) -> Result<()> {
        let from = self.start + at;
        out.copy_from_slice(&self.memory.bytes.lock().unwrap()[from..from + out.len()]);
        Ok(())
    }

    fn lend(
        &self,
        at: usize,
        len: usize,
        align: usize,
        with: crate::Borrower<'_>,
    ) -> Option<Result<usize>> {
        self.lend_frames(at, len, align, with)
    }
}

/// [`ProgramMemory`] of `len` bytes, every one `fill`.
fn program_memory(len: usize, fill: u8) -> std::sync::Arc<ProgramMemory> {
    std::sync::Arc::new(ProgramMemory { bytes: StdMutex::new(std::vec![fill; len]) })
}

impl radix_block::BlockDevice for RawDisk {
    fn info(&self) -> radix_block::Info {
        radix_block::Info { max_segments: self.max_segments, ..self.ram.info() }
    }

    fn read(
        &self,
        sector: u64,
        segments: &mut [radix_block::SegmentMut<'_>],
    ) -> core::result::Result<(), radix_block::Error> {
        assert!(segments.len() <= self.max_segments);
        self.ram.read(sector, segments)
    }

    fn write(
        &self,
        sector: u64,
        segments: &[radix_block::Segment<'_>],
    ) -> core::result::Result<(), radix_block::Error> {
        assert!(segments.len() <= self.max_segments);
        self.ram.write(sector, segments)
    }

    fn flush(&self) -> core::result::Result<(), radix_block::Error> {
        self.ram.flush()
    }

    fn read_frames(
        &self,
        sector: u64,
        frames: &[(u64, u32)],
    ) -> core::result::Result<(), radix_block::Error> {
        let info = self.ram.info();
        radix_block::check_frames(info.sectors, info.unit(), sector, frames)?;
        let memory = self.lent_memory()?;
        let mut bytes = memory.bytes.lock().unwrap();
        let mut at = sector * 512;
        for &(phys, len) in frames {
            let from = (phys - BASE) as usize;
            self.ram.dump(at, &mut bytes[from..from + len as usize]);
            at += u64::from(len);
        }
        Ok(())
    }

    fn write_frames(
        &self,
        sector: u64,
        frames: &[(u64, u32)],
    ) -> core::result::Result<(), radix_block::Error> {
        let info = self.ram.info();
        radix_block::check_frames(info.sectors, info.unit(), sector, frames)?;
        let memory = self.lent_memory()?;
        let bytes = memory.bytes.lock().unwrap();
        let mut at = sector * 512;
        for &(phys, len) in frames {
            let from = (phys - BASE) as usize;
            self.ram.load(at, &bytes[from..from + len as usize])?;
            at += u64::from(len);
        }
        Ok(())
    }
}

#[test]
fn whole_sectors_go_straight_to_lent_memory() {
    const SECTORS: u64 = 4096;
    let expected =
        |at: usize, len: usize| -> Vec<u8> { (at..at + len).map(|i| (i % 251) as u8).collect() };
    let room = program_memory(3 << 20, 0);
    let (disk, node) = lending_disk(SECTORS, 16, Some(room.clone()), 512);
    // (offset, length, where the buffer starts, requests to lent memory):
    // a mebibyte a request from a page's start, less from a sector's,
    // none for a buffer or an offset off a sector's boundary, and a tail
    // of part of a sector copied after the whole ones.
    for (at, len, start, lent) in [
        (0, 2 << 20, 0, 2),
        (512, 1 << 20, 512, 2),
        (4096, 8192, 100, 0),
        (100, 8192, 0, 0),
        (1024, 5000, 4096, 1),
    ] {
        disk.lent.store(0, Ordering::Relaxed);
        room.bytes.lock().unwrap().fill(0);
        let mut out = Lending { memory: room.clone(), start, len };
        assert_eq!(node.read(at as u64, &mut out).unwrap(), len, "at {at}");
        assert!(out.bytes() == expected(at, len), "at {at}, {len} bytes from {start}");
        assert_eq!(disk.lent.load(Ordering::Relaxed), lent, "at {at}, {len} bytes from {start}");
    }
    // Writes the same way, and the disk holds them.
    let mut model: Vec<u8> = (0..SECTORS as usize * 512).map(|i| (i % 251) as u8).collect();
    for (at, len, start, byte, lent) in
        [(0, (1 << 20) + 4096, 0, 0xa1, 2), (3072, 70_000, 1536, 0xa2, 1), (777, 4096, 0, 0xa3, 0)]
    {
        disk.lent.store(0, Ordering::Relaxed);
        room.bytes.lock().unwrap()[start..start + len].fill(byte);
        let data = Lending { memory: room.clone(), start, len };
        assert_eq!(node.write(at as u64, &data).unwrap(), len, "at {at}");
        model[at..at + len].fill(byte);
        assert_eq!(disk.lent.load(Ordering::Relaxed), lent, "at {at}, {len} bytes from {start}");
    }
    let mut back = std::vec![0u8; model.len()];
    disk.ram.dump(0, &mut back);
    assert!(back == model, "the disk is not what was written");
    // An error ends a transfer short after the requests that worked, or
    // fails it.
    disk.fail_lent_after.store(1, Ordering::Relaxed);
    let mut out = Lending { memory: room.clone(), start: 0, len: 2 << 20 };
    assert_eq!(node.read(0, &mut out).unwrap(), 1 << 20);
    assert_eq!(node.read(0, &mut out), Err(Error::Io));
    assert_eq!(node.write(0, &Lending { memory: room, start: 0, len: 4096 }), Err(Error::Io));
}

#[test]
fn whole_4k_blocks_go_straight_to_lent_memory() {
    let expected =
        |at: usize, len: usize| -> Vec<u8> { (at..at + len).map(|i| (i % 251) as u8).collect() };
    let room = program_memory(3 << 20, 0);
    let (disk, node) = lending_disk(4096, 16, Some(room.clone()), 4096);
    // On a disk with 4 KiB sectors, what is off a block's boundary, on
    // the disk or in memory, is copied, as Linux's `O_DIRECT` refuses it.
    for (at, len, start, lent) in [
        (0, 2 << 20, 0, 2),
        (512, 1 << 20, 512, 0),
        (4096, 8192, 512, 0),
        (4096, 8192, 4096, 1),
        (8192, 5000, 0, 1),
        (100, 3, 0, 0),
    ] {
        disk.lent.store(0, Ordering::Relaxed);
        room.bytes.lock().unwrap().fill(0);
        let mut out = Lending { memory: room.clone(), start, len };
        assert_eq!(node.read(at as u64, &mut out).unwrap(), len, "at {at}");
        assert!(out.bytes() == expected(at, len), "at {at}, {len} bytes from {start}");
        assert_eq!(disk.lent.load(Ordering::Relaxed), lent, "at {at}, {len} bytes from {start}");
    }
    // A write of part of a block keeps the rest of it.
    let mut model: Vec<u8> = (0..4096 * 512).map(|i| (i % 251) as u8).collect();
    for (at, len, byte) in [(3, 5, 0xb1), (4094, 4, 0xb2), (12_288, 4096, 0xb3), (512, 9000, 0xb4)]
    {
        let data = std::vec![byte; len];
        assert_eq!(node.write(at as u64, &Slice(&data)).unwrap(), len);
        model[at..at + len].fill(byte);
    }
    let mut back = std::vec![0u8; model.len()];
    disk.ram.dump(0, &mut back);
    assert!(back == model, "the disk is not what was written");
}

#[test]
fn block_devices_read_raw_at_any_offset() {
    let (_, node) = raw_disk(300, 3);
    let expected =
        |at: usize, len: usize| -> Vec<u8> { (at..at + len).map(|i| (i % 251) as u8).collect() };
    for (at, len) in
        [(0, 512), (1, 1), (511, 2), (1000, 70_000), (4096 * 3 - 7, 9000), (0, 300 * 512)]
    {
        let mut out = std::vec![0u8; len];
        assert_eq!(node.read(at as u64, &mut SliceMut(&mut out)).unwrap(), len, "at {at}");
        assert_eq!(out, expected(at, len), "at {at}, {len} bytes");
    }
    // Short at the end, nothing past it.
    let mut out = [0u8; 100];
    assert_eq!(node.read(300 * 512 - 10, &mut SliceMut(&mut out)).unwrap(), 10);
    assert_eq!(&out[..10], &expected(300 * 512 - 10, 10)[..]);
    assert_eq!(node.read(300 * 512, &mut SliceMut(&mut out)).unwrap(), 0);
    assert_eq!(node.stat().size, 300 * 512);
}

#[test]
fn block_devices_write_raw_and_keep_the_rest_of_partial_sectors() {
    let (disk, node) = raw_disk(300, 2);
    let mut model: Vec<u8> = (0..300 * 512).map(|i| (i % 251) as u8).collect();
    for (at, len, byte) in
        [(3, 5, 0xa1), (510, 4, 0xa2), (512, 512, 0xa3), (4000, 20_000, 0xa4), (8191, 1, 0xa5)]
    {
        let data = std::vec![byte; len];
        assert_eq!(node.write(at as u64, &Slice(&data)).unwrap(), len);
        model[at..at + len].fill(byte);
    }
    let mut back = std::vec![0u8; 300 * 512];
    disk.ram.dump(0, &mut back);
    assert!(back == model, "the disk is not what was written");
    // Cut at the end; nothing at it or past it, nor at APPEND.
    assert_eq!(node.write(300 * 512 - 3, &Slice(&[1; 10])).unwrap(), 3);
    assert_eq!(node.write(300 * 512, &Slice(&[1])), Err(Error::NoSpace));
    assert_eq!(node.write(APPEND, &Slice(&[1])), Err(Error::NoSpace));
    let flushes = disk.ram.flushes();
    node.sync().unwrap();
    assert_eq!(disk.ram.flushes(), flushes + 1);
}

#[test]
fn raw_errors_end_transfers_short() {
    let (disk, node) = raw_disk(300, 1);
    // The second request fails: one page came.
    disk.ram.fail_after(1);
    let mut out = std::vec![0u8; 3 * 4096];
    assert_eq!(node.read(0, &mut SliceMut(&mut out)).unwrap(), 4096);
    disk.ram.fail_after(0);
    assert_eq!(node.read(0, &mut SliceMut(&mut out)), Err(Error::Io));
    assert_eq!(node.write(0, &Slice(&out)), Err(Error::Io));
    let ro = radix_block::ram::RamDisk::new(8, true);
    let ro = crate::synthetic::BlockNode::make(8, std::sync::Arc::new(ro)).unwrap();
    assert_eq!(ro.write(0, &Slice(&[0; 512])), Err(Error::ReadOnly));
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    /// Raw writes and reads of any offset and length, through requests of
    /// any size, match a model of the disk.
    #[test]
    fn raw_transfers_match_a_model(
        max_segments in 1usize..5,
        ops in proptest::collection::vec((0u64..40 * 512, 1usize..20_000, any::<u8>(), any::<bool>()), 1..20),
        four_k in any::<bool>(),
    ) {
        let (disk, node) = lending_disk(40, max_segments, None, if four_k { 4096 } else { 512 });
        let mut model: Vec<u8> = (0..40 * 512).map(|i| (i % 251) as u8).collect();
        for (at, len, byte, write) in ops {
            let end = (at as usize + len).min(model.len());
            if write {
                let data = std::vec![byte; len];
                prop_assert_eq!(node.write(at, &Slice(&data)).unwrap(), end - at as usize);
                model[at as usize..end].fill(byte);
            } else {
                let mut out = std::vec![0u8; len];
                prop_assert_eq!(node.read(at, &mut SliceMut(&mut out)).unwrap(), end - at as usize);
                prop_assert_eq!(&out[..end - at as usize], &model[at as usize..end]);
            }
        }
        let mut back = std::vec![0u8; model.len()];
        disk.ram.dump(0, &mut back);
        prop_assert!(back == model);
    }

    /// The same, with buffers in memory the disk reaches straight, from
    /// any byte of it.
    #[test]
    fn lent_transfers_match_a_model(
        max_segments in 1usize..5,
        ops in proptest::collection::vec(
            (0u64..80, 0usize..512, 1usize..40, 0usize..512, 0usize..4096, any::<u8>(), any::<bool>()),
            1..20,
        ),
        four_k in any::<bool>(),
    ) {
        let room = program_memory(1 << 20, 0);
        let block = if four_k { 4096 } else { 512 };
        let (disk, node) = lending_disk(80, max_segments, Some(room.clone()), block);
        let mut model: Vec<u8> = (0..80 * 512).map(|i| (i % 251) as u8).collect();
        for (sector, skew, sectors, extra, start, byte, write) in ops {
            // Mostly whole sectors, from a sector's boundary or not.
            let at = sector * 512 + if skew < 400 { 0 } else { skew as u64 };
            let len = sectors * 512 + if extra < 400 { 0 } else { extra };
            let end = (at as usize + len).min(model.len());
            if at as usize >= model.len() {
                continue;
            }
            if write {
                room.bytes.lock().unwrap()[start..start + len].fill(byte);
                let data = Lending { memory: room.clone(), start, len };
                prop_assert_eq!(node.write(at, &data).unwrap(), end - at as usize);
                model[at as usize..end].fill(byte);
            } else {
                let mut out = Lending { memory: room.clone(), start, len };
                prop_assert_eq!(node.read(at, &mut out).unwrap(), end - at as usize);
                prop_assert_eq!(&out.bytes()[..end - at as usize], &model[at as usize..end]);
            }
        }
        let mut back = std::vec![0u8; model.len()];
        disk.ram.dump(0, &mut back);
        prop_assert!(back == model);
    }
}
