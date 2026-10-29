// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Host tests of reading Btrfs, against the reference tools: filesystems
//! are made by `mkfs.btrfs` and read back (btrfs-progs, `apt install
//! btrfs-progs`; `ob doctor` checks for it).

use std::collections::BTreeSet;
use std::format;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::string::{String, ToString};
use std::sync::atomic::{AtomicU32, Ordering};
use std::vec::Vec;

use radix_block::DeviceRef;
use radix_block::ram::RamDisk;
use radix_vfs::Error;
use radix_vfs::mem::{self, KArc, KBox};

use crate::Volume;
use crate::chunk::kind;
use crate::csum::{self, CsumType};
use crate::format::{Key, item, name_hash, tree, u16_at, u64_at};
use crate::superblock::{self, incompat};
use crate::tree::{Cursor, HEADER_SIZE, Root, find};

/// A tool of btrfs-progs: from the directory `RADIX_BTRFS_PROGS` names,
/// where `ob test` puts a newer copy when the system's is too old for
/// these tests (its `sbin`, and the `bin` beside it), or else the
/// system's.
fn tool(name: &str) -> PathBuf {
    let path = std::env::var_os("PATH").unwrap_or_default();
    let dirs = std::env::var_os("RADIX_BTRFS_PROGS")
        .map(PathBuf::from)
        .into_iter()
        .flat_map(|sbin| [sbin.join("../bin"), sbin])
        .chain(
            ["/usr/sbin", "/sbin", "/usr/bin", "/bin", "/usr/local/sbin", "/usr/local/bin"]
                .into_iter()
                .map(PathBuf::from),
        )
        .chain(std::env::split_paths(&path));
    for dir in dirs {
        let path = dir.join(name);
        if path.exists() {
            return path;
        }
    }
    panic!("{name} is missing: install btrfs-progs (apt install btrfs-progs)");
}

/// A path for scratch files.
fn scratch(what: &str) -> PathBuf {
    static NEXT: AtomicU32 = AtomicU32::new(0);
    let n = NEXT.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!("radix-btrfs-{}-{what}-{n}", std::process::id()))
}

/// A directory of `count` empty files, `n0` to `n{count-1}`, and a
/// subdirectory `sub` holding `inner`.
fn tree_of(count: usize) -> PathBuf {
    let dir = scratch("root");
    std::fs::create_dir_all(dir.join("sub")).unwrap();
    for n in 0..count {
        std::fs::write(dir.join(format!("n{n}")), b"").unwrap();
    }
    std::fs::write(dir.join("sub/inner"), b"inside").unwrap();
    dir
}

/// The names `tree_of(count)` puts in its top directory.
fn names_of(count: usize) -> BTreeSet<String> {
    (0..count).map(|n| format!("n{n}")).chain(["sub".to_string()]).collect()
}

/// A filesystem from `mkfs.btrfs args`, holding the files of `dir`,
/// shrunk to fit them.
fn mkfs(args: &[&str], dir: &Path) -> Vec<u8> {
    let path = scratch("img");
    std::fs::File::create(&path).unwrap();
    let out = Command::new(tool("mkfs.btrfs"))
        .args(["-q", "-f"])
        .args(args)
        .arg("--rootdir")
        .arg(dir)
        .arg("--shrink")
        .arg(&path)
        .stdin(std::process::Stdio::null())
        .output()
        .unwrap();
    assert!(out.status.success(), "mkfs.btrfs: {}", String::from_utf8_lossy(&out.stderr));
    let image = std::fs::read(&path).unwrap();
    let _ = std::fs::remove_file(&path);
    image
}

/// Runs the halves of a `radix_vfs::join` on two threads, as the kernel
/// runs them on two CPUs.
fn join_on_threads(a: &mut dyn FnMut(), b: &mut (dyn FnMut() + Send)) {
    std::thread::scope(|scope| {
        scope.spawn(b);
        a();
    });
}

