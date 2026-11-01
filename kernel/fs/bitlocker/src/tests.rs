// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Host tests: volumes [`crate::make`] encrypts unlock with each of their
//! protectors and with nothing else, read back through
//! [`crate::device::Unlocked`] as they were, in every method; dislocker
//! and libbde, which unlock what Windows makes, unlock them too, and
//! dislocker reads back what Radix's NTFS wrote through the device.

use std::path::PathBuf;
use std::process::Command;
use std::string::{String, ToString};
use std::sync::atomic::{AtomicU32, Ordering};
use std::vec::Vec;
use std::{format, vec};

use radix_block::mem::CachePage;
use radix_block::ram::RamDisk;
use radix_block::{BlockDevice, DeviceRef, PAGE_SIZE, SECTOR, SegmentMut};
use radix_vfs::mem::{self, KArc};
use radix_vfs::{Access, FileSystem, Kind, NodeRef, Slice, SliceMut};

use crate::cipher::Method;
use crate::device::{OpenError, Unlocked};
use crate::format::{self, Error, Metadata, Volume};
use crate::keys::{self, Secret};
use crate::make::{self, Options, RESERVED};

const PASSWORD: &str = "Correct horse, battery staple!";
/// A volume's size: 8 MiB.
const SIZE: usize = 8 << 20;

/// An FVEK for `method`.
fn fvek(method: Method) -> Vec<u8> {
    (0..method.key_bytes()).map(|i| (i * 31 + 7) as u8).collect()
}

fn options<'a>(method: Method, fvek: &'a [u8], recovery: Option<&'a str>) -> Options<'a> {
    Options {
        method,
        fvek,
        vmk: [0x77; 32],
        password: Some(PASSWORD),
        recovery,
        clear: false,
        to_go: false,
        sector: 512,
        description: "DUKOVO-TEST Test volume 2026-10-31",
    }
}

/// A volume of noise, its first sector a boot sector's, with the last
/// [`RESERVED`] bytes left for BitLocker.
fn noise(size: usize) -> Vec<u8> {
    let mut state = 0x1234_5678u32;
    let mut image: Vec<u8> = (0..size)
        .map(|_| {
            state ^= state << 13;
            state ^= state >> 17;
            state ^= state << 5;
            state as u8
        })
        .collect();
    image[510] = 0x55;
    image[511] = 0xaa;
    image[size - RESERVED as usize..].fill(0);
    image
}

/// What the volume unlocked reads: `plain`, but for its tail, where
/// BitLocker's parts are, which reads as zeros.
fn seen(plain: &[u8]) -> Vec<u8> {
    let mut out = plain.to_vec();
    let len = out.len();
    out[len - RESERVED as usize..].fill(0);
    out
}

/// The metadata of `image`, from its first copy.
fn metadata_of(image: &[u8]) -> Metadata<'_> {
    let Some(Volume::Locked { metadata, sector }) = format::volume(&image[..512]) else {
        panic!("no BitLocker volume");
    };
    let at = metadata[0] as usize;
    Metadata::parse(&image[at..at + format::METADATA_AREA as usize], sector).unwrap()
}

/// A RAM disk holding `image`, in logical blocks of `block` bytes.
fn disk(image: &[u8], block: u32) -> KArc<RamDisk> {
    let disk = mem::arc(RamDisk::with_blocks(image.len() as u64 / SECTOR, false, block)).unwrap();
    disk.load(0, image).unwrap();
    disk
}

/// Every byte of `device`, read a page at a time.
fn read_device(device: &dyn BlockDevice) -> Vec<u8> {
    let sectors = device.info().sectors;
    let mut out = Vec::new();
    let mut page = CachePage::new().unwrap();
    let mut sector = 0;
    while sector < sectors {
        let len = ((sectors - sector) * SECTOR).min(PAGE_SIZE as u64) as usize;
        device.read(sector, &mut [SegmentMut { page: &mut page, offset: 0, len }]).unwrap();
        out.extend_from_slice(&page.bytes()[..len]);
        sector += len as u64 / SECTOR;
    }
    out
}

fn scratch(what: &str) -> PathBuf {
    static NEXT: AtomicU32 = AtomicU32::new(0);
    let n = NEXT.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!("radix-bitlocker-{}-{what}-{n}", std::process::id()))
}

/// Runs `cmd`, which must succeed; returns what it printed.
fn run(cmd: &mut Command) -> String {
    let out = cmd.env("LC_ALL", "C.UTF-8").output().unwrap();
    let text = String::from_utf8_lossy(&out.stdout).into_owned();
    assert!(out.status.success(), "{cmd:?} failed: {text}{}", String::from_utf8_lossy(&out.stderr));
    text
}

