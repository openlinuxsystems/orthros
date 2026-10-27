// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Host tests of Btrfs files, directories and subvolumes, through the
//! VFS: what Linux's tools put in a filesystem is read back, byte for
//! byte.

use std::collections::{BTreeMap, BTreeSet};
use std::format;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::string::{String, ToString};
use std::vec;
use std::vec::Vec;

use radix_block::DeviceRef;
use radix_vfs::mem::KArc;
use radix_vfs::{Error, FileSystem, Kind, Node, NodeRef, SliceMut, link_target};

use super::{XorShift, disk_of, mkfs, scratch};
use crate::fs::BtrfsFs;
use crate::node::{BtrfsNode, id_of};

/// The filesystem in `image`, mounted.
pub(super) fn mount(image: &[u8]) -> Result<KArc<BtrfsFs>, Error> {
    let device: DeviceRef = disk_of(512, image);
    BtrfsFs::mount(device)
}

/// `len` bytes no compressor shrinks, from `seed`.
pub(super) fn noise(seed: u64, len: usize) -> Vec<u8> {
    let mut random = XorShift(seed | 1);
    (0..len).map(|_| random.next() as u8).collect()
}

/// A directory of what Btrfs keeps in a filesystem: files of every size
/// around the inline and sector limits, a sparse file, links of both
/// kinds, a FIFO, deep and wide directories, and names that are long,
/// not ASCII, or not UTF-8.
fn rich_tree() -> PathBuf {
    let dir = scratch("rich");
    let write = |path: &str, bytes: &[u8]| {
        let path = dir.join(path);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, bytes).unwrap();
    };
    for (n, len) in [0, 1, 100, 2047, 2048, 2049, 4095, 4096, 4097, 12_345, 1 << 20, 3_000_001]
        .into_iter()
        .enumerate()
    {
        write(&format!("size-{len}"), &noise(n as u64, len));
    }
    // Text, which compresses.
    write("text", "Dukovo reads Btrfs.\n".repeat(10_000).as_bytes());
    {
        use std::io::{Seek, SeekFrom, Write};
        let mut sparse = std::fs::File::create(dir.join("sparse")).unwrap();
        sparse.seek(SeekFrom::Start(1 << 20)).unwrap();
        sparse.write_all(b"after a hole").unwrap();
        sparse.seek(SeekFrom::Start(3 << 20)).unwrap();
        sparse.write_all(&noise(99, 5000)).unwrap();
        sparse.set_len(5 << 20).unwrap();
    }
    write("dir/deeper/deepest/file", b"at the bottom");
    write("dir/hard-a", b"one inode, two names");
    std::fs::hard_link(dir.join("dir/hard-a"), dir.join("hard-b")).unwrap();
    std::os::unix::fs::symlink("size-100", dir.join("link")).unwrap();
    std::os::unix::fs::symlink("nowhere/at/all", dir.join("dangling")).unwrap();
    std::os::unix::fs::symlink("x".repeat(1000), dir.join("long-link")).unwrap();
    std::fs::create_dir_all(dir.join("empty-dir")).unwrap();
    for n in 0..400 {
        write(&format!("many/file-{n:03}"), format!("{n}").as_bytes());
    }
    write("naïve-文件", b"not ASCII");
    write(&"n".repeat(255), b"the longest name");
    let bad = std::ffi::OsStr::from_bytes(b"bad-\xff-name");
    std::fs::write(dir.join(bad), b"not UTF-8").unwrap();
    let fifo = Command::new("mkfifo").arg(dir.join("fifo")).status().unwrap();
    assert!(fifo.success());
    dir
}

/// Every byte of a file, read through the VFS in pieces of `piece`.
pub(super) fn contents(node: &dyn Node, piece: usize) -> Result<Vec<u8>, Error> {
    let mut out = Vec::new();
    let mut buf = vec![0; piece];
    loop {
        let n = node.read(out.len() as u64, &mut SliceMut(&mut buf))?;
        if n == 0 {
            return Ok(out);
        }
        out.extend_from_slice(&buf[..n]);
    }
}

/// The entries of a directory, by name.
fn entries(node: &dyn Node) -> Result<BTreeMap<String, Kind>, Error> {
    let mut found = BTreeMap::new();
    let mut last = 0;
    node.list(0, &mut |cursor, name, kind| {
        assert!(cursor > last, "the cursors go back");
        last = cursor;
        found.insert(name.to_string(), kind);
        true
    })?;
    Ok(found)
}

/// What kind of file the host says `meta` is.
fn host_kind(meta: &std::fs::Metadata) -> Kind {
    let kind = meta.file_type();
    if kind.is_symlink() {
        Kind::Symlink
    } else if kind.is_dir() {
        Kind::Directory
    } else if kind.is_file() {
        Kind::File
    } else {
        Kind::Pipe
    }
}