/// A disk of logical blocks of `block` bytes, holding `image`. Its
/// filesystem's joins run on two threads.
fn disk_of(block: u32, image: &[u8]) -> KArc<RamDisk> {
    radix_vfs::set_join(join_on_threads);
    let sectors = image.len() as u64 / 512;
    let disk = mem::arc(RamDisk::with_blocks(sectors, true, block)).unwrap();
    disk.load(0, image).unwrap();
    disk
}

/// The filesystem in `image`, opened on a disk of logical blocks of
/// `block` bytes.
fn open_in(block: u32, image: &[u8]) -> Result<KBox<Volume>, Error> {
    let device: DeviceRef = disk_of(block, image);
    Volume::open(device)
}

fn open(image: &[u8]) -> Result<KBox<Volume>, Error> {
    open_in(512, image)
}

/// The names in the top directory of the filesystem tree, by its
/// `DIR_INDEX` items.
fn listing(volume: &Volume) -> Result<BTreeSet<String>, Error> {
    let fs = volume.tree(tree::FS)?.ok_or(Error::Corrupt)?;
    let mut cursor = Cursor::seek(volume, &fs, &Key::first(tree::FIRST_FREE, item::DIR_INDEX))?;
    let mut names = BTreeSet::new();
    while let Some((key, data)) = cursor.get() {
        if !key.is(tree::FIRST_FREE, item::DIR_INDEX) {
            break;
        }
        let len = usize::from(u16_at(data, 27));
        let name = data.get(30..30 + len).ok_or(Error::Corrupt)?;
        names.insert(String::from_utf8_lossy(name).into_owned());
        cursor.advance()?;
    }
    Ok(names)
}

/// How many items tree `root` holds, read one after the other.
fn count(volume: &Volume, root: &Root) -> Result<usize, Error> {
    let mut cursor = Cursor::seek(volume, root, &Key::new(0, 0, 0))?;
    let mut n = 0;
    let mut last = None;
    while let Some(key) = cursor.key() {
        assert!(last < Some(key), "the items are out of order");
        last = Some(key);
        n += 1;
        cursor.advance()?;
    }
    Ok(n)
}

/// `bytes` in lowercase hexadecimal.
fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write;
    let mut out = String::new();
    for b in bytes {
        let _ = write!(out, "{b:02x}");
    }
    out
}

/// Rewrites the superblock's checksum after a change to it.
fn reseal_superblock(image: &mut [u8]) {
    let at = superblock::OFFSET as usize;
    let sum = CsumType::Crc32c.sum(&image[at + 32..at + superblock::SIZE]);
    image[at..at + 32].copy_from_slice(&sum);
}