/// The volume `image` as dislocker decrypts it with `how`: `-u` and a
/// password, `-p` and a recovery key, or `-c`.
fn dislocker(image: &[u8], how: &str) -> Vec<u8> {
    let (volume, out) = (scratch("volume.img"), scratch("decrypted.img"));
    std::fs::write(&volume, image).unwrap();
    run(Command::new("dislocker-file").arg("-V").arg(&volume).arg(how).arg("--").arg(&out));
    let decrypted = std::fs::read(&out).unwrap();
    let _ = std::fs::remove_file(&volume);
    let _ = std::fs::remove_file(&out);
    decrypted
}

/// What libbde's bdeinfo says of `image`, unlocked with `args`.
fn bdeinfo(image: &[u8], args: &[&str]) -> String {
    let volume = scratch("volume.img");
    std::fs::write(&volume, image).unwrap();
    let text = run(Command::new("bdeinfo").args(args).arg(&volume));
    let _ = std::fs::remove_file(&volume);
    text
}

/// An NTFS volume `mkntfs` made of all but the last [`RESERVED`] bytes
/// of `size`, with `hello.txt` in it, which `ntfscp` wrote.
fn ntfs(size: usize) -> Vec<u8> {
    let (path, file) = (scratch("ntfs.img"), scratch("hello.txt"));
    std::fs::write(&path, vec![0u8; size]).unwrap();
    let sectors = (size - RESERVED as usize) / 512;
    run(Command::new("mkntfs")
        .args(["-q", "-F", "-f", "-s", "512", "-c", "4096", "-L", "SECRET"])
        .arg(&path)
        .arg(sectors.to_string()));
    std::fs::write(&file, b"Hello from Windows, encrypted.\n").unwrap();
    run(Command::new("ntfscp").arg("-f").arg(&path).arg(&file).arg("hello.txt"));
    let image = std::fs::read(&path).unwrap();
    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_file(&file);
    image
}

/// What ntfs-3g's `ntfscat` reads of `path` in the volume `image`.
fn ntfscat(image: &[u8], path: &str) -> Vec<u8> {
    let volume = scratch("cat.img");
    std::fs::write(&volume, image).unwrap();
    let out = Command::new("ntfscat").arg("-f").arg(&volume).arg(path).output().unwrap();
    let _ = std::fs::remove_file(&volume);
    assert!(out.status.success(), "ntfscat {path}: {}", String::from_utf8_lossy(&out.stderr));
    out.stdout
}

#[test]
fn every_protector_unlocks_and_nothing_else_does() {
    let method = Method::Xts128;
    let key = fvek(method);
    let recovery = make::recovery_key(4242);
    let mut image = noise(SIZE);
    let mut with = options(method, &key, Some(&recovery));
    with.clear = true;
    make::make(&mut image, &with);
    let metadata = metadata_of(&image);
    assert_eq!(metadata.method, method.number());
    let description: String =
        char::decode_utf16(metadata.description().unwrap()).map(Result::unwrap).collect();
    assert_eq!(description, "DUKOVO-TEST Test volume 2026-10-31");
    let unlocked = keys::unlock(&metadata, Secret::Password(PASSWORD)).unwrap();
    assert_eq!((unlocked.method, unlocked.bytes()), (method.number(), &key[..]));
    let bytes = keys::recovery_key(&recovery).unwrap();
    assert_eq!(keys::unlock(&metadata, Secret::Recovery(&bytes)).unwrap().bytes(), &key[..]);
    // With dashes, spaces or neither.
    assert_eq!(keys::recovery_key(&recovery.replace('-', " ")).as_deref(), Some(&*bytes));
    assert_eq!(keys::recovery_key(&recovery.replace('-', "")).as_deref(), Some(&*bytes));
    assert_eq!(keys::unlock_clear(&metadata).unwrap().bytes(), &key[..]);
    let wrong = keys::unlock(&metadata, Secret::Password("correct horse, battery staple!"));
    assert_eq!(wrong.err(), Some(Error::WrongKey));
    let other = keys::recovery_key(&make::recovery_key(4243)).unwrap();
    assert_eq!(keys::unlock(&metadata, Secret::Recovery(&other)).err(), Some(Error::WrongKey));
    // A recovery key's numbers are each 11 times a 16-bit one.
    let mut digits: Vec<char> = recovery.chars().collect();
    digits[5] = if digits[5] == '9' { '8' } else { '9' };
    assert!(keys::recovery_key(&digits.iter().collect::<String>()).is_none());
    assert!(keys::recovery_key("123456").is_none());
    // A volume of only a password has no recovery key or clear key. It
    // holds an NTFS, whose size libbde reads.
    let mut image = ntfs(SIZE);
    make::make(&mut image, &options(method, &key, None));
    let metadata = metadata_of(&image);
    assert_eq!(keys::unlock(&metadata, Secret::Recovery(&bytes)).err(), Some(Error::NoProtector));
    assert_eq!(keys::unlock_clear(&metadata).err(), Some(Error::NoProtector));
    // libbde unlocks it with the password, and finds the method.
    let info = bdeinfo(&image, &["-p", PASSWORD]);
    assert!(info.contains("Encryption method") && info.contains("AES-XTS 128-bit"), "{info}");
    // And with nothing else.
    for args in [["-r", &*make::recovery_key(1)], ["-p", "a wrong password"]] {
        let volume = scratch("volume.img");
        std::fs::write(&volume, &image).unwrap();
        let out = Command::new("bdeinfo").args(args).arg(&volume).output().unwrap();
        let _ = std::fs::remove_file(&volume);
        assert!(!out.status.success(), "bdeinfo {args:?} unlocked it");
    }
}