/// Checks that `node` holds what the host directory `host` does, all
/// the way down; the inodes seen, the host's by device and number, and
/// ours by ID, must pair up one to one.
pub(super) fn same_tree(node: &NodeRef, host: &Path, inodes: &mut BTreeMap<(u64, u64), u64>) {
    let ours = entries(&**node).unwrap();
    let theirs: BTreeSet<String> = std::fs::read_dir(host)
        .unwrap()
        .filter_map(|entry| entry.unwrap().file_name().into_string().ok())
        .collect();
    assert_eq!(ours.keys().cloned().collect::<BTreeSet<_>>(), theirs, "in {}", host.display());
    for (name, kind) in ours {
        let path = host.join(&name);
        let meta = std::fs::symlink_metadata(&path).unwrap();
        let child = node.lookup(&name).unwrap();
        let stat = child.stat();
        assert_eq!(kind, host_kind(&meta), "{}", path.display());
        assert_eq!(stat.kind, kind, "{}", path.display());
        let id = *inodes.entry((meta.dev(), meta.ino())).or_insert(stat.id);
        assert_eq!(id, stat.id, "{} is another inode", path.display());
        // `mkfs.btrfs` keeps whole seconds.
        assert_eq!(stat.modified, meta.mtime() as u64 * 1_000_000_000, "{}", path.display());
        match kind {
            Kind::File => {
                let bytes = std::fs::read(&path).unwrap();
                assert_eq!(stat.size, bytes.len() as u64, "{}", path.display());
                for piece in [4096, 1000] {
                    assert!(contents(&*child, piece).unwrap() == bytes, "{}", path.display());
                }
            }
            Kind::Symlink => {
                let target = std::fs::read_link(&path).unwrap();
                assert_eq!(link_target(&*child).unwrap(), target.as_os_str().as_bytes());
            }
            Kind::Directory => same_tree(&child, &path, inodes),
            _ => assert_eq!(child.read(0, &mut SliceMut(&mut [0; 8])), Err(Error::NotSupported)),
        }
    }
    let unique: BTreeSet<u64> = inodes.values().copied().collect();
    assert_eq!(unique.len(), inodes.len(), "two inodes have one ID");
}

#[test]
fn inode_items_read_as_the_format_lays_them_out() {
    let mut item = [0u8; crate::inode::SIZE];
    let put = |item: &mut [u8], at: usize, bytes: &[u8]| {
        item[at..at + bytes.len()].copy_from_slice(bytes);
    };
    put(&mut item, 16, &12_345u64.to_le_bytes());
    put(&mut item, 24, &16_384u64.to_le_bytes());
    put(&mut item, 40, &2u32.to_le_bytes());
    put(&mut item, 52, &0o100_644u32.to_le_bytes());
    put(&mut item, 64, &crate::inode::flags::NODATASUM.to_le_bytes());
    for (n, at) in [112, 124, 136, 148].into_iter().enumerate() {
        put(&mut item, at, &(1_700_000_000u64 + n as u64).to_le_bytes());
        put(&mut item, at + 8, &(123_456_789u32 + n as u32).to_le_bytes());
    }
    let inode = crate::inode::Inode::read(&item).unwrap();
    assert_eq!((inode.size, inode.nbytes, inode.nlink), (12_345, 16_384, 2));
    assert_eq!(inode.kind(), Ok(Kind::File));
    assert!(!inode.has_sums());
    let time = |n: u64| (1_700_000_000 + n) * 1_000_000_000 + 123_456_789 + n;
    assert_eq!([inode.atime, inode.ctime, inode.mtime, inode.otime], [0, 1, 2, 3].map(time));
    // A mode of no known type is damage.
    put(&mut item, 52, &0o770_644u32.to_le_bytes());
    assert_eq!(crate::inode::Inode::read(&item).unwrap().kind(), Err(Error::Corrupt));
    assert_eq!(crate::inode::Inode::read(&item[..159]), Err(Error::Corrupt));
}