#[test]
fn checksums_match_their_specifications() {
    assert_eq!(csum::crc32c(b"123456789"), 0xe306_9283);
    assert_eq!(csum::crc32c(b""), 0);
    // The tables give what the processor's instruction does.
    let data = files::noise(3, 20_000);
    for len in [0, 1, 7, 8, 9, 4096, 4097, 12_345, 20_000] {
        let part = &data[..len];
        assert_eq!(csum::crc32c_tables(!0, part), csum::crc32c_update(!0, part), "{len}");
    }
    assert_eq!(csum::xxhash64(b""), 0xef46_db37_51d8_e999);
    assert_eq!(csum::xxhash64(b"a"), 0xd24e_c4f1_a98c_6e5b);
    assert_eq!(csum::xxhash64(b"Nobody inspects the spammish repetition"), 0xfbce_a83c_8a37_8bf1);
    // FIPS 180-4's and RFC 7693's examples.
    assert_eq!(
        hex(&CsumType::Sha256.sum(b"abc")),
        "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
    );
    assert_eq!(
        hex(&CsumType::Blake2b.sum(b"abc")),
        "bddd813c634239723171ef3fee98579b94964e3bb1cb3e427262c8c068d52319"
    );
    // Messages around the block sizes and their padding, as Python's
    // hashlib digests them: bytes 0, 1, 2, ... wrapping at 256.
    for (len, sha, blake) in [
        (
            0,
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855",
            "0e5751c026e543b2e8ab2eb06099daa1d1e5df47778f7787faab45cdf12fe3a8",
        ),
        (
            55,
            "463eb28e72f82e0a96c0a4cc53690c571281131f672aa229e0d45ae59b598b59",
            "ace7db9e8f298e823b7b265bbcd004577e0029256c48684b2d7a7cdfcbff5e47",
        ),
        (
            56,
            "da2ae4d6b36748f2a318f23e7ab1dfdf45acdc9d049bd80e59de82a60895f562",
            "a6c1d35f74218e57b4dbd4ed467b41981208666738ffa0d9dc53d3de96be702a",
        ),
        (
            63,
            "29af2686fd53374a36b0846694cc342177e428d1647515f078784d69cdb9e488",
            "29e41a64fbdd2fd27612228623c0702222bf367451e7324287f181cb3dcf7237",
        ),
        (
            64,
            "fdeab9acf3710362bd2658cdc9a29e8f9c757fcf9811603a8c447cd1d9151108",
            "10d8e6d534b00939843fe9dcc4dae48cdf008f6b8b2b82b156f5404d874887f5",
        ),
        (
            65,
            "4bfd2c8b6f1eec7a2afeb48b934ee4b2694182027e6d0fc075074f2fabb31781",
            "84c04ab082c8ae24206561f77397704b627892089a05887a2a1996472bcfe15d",
        ),
        (
            111,
            "60780e9451bdc43cf4530ffc95cbb0c4eb24dae2c39f55f334d679e076c08065",
            "e08b3d16f0a3b663f319da999db897488d1677048a38fa7b4f1f783e4f1974e6",
        ),
        (
            112,
            "09373f127d34e61dbbaa8bc4499c87074f2ddb10e1b465f506d7d70a15011979",
            "d5587e0b33166a320dd69417adb01cb1129aff5506a2f655574cf2e264636bdb",
        ),
        (
            119,
            "da18797ed7c3a777f0847f429724a2d8cd5138e6ed2895c3fa1a6d39d18f7ec6",
            "b0d6518afa4e3590746aa5fd1e58f7ec06a347981177737ad6631b22c53b6617",
        ),
        (
            120,
            "f52b23db1fbb6ded89ef42a23ce0c8922c45f25c50b568a93bf1c075420bbb7c",
            "16684ddd272dfc18dd40c16a7f57cecca70df0a96c4a066b97646953e7c7691a",
        ),
        (
            127,
            "92ca0fa6651ee2f97b884b7246a562fa71250fedefe5ebf270d31c546bfea976",
            "f2fe67ff342e21b8f45e8f2e0bcd1d9243245d50ee6c78042e9c491388791c72",
        ),
        (
            128,
            "471fb943aa23c511f6f72f8d1652d9c880cfa392ad80503120547703e56a2be5",
            "c3582f71ebb2be66fa5dd750f80baae97554f3b015663c8be377cfcb2488c1d1",
        ),
        (
            129,
            "5099c6a56203f9687f7d33f4bfdf576d31dc91f6b695ecea38b2770c87631135",
            "f7f3c46ba2564ff4c4c162da1f5b605f9f1c4aa6a20652a9f9a337c1a2f5b9c9",
        ),
        (
            256,
            "40aff2e9d2d8922e47afd4648e6967497158785fbd1da870e7110266bf944880",
            "39a7eb9fedc19aabc83425c6755dd90e6f9d0c804964a1f4aaeea3b9fb599835",
        ),
        (
            1000,
            "a8af099bf2e878609558dbf69d8f88f4a31040a8cf84b549a0cfa912f12ffc3f",
            "c636324d47d89f2b2434dc2c994100663fbbaea880ff020fc5de89dd0f77a1ec",
        ),
        (
            4096,
            "c8f5d0341d54d951a71b136e6e2afcb14d11ed8489a7ae126a8fee0df6ecf193",
            "81baab4ea5d75a0aa53b8780ae5dbd7c8dac7f07e7c103644399492bb9ea6f87",
        ),
    ] {
        let data: Vec<u8> = (0..len).map(|i| i as u8).collect();
        assert_eq!(hex(&CsumType::Sha256.sum(&data)), sha, "SHA-256 of {len} bytes");
        assert_eq!(hex(&CsumType::Blake2b.sum(&data)), blake, "BLAKE2b of {len} bytes");
    }
    // Every length up to two blocks of eight and of thirty-two bytes, by
    // the tables and byte by byte.
    let data: Vec<u8> = (0..80u8).map(|b| b.wrapping_mul(151)).collect();
    for len in 0..data.len() {
        let mut slow = !0u32;
        for &byte in &data[..len] {
            slow ^= u32::from(byte);
            for _ in 0..8 {
                slow = if slow & 1 != 0 { (slow >> 1) ^ 0x82f6_3b78 } else { slow >> 1 };
            }
        }
        assert_eq!(csum::crc32c(&data[..len]), !slow, "length {len}");
    }
}

