// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Host tests of the UDF driver, against volumes the reference tools
//! make: udftools' `mkudffs` for each kind of partition (physical,
//! sparable, virtual and metadata) and revision, UDFclient's `udfclient`
//! to fill them, and genisoimage's `-udf` for the UDF and ISO 9660
//! bridge of DVDs (`apt install udftools udfclient genisoimage`; `ob
//! doctor` checks for them).

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::string::{String, ToString};
use std::sync::atomic::{AtomicU32, Ordering};
use std::vec::Vec;
use std::{format, vec};

use radix_block::DeviceRef;
use radix_block::ram::RamDisk;
use radix_vfs::mem::{self, KArc};
use radix_vfs::{Error, FileSystem, Kind, NodeRef, SliceMut};

use crate::UdfFs;
use crate::format::{self, NameBuf};

/// A scratch path of its own for each use.
fn scratch(what: &str) -> PathBuf {
    static NEXT: AtomicU32 = AtomicU32::new(0);
    let n = NEXT.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!("radix-udf-{}-{what}-{n}", std::process::id()))
}

/// A tool, wherever its package put it.
fn tool(name: &str, package: &str) -> PathBuf {
    for dir in ["/usr/bin", "/usr/sbin", "/bin", "/sbin", "/usr/local/bin"] {
        let path = Path::new(dir).join(name);
        if path.exists() {
            return path;
        }
    }
    panic!("{name} is missing: apt install {package}");
}

/// An empty volume from `mkudffs` of `blocks` blocks, labelled
/// `RadixUDF`, with `args` (media type first, then revision).
fn mkudffs(args: &[&str], blocks: u32) -> Vec<u8> {
    let path = scratch("mkudffs.img");
    let out = Command::new(tool("mkudffs", "udftools"))
        .arg("--new-file")
        .arg("--label=RadixUDF")
        .args(args)
        .arg(&path)
        .arg(blocks.to_string())
        .output()
        .unwrap();
    assert!(out.status.success(), "mkudffs: {}", String::from_utf8_lossy(&out.stderr));
    let image = std::fs::read(&path).unwrap();
    let _ = std::fs::remove_file(&path);
    image
}

/// `image` with what `udfclient` does when given `commands` in its
/// volume's root, from the host directory `from`.
fn udfclient(image: &[u8], from: &Path, commands: &str) -> Vec<u8> {
    let path = scratch("udfclient.img");
    std::fs::write(&path, image).unwrap();
    let run = |script: &str| {
        let mut child = Command::new(tool("udfclient", "udfclient"))
            .arg("-W")
            .arg(&path)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        use std::io::Write;
        child.stdin.take().unwrap().write_all(script.as_bytes()).unwrap();
        let out = child.wait_with_output().unwrap();
        String::from_utf8_lossy(&out.stdout).into_owned()
    };
    // The volume is a directory of its own in udfclient's root.
    let listed = run("ls\nquit\n");
    let volume = listed
        .lines()
        .skip_while(|line| !line.contains("Directory listing"))
        .nth(1)
        .and_then(|line| line.split_whitespace().last())
        .expect("udfclient lists the volume")
        .to_string();
    run(&format!("cd {volume}\nlcd {}\n{commands}\nquit\n", from.display()));
    let image = std::fs::read(&path).unwrap();
    let _ = std::fs::remove_file(&path);
    image
}

