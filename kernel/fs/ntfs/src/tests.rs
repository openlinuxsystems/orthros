// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Host tests of the NTFS reader, against ntfs-3g (`apt install
//! ntfs-3g`; `ob doctor` checks for it): volumes are made by its `mkntfs`
//! with sectors and clusters of each size, files are written into them by
//! its `ntfscp`, and its `ntfscat` reads back what Radix counts; and a
//! volume ntfs-3g filled through FUSE is read ([`NTFS_3G`]), with the
//! links, compressed and sparse files, hard links and names the others
//! cannot make.

use std::collections::BTreeSet;
use std::path::PathBuf;
use std::process::Command;
use std::string::String;
use std::sync::atomic::{AtomicU32, Ordering};
use std::vec::Vec;
use std::{format, vec};

use proptest::prelude::*;
use radix_block::DeviceRef;
use radix_block::ram::RamDisk;
use radix_vfs::mem::{self, KArc};
use radix_vfs::{Error, FileSystem, Kind, NodeRef, SliceMut};

use crate::NtfsFs;
use crate::record::{Record, fix_up};
use crate::runs::{HOLE, Runs};
use crate::{index, lznt1};

/// A tool of ntfs-3g (or of dosfstools or exfatprogs).
pub(crate) fn tool(name: &str) -> PathBuf {
    for dir in ["/usr/sbin", "/sbin", "/usr/bin", "/bin", "/usr/local/sbin", "/usr/local/bin"] {
        let path = PathBuf::from(dir).join(name);
        if path.exists() {
            return path;
        }
    }
    panic!("{name} is missing: install ntfs-3g, dosfstools and exfatprogs");
}

/// A file name for a scratch file.
pub(crate) fn scratch(what: &str) -> PathBuf {
    static NEXT: AtomicU32 = AtomicU32::new(0);
    let n = NEXT.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!("radix-ntfs-{}-{what}-{n}", std::process::id()))
}

/// Runs `cmd`, which must succeed; returns its output.
pub(crate) fn run(cmd: &mut Command) -> Vec<u8> {
    let out = cmd.env("LC_ALL", "C.UTF-8").output().unwrap();
    assert!(out.status.success(), "{cmd:?}: {}", String::from_utf8_lossy(&out.stderr));
    out.stdout
}

/// An image file being made: removed when done with.
pub(crate) struct Image(pub(crate) PathBuf);

impl Image {
    /// A fresh volume of `mib` MiB from `mkntfs`, labelled `RADIXTEST`,
    /// with sectors and clusters of `sector` and `cluster` bytes.
    pub(crate) fn mkntfs(mib: u64, sector: u32, cluster: u32) -> Image {
        let path = scratch("mkntfs.img");
        std::fs::File::create(&path).unwrap().set_len(mib << 20).unwrap();
        run(Command::new(tool("mkntfs"))
            .args(["-q", "-F", "-Q", "-L", "RADIXTEST"])
            .args(["-s", &sector.to_string(), "-c", &cluster.to_string()])
            .arg(&path));
        Image(path)
    }

    /// Writes `data` to the file `name` in the root, with `ntfscp`.
    pub(crate) fn copy_in(&self, name: &str, data: &[u8]) {
        let source = scratch("source");
        std::fs::write(&source, data).unwrap();
        run(Command::new(tool("ntfscp"))
            .arg("-q")
            .arg(&self.0)
            .arg(&source)
            .arg(format!("/{name}")));
        let _ = std::fs::remove_file(&source);
    }

    /// The bytes of the system file `name`, as `ntfscat` reads them.
    pub(crate) fn cat(&self, name: &str) -> Vec<u8> {
        run(Command::new(tool("ntfscat")).arg(&self.0).arg(name))
    }

    pub(crate) fn bytes(&self) -> Vec<u8> {
        std::fs::read(&self.0).unwrap()
    }
}

impl Drop for Image {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

/// The kinds of volume the tests use: (MiB, sector, cluster bytes), with
/// clusters smaller than a page, a page, larger, and 4 KiB sectors.
pub(crate) const VOLUMES: [(u64, u32, u32); 4] =
    [(16, 512, 512), (32, 512, 4096), (64, 512, 65536), (32, 4096, 4096)];

/// A volume on a RAM disk of logical blocks of `block` bytes.
struct Volume {
    fs: KArc<NtfsFs>,
}

impl Volume {
    fn mount_in(block: u32, image: &[u8]) -> Result<Volume, Error> {
        let sectors = image.len() as u64 / 512;
        let disk = mem::arc(RamDisk::with_blocks(sectors, false, block)).unwrap();
        disk.load(0, image).unwrap();
        let device: DeviceRef = disk;
        Ok(Volume { fs: NtfsFs::mount(device, true)? })
    }