#[test]
fn reads_every_checksum_and_node_size() {
    let dir = tree_of(40);
    for (csum, name) in [
        (CsumType::Crc32c, "crc32c"),
        (CsumType::Xxhash64, "xxhash"),
        (CsumType::Sha256, "sha256"),
        (CsumType::Blake2b, "blake2"),
    ] {
        for nodesize in ["4096", "16384", "65536"] {
            let image = mkfs(&["-m", "single", "--csum", name, "--nodesize", nodesize], &dir);
            let volume = open(&image).unwrap();
            let sb = volume.superblock();
            assert_eq!(sb.csum_type, csum);
            assert_eq!(sb.nodesize.to_string(), nodesize);
            assert_eq!(sb.sectorsize, 4096);
            assert_eq!(listing(&volume).unwrap(), names_of(40), "{name}, nodes of {nodesize}");
            // And each name by its hash, as a lookup finds it.
            let fs = volume.tree(tree::FS).unwrap().unwrap();
            for name in names_of(40) {
                let key = Key::new(tree::FIRST_FREE, item::DIR_ITEM, name_hash(name.as_bytes()));
                let found = find(&volume, &fs, &key).unwrap().expect("a name's item");
                let len = usize::from(u16_at(&found, 27));
                assert_eq!(&found[30..30 + len], name.as_bytes());
            }
        }
    }
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn walks_trees_of_three_levels() {
    let dir = tree_of(3000);
    let image = mkfs(&["-m", "single", "--nodesize", "4096"], &dir);
    // On disks of 4 KiB blocks too.
    for block in [512, 4096] {
        let volume = open_in(block, &image).unwrap();
        let fs = volume.tree(tree::FS).unwrap().unwrap();
        assert!(fs.level >= 2, "level {}", fs.level);
        assert_eq!(listing(&volume).unwrap(), names_of(3000));
        // Each file has an inode item, a reference from its directory,
        // and two directory items: by hash and by index.
        let items = count(&volume, &fs).unwrap();
        assert!(items >= 4 * 3000, "{items} items");
        for n in [0, 1499, 2999] {
            let name = format!("n{n}");
            let key = Key::new(tree::FIRST_FREE, item::DIR_ITEM, name_hash(name.as_bytes()));
            assert!(find(&volume, &fs, &key).unwrap().is_some(), "{name}");
        }
        let missing = Key::new(tree::FIRST_FREE, item::DIR_ITEM, name_hash(b"n3000"));
        assert!(find(&volume, &fs, &missing).unwrap().is_none());
        // The other trees read too.
        for id in [tree::EXTENT, tree::DEV, tree::CSUM] {
            let root = volume.tree(id).unwrap().unwrap();
            count(&volume, &root).unwrap();
        }
        count(&volume, &volume.chunk_root()).unwrap();
        count(&volume, &volume.root_tree()).unwrap();
    }
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn reads_the_profiles_one_device_has() {
    let dir = tree_of(10);
    for args in [&["-m", "single", "-d", "single"][..], &["-m", "dup", "-d", "dup"], &["--mixed"]] {
        let image = mkfs(args, &dir);
        let volume = open(&image).unwrap();
        assert_eq!(listing(&volume).unwrap(), names_of(10), "{args:?}");
        volume.with_chunks(|map| {
            let chunks = map.chunks();
            assert!(chunks.iter().any(|c| c.kind & kind::SYSTEM != 0));
            let dup = args.contains(&"dup");
            assert!(
                chunks.iter().filter(|c| c.kind & kind::METADATA != 0).all(|c| {
                    (c.kind & kind::DUP != 0) == dup && c.stripes.len() == if dup { 2 } else { 1 }
                }),
                "{args:?}"
            );
        });
    }
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn reads_the_other_copy_of_a_damaged_block() {
    let dir = tree_of(10);
    let mut image = mkfs(&["-m", "dup"], &dir);
    let root = open(&image).unwrap().root_tree();
    let place = open(&image).unwrap().place(root.bytenr).unwrap();
    assert_eq!(place.copies().len(), 2);
    // A damaged first copy: the second is read.
    let first = place.copies()[0].at as usize;
    image[first + HEADER_SIZE + 3] ^= 0x40;
    assert_eq!(listing(&open(&image).unwrap()).unwrap(), names_of(10));
    // A block from another transaction (a lost write) is no better.
    let second = place.copies()[1].at as usize;
    image[first + HEADER_SIZE + 3] ^= 0x40;
    image[first + 0x50] ^= 1;
    let sum = CsumType::Crc32c.sum(&image[first + 32..first + 16384]);
    image[first..first + 32].copy_from_slice(&sum);
    assert_eq!(listing(&open(&image).unwrap()).unwrap(), names_of(10));
    // Both damaged: the filesystem is.
    image[second + HEADER_SIZE + 3] ^= 0x40;
    assert_eq!(open(&image).and_then(|v| listing(&v)).err(), Some(Error::Corrupt));
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn refuses_what_it_cannot_read() {
    let dir = tree_of(1);
    let image = mkfs(&["-m", "single"], &dir);
    let sb = superblock::OFFSET as usize;
    let with = |change: &dyn Fn(&mut Vec<u8>)| {
        let mut image = image.clone();
        change(&mut image);
        open(&image).err()
    };
    assert_eq!(with(&|_| {}), None);
    // Not Btrfs.
    assert_eq!(with(&|i| i[sb + 0x40] = b'X'), Some(Error::UnknownFilesystem));
    assert_eq!(open(&image[..64 * 1024]).err(), Some(Error::UnknownFilesystem));
    // Damaged: the checksum, or fields that cannot be.
    assert_eq!(with(&|i| i[sb + 0x200] ^= 1), Some(Error::Corrupt));
    let resealed = |at: usize, value: u64| {
        move |i: &mut Vec<u8>| {
            i[sb + at..sb + at + 8].copy_from_slice(&value.to_le_bytes());
            reseal_superblock(i);
        }
    };
    assert_eq!(with(&resealed(0x30, 0x20000)), Some(Error::Corrupt), "its own address");
    assert_eq!(with(&resealed(0x90, 0x1000_1000)), Some(Error::Corrupt), "sector size");
    assert_eq!(with(&resealed(0x50, u64_at(&image, sb + 0x50) + 512)), Some(Error::Corrupt));
    // A smaller device than the filesystem: its end is lost.
    let mut short = image.clone();
    short.truncate(image.len() - 1024 * 1024);
    assert_eq!(open(&short).err(), Some(Error::Corrupt));
    // Features Radix does not read yet, or at all.
    let flags = u64_at(&image, sb + 0xbc);
    assert_eq!(with(&resealed(0xbc, flags | incompat::ZONED)), Some(Error::NotSupported));
    assert_eq!(with(&resealed(0xbc, flags | incompat::RAID56)), Some(Error::NotSupported));
    assert_eq!(with(&resealed(0xbc, flags | 1 << 20)), Some(Error::NotSupported));
    // Two disks, but the chunk tree names one; more disks than Radix
    // mounts.
    assert_eq!(with(&resealed(0x88, 2)), Some(Error::Corrupt), "two devices");
    assert_eq!(with(&resealed(0x88, 17)), Some(Error::NotSupported), "17 devices");
    // A log of fsyncs is replayed (ADR 0151), but its root has a level.
    assert_eq!(
        with(&|i| {
            i[sb + 0x60..sb + 0x68].copy_from_slice(&0x10000u64.to_le_bytes());
            i[sb + 0xc8] = 8;
            reseal_superblock(i);
        }),
        Some(Error::Corrupt),
        "a log root above the highest level"
    );
    assert_eq!(
        with(&|i| {
            i[sb + 0xc4] = 9;
            reseal_superblock(i);
        }),
        Some(Error::NotSupported),
        "an unknown checksum"
    );
    std::fs::remove_dir_all(dir).unwrap();
}

/// A small generator of numbers, for the same damage on every run.
struct XorShift(u64);

impl XorShift {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }

    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }
}

/// Tree blocks with their checksums put right after damage, so that only
/// the checks of what is in them stand between the damage and the code
/// that reads it: every read ends in an answer or an error, never a
/// panic.
#[test]
fn damaged_blocks_with_good_checksums_never_panic() {
    let dir = tree_of(300);
    let image = mkfs(&["-m", "single", "--nodesize", "4096"], &dir);
    let fsid = &image[superblock::OFFSET as usize + 0x20..][..16];
    // Every tree block: the 4 KiB blocks whose header names the
    // filesystem and their own place.
    let blocks: Vec<usize> = (0..image.len() / 4096)
        .map(|n| n * 4096)
        .filter(|&at| &image[at + 0x20..at + 0x30] == fsid && u64_at(&image, at + 0x30) != 0)
        .collect();
    assert!(blocks.len() > 10, "{} blocks", blocks.len());
    let disk = disk_of(512, &image);
    let mut random = XorShift(0x9e37_79b9_7f4a_7c15);
    let mut refused = 0;
    for _ in 0..300 {
        let block = blocks[random.below(blocks.len())];
        let mut damaged = image[block..block + 4096].to_vec();
        for _ in 0..=random.below(3) {
            // The header and the first items, where the checks are, more
            // often than the rest.
            let span = if random.below(2) == 0 { 400 } else { 4096 - 32 };
            let at = 32 + random.below(span);
            damaged[at] = match random.below(3) {
                0 => damaged[at] ^ (1 << random.below(8)),
                1 => 0xff,
                _ => random.next() as u8,
            };
        }
        let sum = CsumType::Crc32c.sum(&damaged[32..]);
        damaged[..32].copy_from_slice(&sum);
        disk.load(block as u64, &damaged).unwrap();
        let read = Volume::open(disk.clone()).and_then(|volume| {
            for id in [tree::ROOT, tree::EXTENT, tree::DEV, tree::FS, tree::CSUM] {
                let root =
                    if id == tree::ROOT { Some(volume.root_tree()) } else { volume.tree(id)? };
                if let Some(root) = root {
                    count(&volume, &root)?;
                }
            }
            listing(&volume)
        });
        if read.is_err() {
            refused += 1;
        }
        disk.load(block as u64, &image[block..block + 4096]).unwrap();
    }
    // Damage to what the checks cover is caught; damage to an item's
    // contents is for whoever reads it.
    assert!(refused > 0, "none of 300 refused");
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn a_cache_full_of_blocks_still_reads() {
    let dir = tree_of(3000);
    let image = mkfs(&["-m", "single", "--nodesize", "4096"], &dir);
    let mut volume = open(&image).unwrap();
    volume.limit_blocks(16);
    let fs = volume.tree(tree::FS).unwrap().unwrap();
    // More blocks than the cache keeps, twice; then dropped.
    for _ in 0..2 {
        assert_eq!(listing(&volume).unwrap(), names_of(3000));
        count(&volume, &fs).unwrap();
    }
    assert!(volume.shrink(usize::MAX) > 0);
    assert_eq!(listing(&volume).unwrap(), names_of(3000));
    std::fs::remove_dir_all(dir).unwrap();
}

mod balance;
mod clones;
mod compressed;
mod files;
mod fsync;
mod links;
mod log;
mod quota;
mod raid;
mod rw;
mod scrub;
mod snapshots;
mod trim;
mod write;