#[test]
fn reads_every_file_as_linux_made_it() {
    let dir = rich_tree();
    for args in [
        &["--nodesize", "4096"][..],
        &["--nodesize", "16384", "--csum", "xxhash"],
        &["--nodesize", "65536", "--csum", "sha256", "-m", "dup", "-d", "dup"],
        &["--csum", "blake2", "--mixed"],
    ] {
        let image = mkfs(args, &dir);
        let fs = mount(&image).unwrap();
        assert!(fs.read_only());
        let root = fs.root();
        let mut inodes = BTreeMap::new();
        same_tree(&root, &dir, &mut inodes);
        // The top directory is the top subvolume's, inode 256.
        assert_eq!(root.stat().id, 256);
        assert_eq!(root.stat().kind, Kind::Directory);
        // Asked of the wrong kind of node, each says so; nothing writes.
        let file = root.lookup("size-100").unwrap();
        assert_eq!(file.lookup("x").err(), Some(Error::NotDirectory));
        assert_eq!(root.read(0, &mut SliceMut(&mut [0; 8])), Err(Error::IsDirectory));
        assert_eq!(root.create("new", Kind::File).err(), Some(Error::ReadOnly));
        assert_eq!(root.remove("size-100"), Err(Error::ReadOnly));
        assert_eq!(file.set_size(0), Err(Error::ReadOnly));
        assert_eq!(root.lookup("missing").err(), Some(Error::NotFound));
        // Memory pressure drops what can be read again.
        assert!(fs.shrink(usize::MAX) > 0);
        same_tree(&root, &dir, &mut inodes);
    }
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn reads_subvolumes_and_the_default_one() {
    let dir = scratch("subvols");
    for (path, bytes) in [
        ("top", &b"in the top subvolume"[..]),
        ("vol/a", b"in vol"),
        ("vol/inner/b", b"in a subvolume inside vol"),
        ("other/c", b"in other"),
    ] {
        let path = dir.join(path);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, bytes).unwrap();
    }
    // Subvolumes are directories of the mount, each with inode 256.
    let image =
        mkfs(&["--subvol", "vol", "--subvol", "ro:vol/inner", "--subvol", "rw:other"], &dir);
    let fs = mount(&image).unwrap();
    let mut inodes = BTreeMap::new();
    same_tree(&fs.root(), &dir, &mut inodes);
    let vol = fs.root().lookup("vol").unwrap();
    let inner = vol.lookup("inner").unwrap();
    let other = fs.root().lookup("other").unwrap();
    let node = |node: &NodeRef| node.as_any().downcast_ref::<BtrfsNode>().unwrap().subvol;
    // `mkfs.btrfs` numbers subvolumes from 256 in the order it finds
    // them, which is the order the host's directory lists them in.
    assert_eq!(node(&fs.root()), 5);
    let mut numbers = [node(&vol), node(&inner), node(&other)];
    numbers.sort_unstable();
    assert_eq!(numbers, [256, 257, 258]);
    let ids = [fs.root().stat().id, vol.stat().id, inner.stat().id];
    assert_eq!(ids, [256, id_of(node(&vol), 256), id_of(node(&inner), 256)]);
    // The same subvolume, found twice, is one node.
    assert!(KArc::ptr_eq(&vol, &fs.root().lookup("vol").unwrap()));
    // A default subvolume is what mounts.
    let image = mkfs(&["--subvol", "vol", "--subvol", "default:other"], &dir);
    let fs = mount(&image).unwrap();
    same_tree(&fs.root(), &dir.join("other"), &mut BTreeMap::new());
    std::fs::remove_dir_all(dir).unwrap();
}

/// Where the bytes `pattern` are in `image`.
fn places(image: &[u8], pattern: &[u8]) -> Vec<usize> {
    image
        .windows(pattern.len())
        .enumerate()
        .filter(|(_, w)| *w == pattern)
        .map(|(at, _)| at)
        .collect()
}

#[test]
fn damaged_data_is_read_from_its_other_copy_or_fails() {
    let dir = scratch("damage");
    std::fs::create_dir_all(&dir).unwrap();
    let data = noise(7, 3 * 4096);
    std::fs::write(dir.join("summed"), &data).unwrap();
    std::fs::write(dir.join("unsummed"), noise(8, 3 * 4096)).unwrap();
    let unsummed = noise(8, 3 * 4096);
    for (profile, copies) in [("single", 1), ("dup", 2)] {
        let args = ["-d", profile, "--inode-flags", "nodatasum:unsummed"];
        let mut image = mkfs(&args, &dir);
        let at = places(&image, &data[4096..4096 + 64]);
        assert_eq!(at.len(), copies, "{profile}");
        // The second sector of the first copy.
        image[at[0] + 100] ^= 1;
        let fs = mount(&image).unwrap();
        let file = fs.root().lookup("summed").unwrap();
        if copies == 2 {
            assert!(contents(&*file, 4096).unwrap() == data);
            // Both copies damaged: the read fails, as on Linux.
            image[at[1] + 100] ^= 1;
            let fs = mount(&image).unwrap();
            let file = fs.root().lookup("summed").unwrap();
            assert_eq!(contents(&*file, 4096), Err(Error::Io));
        } else {
            assert_eq!(contents(&*file, 4096), Err(Error::Io));
            // The sectors around it still read.
            let mut first = [0; 4096];
            assert_eq!(file.read(0, &mut SliceMut(&mut first)), Ok(4096));
            assert!(first[..] == data[..4096]);
        }
        // A file without checksums reads what the disk holds, damaged or
        // not.
        let at = places(&image, &unsummed[..64]);
        image[at[0] + 10] ^= 1;
        let fs = mount(&image).unwrap();
        let file = fs.root().lookup("unsummed").unwrap();
        let read = contents(&*file, 4096).unwrap();
        assert_eq!(read.len(), unsummed.len());
        assert_eq!(read[10], unsummed[10] ^ 1, "{profile}");
    }
    std::fs::remove_dir_all(dir).unwrap();
}