/// Bytes that are not the same from one place to the next.
fn pattern(len: usize, seed: u32) -> Vec<u8> {
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

/// The files the tests put on volumes: (name, size).
const FILES: [(&str, usize); 5] =
    [("empty", 0), ("small", 100), ("odd", 5000), ("big", 300 * 1024 + 7), ("whole", 1 << 20)];

/// A host directory of [`FILES`].
fn files() -> PathBuf {
    let dir = scratch("files");
    std::fs::create_dir_all(&dir).unwrap();
    for (i, (name, size)) in FILES.iter().enumerate() {
        std::fs::write(dir.join(name), pattern(*size, i as u32)).unwrap();
    }
    dir
}

/// The volume in `image`, on a disk of `block`-byte blocks.
fn mount(image: &[u8], block: u32) -> Result<KArc<UdfFs>, Error> {
    let disk = mem::arc(RamDisk::with_blocks(image.len() as u64 / 512, false, block)).unwrap();
    disk.load(0, image).unwrap();
    let device: DeviceRef = disk;
    UdfFs::mount(device, true)
}

/// The node at `path` under `dir`.
fn walk(dir: &NodeRef, path: &str) -> Result<NodeRef, Error> {
    let mut node = dir.clone();
    for name in path.split('/').filter(|n| !n.is_empty()) {
        node = node.lookup(name)?;
    }
    Ok(node)
}

/// A file's bytes, read `chunk` bytes at a time.
fn read_all(node: &NodeRef, chunk: usize) -> Vec<u8> {
    let mut all = Vec::new();
    let mut buf = vec![0u8; chunk];
    loop {
        let n = node.read(all.len() as u64, &mut SliceMut(&mut buf)).unwrap();
        if n == 0 {
            return all;
        }
        all.extend_from_slice(&buf[..n]);
    }
}

/// A directory's entries, sorted by name.
fn names(dir: &NodeRef) -> Vec<(String, Kind)> {
    let mut all = Vec::new();
    dir.list(0, &mut |_, name, kind| {
        all.push((name.to_string(), kind));
        true
    })
    .unwrap();
    all.sort_by(|a, b| a.0.cmp(&b.0));
    all
}

fn label(fs: &UdfFs) -> String {
    let mut label = String::new();
    fs.label(&mut label).unwrap();
    label
}

/// Checks that `dir` holds [`FILES`].
fn check_files(dir: &NodeRef) {
    for (i, (name, size)) in FILES.iter().enumerate() {
        let node = walk(dir, name).unwrap();
        assert_eq!(node.stat().size, *size as u64, "{name}");
        assert_eq!(node.kind(), Kind::File);
        let want = pattern(*size, i as u32);
        assert_eq!(read_all(&node, 4096), want, "{name}");
        // Reads that start and end in the middle of pages.
        assert_eq!(read_all(&node, 1000), want, "{name}");
    }
}

#[test]
fn files_and_directories_of_each_kind_of_entry() {
    let from = files();
    let commands = "mkdir dir\ncd dir\nmput empty small odd big whole\ncd ..\nmput small big";
    // UDF 1.02 with file entries, and 2.01 with extended ones, their
    // allocation descriptors in the entries, short and long.
    for args in [
        &["--media-type=hd", "--udfrev=1.02"][..],
        &["--media-type=hd", "--udfrev=2.01"],
        &["--media-type=hd", "--udfrev=2.01", "--ad=short"],
        &["--media-type=hd", "--udfrev=2.01", "--ad=long", "--noefe"],
    ] {
        let image = udfclient(&mkudffs(args, 20_000), &from, commands);
        let fs = mount(&image, 512).unwrap();
        assert_eq!(fs.kind(), radix_vfs::FsKind::Udf);
        assert!(fs.read_only());
        assert_eq!(label(&fs), "RadixUDF");
        let root = fs.root();
        assert_eq!(
            names(&root),
            [("big", Kind::File), ("dir", Kind::Directory), ("small", Kind::File)]
                .map(|(n, k)| (n.to_string(), k))
                .to_vec(),
            "{args:?}"
        );
        check_files(&walk(&root, "dir").unwrap());
        assert_eq!(read_all(&walk(&root, "big").unwrap(), 4096), pattern(FILES[3].1, 3));
        let small = walk(&root, "small").unwrap();
        assert_eq!(small.access().mode, 0o644, "{args:?}");
        assert!(small.stat().modified > 1_700_000_000_000_000_000, "{args:?}");
        assert_eq!(root.lookup("missing").err(), Some(Error::NotFound));
        assert_eq!(root.lookup("SMALL").err(), Some(Error::NotFound));
        assert_eq!(walk(&root, "small/x").err(), Some(Error::NotDirectory));
        assert_eq!(root.read(0, &mut SliceMut(&mut [0; 8])).err(), Some(Error::IsDirectory));
        // One node for each file while it is in use.
        let (a, b) = (walk(&root, "dir/big").unwrap(), walk(&root, "dir/big").unwrap());
        assert!(core::ptr::addr_eq(&raw const *a, &raw const *b));
        assert_eq!(a.write(0, &radix_vfs::Slice(b"x")).err(), Some(Error::ReadOnly));
    }
    std::fs::remove_dir_all(from).unwrap();
}

#[test]
fn each_kind_of_partition_mounts() {
    // Physical partitions of larger blocks, sparable (CD-RW), virtual
    // (CD-R, with its allocation table) and metadata (UDF 2.50 and
    // later), each with its root directory.
    for (args, block, blocks) in [
        (&["--media-type=hd", "--udfrev=1.50", "--blocksize=2048"][..], 2048, 4000),
        (&["--media-type=hd", "--blocksize=4096"], 4096, 2000),
        (&["--media-type=cdrw"], 2048, 20_000),
        (&["--media-type=dvdrw"], 2048, 20_000),
        (&["--media-type=cdr"], 2048, 1000),
        (&["--media-type=bdr", "--udfrev=2.50"], 2048, 1000),
        (&["--media-type=bdr", "--udfrev=2.60"], 2048, 1000),
        (&["--media-type=dvdr", "--udfrev=2.50"], 2048, 20_000),
    ] {
        let image = mkudffs(args, blocks);
        // On a disk of 512-byte blocks, as an image on a USB stick, and
        // of its own blocks, as a disc.
        for disk in [512, block] {
            let fs = mount(&image, disk).unwrap_or_else(|err| panic!("{args:?}: {err:?}"));
            assert_eq!(label(&fs), "RadixUDF", "{args:?}");
            assert_eq!(fs.info().block_size, block, "{args:?}");
            assert_eq!(names(&fs.root()), [], "{args:?}");
            assert_eq!(fs.root().access().mode, 0o755, "{args:?}");
        }
    }
}

#[test]
fn a_bridge_disc_is_udf_and_iso_9660() {
    let from = files();
    std::fs::create_dir_all(from.join("Sub Dir")).unwrap();
    std::fs::write(from.join("Sub Dir/Summer Time"), b"summer").unwrap();
    let path = scratch("bridge.iso");
    let made = Command::new(tool("genisoimage", "genisoimage"))
        .args(["-quiet", "-udf", "-R", "-J", "-V", "BRIDGE", "-o"])
        .arg(&path)
        .arg(&from)
        .output()
        .unwrap();
    assert!(made.status.success(), "genisoimage: {}", String::from_utf8_lossy(&made.stderr));
    let image = std::fs::read(&path).unwrap();
    let _ = std::fs::remove_file(&path);
    for block in [512, 2048] {
        let fs = mount(&image, block).unwrap();
        assert_eq!(label(&fs), "BRIDGE");
        let root = fs.root();
        check_files(&root);
        assert_eq!(read_all(&walk(&root, "Sub Dir/Summer Time").unwrap(), 64), b"summer");
        assert_eq!(walk(&root, "Sub Dir").unwrap().kind(), Kind::Directory);
    }
    std::fs::remove_dir_all(from).unwrap();
}

#[test]
fn other_disks_and_writable_mounts_are_refused() {
    assert_eq!(mount(&vec![0u8; 1 << 20], 512).err(), Some(Error::UnknownFilesystem));
    let image = mkudffs(&["--media-type=hd"], 4000);
    let disk = mem::arc(RamDisk::new(image.len() as u64 / 512, false)).unwrap();
    disk.load(0, &image).unwrap();
    let device: DeviceRef = disk;
    assert_eq!(UdfFs::mount(device, false).err(), Some(Error::ReadOnly));
}

#[test]
fn names_timestamps_and_tags_decode() {
    let mut name = NameBuf::default();
    format::cs0(b"\x08caf\xe9", &mut name);
    assert_eq!(name.name(), Some("café"));
    format::cs0(b"\x10\x00a\x00/\x00b", &mut name);
    assert_eq!(name.name(), Some("a_b"));
    format::cs0(b"\x07x", &mut name);
    assert_eq!(name.name(), None);
    // 2026-10-31 12:00:00.5 at UTC+01:00.
    let stamp = [0x3c, 0x10, 0xea, 0x07, 10, 31, 13, 0, 0, 50, 0, 0];
    assert_eq!(format::timestamp(&stamp), 1_793_448_000_500_000_000);
    // A tag whose checksum is off is no tag.
    let mut tag = [0u8; 16];
    tag[0] = 2;
    tag[4] = 2;
    assert_eq!(format::tag(&tag, None), Some(2));
    tag[4] = 3;
    assert_eq!(format::tag(&tag, None), None);
}