#[test]
fn every_method_reads_back_as_it_was() {
    let plain = noise(SIZE);
    for method in Method::ALL {
        for block in [512u32, 4096] {
            let key = fvek(method);
            let mut with = options(method, &key, None);
            with.password = None;
            with.clear = true;
            with.sector = u64::from(block);
            let mut image = plain.clone();
            make::make(&mut image, &with);
            assert!(image[1 << 20..(1 << 20) + 4096] != plain[1 << 20..(1 << 20) + 4096]);
            let device: DeviceRef = disk(&image, block);
            let unlocked = Unlocked::open(device.clone(), method.number(), &key, false).unwrap();
            assert_eq!(unlocked.info().logical_block, block);
            assert!(read_device(&unlocked) == seen(&plain), "{method:?} in {block}-byte sectors");
            // Another key, or another method, opens nothing.
            let mut other = key.clone();
            other[3] ^= 1;
            let wrong = Unlocked::open(device.clone(), method.number(), &other, false);
            assert_eq!(wrong.err(), Some(OpenError::BitLocker(Error::WrongKey)), "{method:?}");
            let number = method.number() ^ 1;
            let wrong = Unlocked::open(device, number, &key, false);
            assert_eq!(wrong.err(), Some(OpenError::BitLocker(Error::WrongKey)), "{method:?}");
            // dislocker decrypts it as Radix does.
            if block == 512 {
                assert!(dislocker(&image, "-c") == seen(&plain), "dislocker reads {method:?}");
            }
        }
    }
}

/// Writes `data` to a new file `name` in `dir`.
fn create(dir: &NodeRef, name: &str, data: &[u8]) {
    let node = dir.create(name, Kind::File, Access::default_for(Kind::File)).unwrap();
    assert_eq!(node.write(0, &Slice(data)).unwrap(), data.len());
}

#[test]
fn what_radix_writes_dislocker_reads() {
    let plain = ntfs(SIZE);
    for (method, to_go) in
        [(Method::Xts128, false), (Method::Cbc128, true), (Method::Elephant256, false)]
    {
        let key = fvek(method);
        let recovery = make::recovery_key(77);
        let mut with = options(method, &key, Some(&recovery));
        with.to_go = to_go;
        let mut image = plain.clone();
        make::make(&mut image, &with);
        let disk = disk(&image, 512);
        let device: DeviceRef = disk.clone();
        let unlocked: DeviceRef =
            mem::arc(Unlocked::open(device, method.number(), &key, false).unwrap()).unwrap();
        let fs = radix_ntfs::NtfsFs::mount(unlocked, false).unwrap();
        let root = fs.root();
        let hello = root.lookup("hello.txt").unwrap();
        let mut read = vec![0u8; 100];
        let n = hello.read(0, &mut SliceMut(&mut read)).unwrap();
        assert_eq!(&read[..n], b"Hello from Windows, encrypted.\n");
        let written: Vec<u8> = (0..300_000u32).map(|i| (i * 7 % 251) as u8).collect();
        create(&root, "written by Radix.bin", &written);
        assert_eq!(hello.write(6, &Slice(b"Dukovo,")).unwrap(), 7);
        drop((root, hello));
        fs.sync().unwrap();
        drop(fs);
        let mut raw = vec![0u8; SIZE];
        disk.dump(0, &mut raw);
        assert!(!raw.windows(7).any(|w| w == b"Dukovo,"), "{method:?} wrote in the clear");
        for how in [format!("-u{PASSWORD}"), format!("-p{recovery}")] {
            let decrypted = dislocker(&raw, &how);
            assert_eq!(ntfscat(&decrypted, "/hello.txt"), b"Hello Dukovo,ndows, encrypted.\n");
            assert!(ntfscat(&decrypted, "/written by Radix.bin") == written, "{method:?}");
        }
    }
}