/// Reads everything below `node` that damage lets it read, at most
/// `depth` directories down and `budget` entries in all: damage can make
/// a directory its own child, and a file's size anything.
fn read_all(node: &NodeRef, depth: usize, budget: &mut usize) {
    let Ok(listed) = entries(&**node) else { return };
    for name in listed.keys() {
        if *budget == 0 {
            return;
        }
        *budget -= 1;
        let Ok(child) = node.lookup(name) else { continue };
        let stat = child.stat();
        match stat.kind {
            Kind::Directory if depth > 0 => read_all(&child, depth - 1, budget),
            Kind::Symlink => {
                let _ = link_target(&*child);
            }
            Kind::File => {
                let mut buf = vec![0; 3 * 4096];
                let _ = child.read(0, &mut SliceMut(&mut buf));
                let _ = child.read(stat.size.saturating_sub(5000), &mut SliceMut(&mut buf));
            }
            _ => {}
        }
    }
}

#[test]
fn damaged_files_with_good_checksums_never_panic() {
    let dir = scratch("fuzz");
    let write = |path: &str, bytes: &[u8]| {
        let path = dir.join(path);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, bytes).unwrap();
    };
    for (n, len) in [0, 10, 3000, 4096, 9000, 70_000].into_iter().enumerate() {
        write(&format!("file-{len}"), &noise(n as u64, len));
    }
    for n in 0..60 {
        write(&format!("dir/inner/file-{n}"), format!("{n}").as_bytes());
    }
    std::os::unix::fs::symlink("file-10", dir.join("link")).unwrap();
    std::fs::hard_link(dir.join("file-10"), dir.join("dir/hard")).unwrap();
    let image = mkfs(&["-m", "single", "--nodesize", "4096", "--subvol", "dir/inner"], &dir);
    let fsid = &image[crate::superblock::OFFSET as usize + 0x20..][..16];
    // The blocks of the subvolumes' trees and of the checksum tree, by the
    // owner in their headers.
    let blocks: Vec<usize> = (0..image.len() / 4096)
        .map(|n| n * 4096)
        .filter(|&at| {
            let owner = u64::from_le_bytes(image[at + 0x58..at + 0x60].try_into().unwrap());
            &image[at + 0x20..at + 0x30] == fsid && matches!(owner, 5 | 7 | 256)
        })
        .collect();
    assert!(blocks.len() > 4, "{} blocks", blocks.len());
    let disk = disk_of(512, &image);
    let mut random = XorShift(0x2545_f491_4f6c_dd1d);
    let mut failed = 0;
    for _ in 0..300 {
        let block = blocks[random.below(blocks.len())];
        let mut damaged = image[block..block + 4096].to_vec();
        for _ in 0..=random.below(3) {
            // Item data sits at the block's end, so damage goes there as
            // often as to the headers and keys.
            let at = if random.below(2) == 0 {
                101 + random.below(400)
            } else {
                4096 - 1 - random.below(1500)
            };
            damaged[at] = match random.below(3) {
                0 => damaged[at] ^ (1 << random.below(8)),
                1 => 0xff,
                _ => random.next() as u8,
            };
        }
        let sum = crate::csum::CsumType::Crc32c.sum(&damaged[32..]);
        damaged[..32].copy_from_slice(&sum);
        disk.load(block as u64, &damaged).unwrap();
        match BtrfsFs::mount(disk.clone()) {
            Ok(fs) => read_all(&fs.root(), 6, &mut 500),
            Err(_) => failed += 1,
        }
        disk.load(block as u64, &image[block..block + 4096]).unwrap();
    }
    // Some damage stops the mount; the rest is for the reads.
    assert!(failed < 300, "every damaged filesystem was refused");
    let fs = mount(&image).unwrap();
    assert_eq!(contents(&*fs.root().lookup("file-9000").unwrap(), 4096), Ok(noise(4, 9000)));
    std::fs::remove_dir_all(dir).unwrap();
}