    fn new_in(block: u32, image: &[u8]) -> Volume {
        Volume::mount_in(block, image).unwrap()
    }

    fn root(&self) -> NodeRef {
        self.fs.root()
    }

    fn label(&self) -> String {
        let mut label = String::new();
        self.fs.label(&mut label).unwrap();
        label
    }
}

/// The node at `path` (names separated by `/`) under `dir`.
pub(crate) fn walk(dir: &NodeRef, path: &str) -> Result<NodeRef, Error> {
    let mut node = dir.clone();
    for name in path.split('/').filter(|n| !n.is_empty()) {
        node = node.lookup(name)?;
    }
    Ok(node)
}

pub(crate) fn read_all(node: &NodeRef) -> Vec<u8> {
    let size = node.stat().size as usize;
    let mut out = vec![0u8; size + 100];
    let n = node.read(0, &mut SliceMut(&mut out)).unwrap();
    assert_eq!(n, size);
    out.truncate(n);
    out
}

pub(crate) fn read_link(node: &NodeRef) -> String {
    let mut out = [0u8; 256];
    let n = node.read_link(&mut SliceMut(&mut out)).unwrap();
    String::from_utf8(out[..n].to_vec()).unwrap()
}

pub(crate) fn names(dir: &NodeRef) -> Vec<(String, Kind)> {
    let mut out = Vec::new();
    dir.list(0, &mut |_, name, kind| {
        out.push((String::from(name), kind));
        true
    })
    .unwrap();
    out.sort_by(|a, b| a.0.cmp(&b.0));
    out
}

/// Bytes that differ from offset to offset and file to file, as `ob
/// fixture ntfs-3g` makes them.
pub(crate) fn pattern(seed: u32, len: usize) -> Vec<u8> {
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

/// Names of every kind: short, with spaces, non-ASCII, astral (two units
/// a character), and as long as NTFS allows, 255 units.
pub(crate) fn test_names() -> Vec<String> {
    let mut names: Vec<String> = [
        "README.TXT",
        "notes.txt",
        "A long file name with spaces.markdown",
        ".profile",
        "\u{dc}n\u{ef}c\u{f6}d\u{e9} \u{2713}.txt",
        "x",
        "emoji \u{1f600}\u{1f680}.bin",
    ]
    .iter()
    .map(|s| String::from(*s))
    .collect();
    names.push(format!("{}.txt", "n".repeat(251)));
    names
}

/// The sizes of the test files: empty, resident in the record, around a
/// page, and over many clusters.
pub(crate) const SIZES: [usize; 8] = [0, 1, 600, 4095, 4096, 4097, 70_000, 300_001];

#[test]
fn a_fresh_volume_mounts() {
    for (mib, sector, cluster) in VOLUMES {
        let image = Image::mkntfs(mib, sector, cluster);
        let volume = Volume::new_in(sector, &image.bytes());
        assert_eq!(volume.label(), "RADIXTEST");
        assert_eq!(volume.fs.version(), (3, 1));
        assert!(!volume.fs.dirty() && !volume.fs.hibernated());
        let geo = volume.fs.geometry();
        assert_eq!((geo.sector_size, geo.cluster_size), (sector, cluster));
        assert_eq!(geo.record_size, if sector == 4096 { 4096 } else { 1024 });
        // The system's files are left out.
        assert_eq!(names(&volume.root()), vec![]);
        assert!(volume.fs.read_only());
        // The free clusters are those `$Bitmap` has clear, as ntfscat
        // reads it.
        let info = volume.fs.info();
        let bitmap = image.cat("$Bitmap");
        let used: u64 = (0..info.blocks)
            .filter(|&c| bitmap[(c / 8) as usize] & (1 << (c % 8)) != 0)
            .count() as u64;
        assert_eq!((info.block_size, info.free), (cluster, info.blocks - used));
        // The volume's last sector holds the copy of its boot sector,
        // after its last cluster.
        let sectors = (mib << 20) / u64::from(sector) - 1;
        assert_eq!(info.blocks, sectors / u64::from(cluster / sector));
    }
}

#[test]
fn what_is_not_ntfs_is_refused() {
    assert_eq!(Volume::mount_in(512, &vec![0u8; 1 << 20]).err(), Some(Error::UnknownFilesystem));
    let mut image = Image::mkntfs(16, 512, 4096).bytes();
    // A volume bigger than its disk.
    image[40..48].copy_from_slice(&(1u64 << 40).to_le_bytes());
    assert_eq!(Volume::mount_in(512, &image).err(), Some(Error::Corrupt));
    // A volume of 512-byte sectors on a disk of 4 KiB ones.
    let image = Image::mkntfs(16, 512, 4096).bytes();
    assert_eq!(Volume::mount_in(4096, &image).err(), Some(Error::NotSupported));
}

#[test]
fn other_filesystems_are_told_apart() {
    // FAT and exFAT take NTFS for none of theirs, and NTFS them for none
    // of its, so that Spark can try each in turn.
    let disk = |image: &[u8]| {
        let disk = mem::arc(RamDisk::with_blocks(image.len() as u64 / 512, false, 512)).unwrap();
        disk.load(0, image).unwrap();
        let device: DeviceRef = disk;
        device
    };
    let ntfs = Image::mkntfs(16, 512, 4096).bytes();
    assert_eq!(radix_fat::FatFs::mount(disk(&ntfs), true).err(), Some(Error::UnknownFilesystem));
    let exfat = radix_exfat::ExfatFs::mount(disk(&ntfs), true).err();
    assert_eq!(exfat, Some(Error::UnknownFilesystem));
    for (mkfs, args) in [("mkfs.fat", &["-C"][..]), ("mkfs.exfat", &[][..])] {
        let path = scratch("other");
        if args.is_empty() {
            std::fs::File::create(&path).unwrap().set_len(16 << 20).unwrap();
            run(Command::new(tool(mkfs)).arg(&path));
        } else {
            run(Command::new(tool(mkfs)).args(args).arg(&path).arg("16384"));
        }
        let image = std::fs::read(&path).unwrap();
        let _ = std::fs::remove_file(&path);
        assert_eq!(
            NtfsFs::mount(disk(&image), true).err(),
            Some(Error::UnknownFilesystem),
            "{mkfs}"
        );
    }
}

#[test]
fn files_ntfscp_wrote_read_back() {
    for (mib, sector, cluster) in VOLUMES {
        let image = Image::mkntfs(mib, sector, cluster);
        let names_used = test_names();
        let mut expected = Vec::new();
        for (i, (name, size)) in names_used.iter().zip(SIZES.iter().cycle()).enumerate() {
            let data = pattern(i as u32 + 1, *size);
            image.copy_in(name, &data);
            expected.push((name.clone(), data));
        }
        let volume = Volume::new_in(sector, &image.bytes());
        let root = volume.root();
        for (name, data) in &expected {
            let node = walk(&root, name).unwrap();
            assert_eq!(&read_all(&node), data, "{name} in clusters of {cluster}");
            assert_eq!(node.stat().kind, Kind::File);
            // Names are found whatever their case, by the up-case table.
            assert_eq!(walk(&root, &name.to_uppercase()).unwrap().stat().id, node.stat().id);
            assert_eq!(walk(&root, &name.to_lowercase()).unwrap().stat().id, node.stat().id);
        }
        let mut want: Vec<_> = expected.iter().map(|(n, _)| (n.clone(), Kind::File)).collect();
        want.sort_by(|a, b| a.0.cmp(&b.0));
        assert_eq!(names(&root), want);
        assert_eq!(root.lookup("missing").err(), Some(Error::NotFound));
        assert_eq!(root.lookup(&"n".repeat(256)).err(), Some(Error::NameTooLong));
        // Reads from the middle, across clusters and pages.
        let (name, data) = &expected[6];
        let node = walk(&root, name).unwrap();
        let mut out = vec![0u8; 10_000];
        assert_eq!(node.read(12_345, &mut SliceMut(&mut out)).unwrap(), 10_000);
        assert_eq!(out, data[12_345..22_345]);
    }
}

#[test]
fn a_big_directory_is_listed_and_searched() {
    let image = Image::mkntfs(32, 512, 4096);
    let mut want = BTreeSet::new();
    for i in 0..1000 {
        let name = format!("file number {i:04} with a long name.txt");
        image.copy_in(&name, format!("{i}\n").as_bytes());
        want.insert(name);
    }
    let volume = Volume::new_in(512, &image.bytes());
    let root = volume.root();
    let listed: BTreeSet<_> = names(&root).into_iter().map(|(n, _)| n).collect();
    assert_eq!(listed, want);
    for (i, name) in want.iter().enumerate().step_by(37) {
        let node = root.lookup(&name.to_uppercase()).unwrap();
        assert_eq!(read_all(&node), format!("{i}\n").into_bytes());
    }
    // A listing taken a few names at a time, each call from the cursor
    // the last one stopped at, gives every name once.
    let mut seen = Vec::new();
    let mut cursor = 0;
    loop {
        let mut taken = 0;
        let mut next = cursor;
        root.list(cursor, &mut |at, name, _| {
            if taken == 7 {
                return false;
            }
            taken += 1;
            seen.push(String::from(name));
            next = at;
            true
        })
        .unwrap();
        if taken == 0 {
            break;
        }
        cursor = next;
    }
    assert_eq!(seen.len(), want.len());
    assert_eq!(seen.into_iter().collect::<BTreeSet<_>>(), want);
}

#[test]
fn a_hibernated_volume_says_so() {
    for (start, hibernated) in [(&b"HIBR"[..], true), (b"hibr", true), (b"wake", false)] {
        let image = Image::mkntfs(16, 512, 4096);
        let mut data = start.to_vec();
        data.resize(4096, 0);
        image.copy_in("hiberfil.sys", &data);
        let volume = Volume::new_in(512, &image.bytes());
        assert_eq!(volume.fs.hibernated(), hibernated);
        // Mounted read-only, it says why it could not be written.
        let why = volume.fs.kept_from_writing();
        assert_eq!(why.is_some_and(|w| w.contains("Fast Startup")), hibernated, "{why:?}");
        let rw = NtfsFs::mount(volume_device(&image.bytes()), false).err();
        assert_eq!(rw, hibernated.then_some(Error::NotSupported));
    }
}

/// `image` on a RAM disk of 512-byte blocks.
fn volume_device(image: &[u8]) -> DeviceRef {
    let disk = mem::arc(RamDisk::with_blocks(image.len() as u64 / 512, false, 512)).unwrap();
    disk.load(0, image).unwrap();
    disk
}

/// A volume of 8 MiB that `mkntfs -c 512 -L NTFS3G` made and ntfs-3g
/// 2022.10.3 filled through FUSE, as `ob fixture ntfs-3g` makes it: the
/// files [`what_ntfs_3g_wrote_reads_back`] checks. Its sectors that are
/// not all zeros: a count of sectors, then each such sector's number and
/// bytes, the numbers as little-endian `u32`s, a number with its top bit
/// set standing for a sector all 0xff, without its bytes.
const NTFS_3G: &[u8] = include_bytes!("fixtures/ntfs-3g.sectors");

/// [`NTFS_3G`]'s volume.
pub(crate) fn ntfs_3g() -> Vec<u8> {
    let word = |at: &[u8]| u32::from_le_bytes(at[..4].try_into().unwrap());
    let mut image = vec![0u8; word(NTFS_3G) as usize * 512];
    let mut at = 4;
    while at < NTFS_3G.len() {
        let n = word(&NTFS_3G[at..]);
        at += 4;
        let sector = (n & !(1 << 31)) as usize * 512;
        if n & (1 << 31) != 0 {
            image[sector..sector + 512].fill(0xff);
        } else {
            image[sector..sector + 512].copy_from_slice(&NTFS_3G[at..at + 512]);
            at += 512;
        }
    }
    image
}

/// The text `ob fixture ntfs-3g` writes compressed.
pub(crate) fn text() -> Vec<u8> {
    (0..4000)
        .flat_map(|i| {
            format!("line {} of a text that compresses well, again and again\n", i % 50)
                .into_bytes()
        })
        .collect()
}

/// 8 KiB of text, 16 KiB of zeros, and the 8 KiB again.
fn mixed() -> Vec<u8> {
    let text: Vec<u8> =
        (0..2000).flat_map(|i| format!("mixed {i}\n").into_bytes()).take(8192).collect();
    [&text[..], &[0u8; 16384][..], &text[..]].concat()
}

#[test]
fn what_ntfs_3g_wrote_reads_back() {
    let volume = Volume::new_in(512, &ntfs_3g());
    assert_eq!(volume.label(), "NTFS3G");
    let root = volume.root();
    let file = |name: &str| (String::from(name), Kind::File);
    let want = vec![
        file("Case.txt"),
        (String::from("Documents"), Kind::Directory),
        file("a long file name.txt"),
        file("case.txt"),
        file("cloud.txt"),
        (String::from("compressed"), Kind::Directory),
        file("empty"),
        (String::from("empty dir"), Kind::Directory),
        (String::from("interix link"), Kind::Symlink),
        (String::from("junction"), Kind::Symlink),
        file("linked.txt"),
        (String::from("links"), Kind::Directory),
        (String::from("many"), Kind::Directory),
        file("pieces.bin"),
        file("read only.txt"),
        file("report link.txt"),
        file("sparse.bin"),
        file("streams.txt"),
        (String::from("windows link"), Kind::Symlink),
        (String::from("wsl link"), Kind::Symlink),
    ];
    // The short name is left out, as are the system's files.
    assert_eq!(names(&root), want);
    let read = |path: &str| read_all(&walk(&root, path).unwrap());
    assert_eq!(read("Documents/report.txt"), b"Written by ntfs-3g.\n");
    assert_eq!(read("Documents/big.bin"), pattern(1, 40_000));
    assert_eq!(read("documents/\u{dc}N\u{cf}C\u{d6}D\u{c9} \u{2713}.TXT"), pattern(3, 1000));
    assert_eq!(read("Documents/emoji \u{1f600}\u{1f680}.bin"), b"emoji\n");
    assert_eq!(read("Documents/Deep/er/still/file.txt"), b"deep\n");
    assert_eq!(read("empty"), b"");
    assert!(names(&walk(&root, "empty dir").unwrap()).is_empty());
    let many = walk(&root, "many").unwrap();
    assert_eq!(names(&many).len(), 40);
    for i in 0..40 {
        assert_eq!(read(&format!("many/file {i:02}.txt")), format!("file {i:02}\n").into_bytes());
    }
    // Hard links: one node, however many names; the linked file's names
    // overflow its record into others, which its attribute list names.
    let report = walk(&root, "Documents/report.txt").unwrap();
    let link = walk(&root, "report link.txt").unwrap();
    assert_eq!((report.stat().id, report.stat().links), (link.stat().id, 2));
    let linked = walk(&root, "linked.txt").unwrap();
    assert_eq!(linked.stat().links, 31);
    let links = walk(&root, "links").unwrap();
    assert_eq!(names(&links).len(), 30);
    for i in 0..30 {
        let name = format!("name number {i:02} of the linked file.txt");
        let node = links.lookup(&name).unwrap();
        assert_eq!((node.stat().id, read_all(&node)), (linked.stat().id, b"linked\n".to_vec()));
    }
    // Links: ntfs-3g's own, Windows' relative one, an absolute one and a
    // junction read as on their own volume, and WSL's.
    for (path, target) in [
        ("interix link", "Documents/report.txt"),
        ("windows link", "Documents/report.txt"),
        ("Documents/Deep/er/absolute link", "../../../Documents/Deep"),
        ("junction", "Documents"),
        ("wsl link", "Documents/report.txt"),
    ] {
        let node = walk(&root, path).unwrap();
        assert_eq!(node.stat().kind, Kind::Symlink, "{path}");
        assert_eq!(read_link(&node), target, "{path}");
        assert_eq!(node.stat().size, target.len() as u64, "{path}");
    }
    // Compressed files: units that compress, units that do not, and units
    // all zeros, which are holes.
    for (path, data) in [
        ("compressed/text.txt", text()),
        ("compressed/random.bin", pattern(7, 12_000)),
        ("compressed/mixed.bin", mixed()),
    ] {
        let node = walk(&root, path).unwrap();
        assert_eq!(read_all(&node), data, "{path}");
    }
    let text_node = walk(&root, "compressed/text.txt").unwrap();
    assert!(text_node.stat().allocated < text_node.stat().size / 4);
    // A sparse file: data, holes, data.
    let sparse = walk(&root, "sparse.bin").unwrap();
    let bytes = read_all(&sparse);
    let mut want = vec![0u8; 6 << 20];
    want[..500].copy_from_slice(&b"start".repeat(100));
    want[3 << 20..(3 << 20) + 600].copy_from_slice(&b"middle".repeat(100));
    want[(6 << 20) - 600..].copy_from_slice(&b"end.".repeat(150));
    assert!(bytes == want, "sparse.bin differs");
    assert!(sparse.stat().allocated < 64 << 10);
    // A file in pieces.
    let pieces: Vec<u8> = (0..40u8).flat_map(|i| [0x40 + i; 512]).collect();
    assert_eq!(read("pieces.bin"), pieces);
    // The stream is not the file's data.
    assert_eq!(read("streams.txt"), b"main\n");
    // A file marked read-only loses its write bits.
    assert_eq!(walk(&root, "read only.txt").unwrap().access().mode, 0o555);
    assert_eq!(walk(&root, "linked.txt").unwrap().access().mode, 0o755);
    // A file whose data is elsewhere does not read as zeros.
    let cloud = walk(&root, "cloud.txt").unwrap();
    assert_eq!(cloud.stat().size, 13);
    let mut out = [0u8; 16];
    assert_eq!(cloud.read(0, &mut SliceMut(&mut out)).err(), Some(Error::NotSupported));
    // The short name finds the file too.
    let long = walk(&root, "a long file name.txt").unwrap();
    assert_eq!(walk(&root, "ALONGF~1.TXT").unwrap().stat().id, long.stat().id);
    // Names the same but for case: each found by its own case, and one of
    // them by another.
    assert_eq!(read("Case.txt"), b"upper\n");
    assert_eq!(read("case.txt"), b"lower\n");
    assert!([&b"upper\n"[..], b"lower\n"].contains(&&read("CASE.TXT")[..]));
}

/// A compressed chunk of `items`: bytes, and copies of (how far back,
/// how many) bytes.
fn chunk(items: &[Result<u8, (usize, usize)>]) -> Vec<u8> {
    let mut data = Vec::new();
    let mut pos = 0usize;
    for group in items.chunks(8) {
        let mut flags = 0u8;
        let mut body = Vec::new();
        for (bit, item) in group.iter().enumerate() {
            match *item {
                Ok(byte) => {
                    body.push(byte);
                    pos += 1;
                }
                Err((back, len)) => {
                    flags |= 1 << bit;
                    let mut shift = 0;
                    let mut p = pos - 1;
                    while p >= 0x10 {
                        p >>= 1;
                        shift += 1;
                    }
                    let token = (((back - 1) as u16) << (12 - shift)) | (len - 3) as u16;
                    body.extend_from_slice(&token.to_le_bytes());
                    pos += len;
                }
            }
        }
        data.push(flags);
        data.extend_from_slice(&body);
    }
    let header = 0xb000 | (data.len() + 2 - 3) as u16;
    [&header.to_le_bytes()[..], &data].concat()
}

#[test]
fn lznt1_uncompresses_bytes_and_copies() {
    // `abc`, then 12 bytes copied from 3 back, overlapping what they
    // write.
    let input = chunk(&[Ok(b'a'), Ok(b'b'), Ok(b'c'), Err((3, 12))]);
    let mut out = vec![0xaa; 8192];
    lznt1::decompress(&input, &mut out).unwrap();
    assert_eq!(&out[..15], b"abcabcabcabcabc");
    assert!(out[15..].iter().all(|&b| b == 0));
    // Past 16 bytes, the length has fewer bits and the distance more.
    let mut items: Vec<_> = (0..40u8).map(Ok).collect();
    items.push(Err((40, 10)));
    let mut out = vec![0u8; 4096];
    lznt1::decompress(&chunk(&items), &mut out).unwrap();
    let want: Vec<u8> = (0..40u8).chain(0..10u8).collect();
    assert_eq!(&out[..50], &want[..]);
    // A chunk stored as it is, then a compressed one in the next 4 KiB.
    let mut input = vec![0xff, 0x3f];
    input.extend(pattern(5, 4096));
    input.extend(chunk(&[Ok(b'z')]));
    let mut out = vec![0u8; 8192];
    lznt1::decompress(&input, &mut out).unwrap();
    assert_eq!(&out[..4096], &pattern(5, 4096)[..]);
    assert_eq!(out[4096], b'z');
    // A copy from before the chunk's start.
    let bad = chunk(&[Ok(b'a'), Err((2, 3))]);
    assert_eq!(lznt1::decompress(&bad, &mut out).err(), Some(Error::Corrupt));
}

#[test]
fn run_lists_put_runs_and_holes_together() {
    // 16 clusters at 256, a hole of 8, 4 clusters 16 back, and the end.
    let packed = [0x21, 0x10, 0x00, 0x01, 0x01, 0x08, 0x11, 0x04, 0xf0, 0x00];
    let mut runs = Runs::new();
    runs.add(&packed, 0, 27, 1000).unwrap();
    assert_eq!(runs.map(0), Some((256, 16)));
    assert_eq!(runs.map(17), Some((HOLE, 7)));
    assert_eq!(runs.map(25), Some((241, 3)));
    assert_eq!(runs.map(28), None);
    assert_eq!(runs.allocated_in(0, 28), 20);
    // A second piece, from where the first ended.
    runs.add(&[0x11, 0x02, 0x05, 0x00], 28, 29, 1000).unwrap();
    assert_eq!(runs.map(28), Some((5, 2)));
    for (packed, lowest, highest) in [
        // Past the volume.
        (&[0x21, 0x10, 0xff, 0x03, 0x00][..], 0, 15),
        // Before the volume's start.
        (&[0x11, 0x10, 0xf0, 0x00], 0, 15),
        // A run of no clusters.
        (&[0x11, 0x00, 0x10, 0x00], 0, 0),
        // No end.
        (&[0x11, 0x10, 0x10], 0, 15),
        // Not the clusters its attribute says.
        (&[0x11, 0x10, 0x10, 0x00], 0, 20),
    ] {
        assert_eq!(Runs::new().add(packed, lowest, highest, 1000).err(), Some(Error::Corrupt));
    }
    // A piece that does not start where the runs end.
    assert_eq!(runs.add(&[0x11, 0x01, 0x05, 0x00], 40, 40, 1000).err(), Some(Error::Corrupt));
}

#[test]
fn a_torn_record_is_refused() {
    let mut record = vec![0u8; 1024];
    record[..4].copy_from_slice(b"FILE");
    record[4..6].copy_from_slice(&0x30u16.to_le_bytes());
    record[6..8].copy_from_slice(&3u16.to_le_bytes());
    record[0x30..0x32].copy_from_slice(&[0x34, 0x12]);
    record[0x32..0x34].copy_from_slice(&[0xaa, 0xbb]);
    record[0x34..0x36].copy_from_slice(&[0xcc, 0xdd]);
    record[510..512].copy_from_slice(&[0x34, 0x12]);
    record[1022..1024].copy_from_slice(&[0x34, 0x12]);
    let mut good = record.clone();
    fix_up(&mut good, b"FILE").unwrap();
    assert_eq!((&good[510..512], &good[1022..1024]), (&[0xaa, 0xbb][..], &[0xcc, 0xdd][..]));
    let mut torn = record.clone();
    torn[1022] = 0x35;
    assert_eq!(fix_up(&mut torn, b"FILE").err(), Some(Error::Corrupt));
    assert_eq!(fix_up(&mut record, b"INDX").err(), Some(Error::Corrupt));
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(512))]

    // What comes from the disk never makes the parsers panic.
    #[test]
    fn records_from_the_disk_never_panic(bytes in proptest::collection::vec(any::<u8>(), 0..1100)) {
        let mut fixed = bytes.clone();
        let _ = fix_up(&mut fixed, b"FILE");
        if let Ok(rec) = Record::new(&bytes) {
            for attr in rec.attrs().take(64) {
                let Ok(attr) = attr else { break };
                let _ = (attr.name(), attr.value(), attr.size(), attr.initialized_size());
                if attr.non_resident() {
                    let _ = Runs::new().add(attr.runs(), attr.lowest_vcn(), attr.highest_vcn(), 1 << 20);
                }
            }
        }
        for entry in index::entries(&bytes, 0).take(64) {
            let Ok(entry) = entry else { break };
            let _ = (entry.reference(), entry.subnode(), entry.name().map(|n| n.units().count()));
        }
        let _ = crate::reparse::target(&bytes);
        let mut out = vec![0u8; 16384];
        let _ = lznt1::decompress(&bytes, &mut out);
        let _ = Runs::new().add(&bytes, 0, 100, 1 << 20);
    }
}
