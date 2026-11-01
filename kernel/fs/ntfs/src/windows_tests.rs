// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! The volumes the write tests leave, kept for Windows' `chkdsk`.
//!
//! When `NTFS_IMAGES` names a directory, as it does in CI's unit job,
//! each volume a write test checks is kept there for the Windows job
//! (`.github/workflows/ci.yml`, `tools/windows/chkdsk.ps1`), which
//! mounts it, runs `chkdsk` on it and reads its files back:
//!
//! - `<name>.vhd`: the volume as the one partition of a fixed VHD, the
//!   format of Microsoft's Virtual Hard Disk Image Format Specification
//!   (2006), which Windows mounts as it is;
//! - `<name>.txt`: what Windows must find on it, a line for each path:
//!   `d` for a directory, `l` for a link, or `f` and the SHA-256 of its
//!   bytes, then a tab and the path; or `s` and the SHA-256 of a stream's
//!   bytes, then a tab and `path:stream` (ADR 0241).
//!
//! A name ending in `-cut` is of a volume a power cut left: `chkdsk /f`
//! must mend it, and only what was synced before the cut is listed, as
//! more may be on it. One ending in `-some` lists only what a test
//! changed on a volume others filled. A volume of sectors other than 512
//! bytes is not kept, as a VHD has none other.

use std::fmt::Write as _;
use std::os::unix::fs::FileExt;
use std::string::String;
use std::sync::atomic::{AtomicU32, Ordering};
use std::vec::Vec;
use std::{format, vec};

use sha2::{Digest, Sha256};

/// What a path holds.
pub(crate) enum Found<'a> {
    Dir,
    File(&'a [u8]),
    Link,
    /// A stream of a file or directory, its path `file:stream`
    /// (ADR 0241).
    Stream(&'a [u8]),
}

/// What a kept volume's list names.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Listed {
    /// Everything on it.
    All,
    /// Only some of what is on it: what a test changed.
    Some,
    /// What was synced before a power cut left it.
    Cut,
}

/// Keeps `image` and what Windows must `find` on it, if `NTFS_IMAGES`
/// asks; `listed` says what `find` names.
pub(crate) fn keep(image: &[u8], find: &[(&str, Found<'_>)], listed: Listed) {
    static NEXT: AtomicU32 = AtomicU32::new(0);
    let Ok(dir) = std::env::var("NTFS_IMAGES") else { return };
    if u16::from_le_bytes([image[11], image[12]]) != 512 {
        return;
    }
    let n = NEXT.fetch_add(1, Ordering::Relaxed);
    let thread = std::thread::current();
    let test = thread.name().unwrap_or("test").rsplit("::").next().unwrap_or("test");
    let end = match listed {
        Listed::All => "",
        Listed::Some => "-some",
        Listed::Cut => "-cut",
    };
    let name = format!("{}-{n}{end}", test.replace('_', "-"));
    let dir = std::path::Path::new(&dir);
    std::fs::create_dir_all(dir).unwrap();
    let mut list = String::new();
    for (path, found) in find {
        let what = match found {
            Found::Dir => String::from("d"),
            Found::Link => String::from("l"),
            Found::File(data) | Found::Stream(data) => {
                let hash = Sha256::digest(data);
                let kind = if matches!(found, Found::File(_)) { 'f' } else { 's' };
                let mut what = format!("{kind} ");
                for b in hash {
                    let _ = write!(what, "{b:02x}");
                }
                what
            }
        };
        list.push_str(&format!("{what}\t{path}\n"));
    }
    std::fs::write(dir.join(format!("{name}.txt")), list).unwrap();
    write_vhd(&dir.join(format!("{name}.vhd")), image, n);
}

/// Where the partition starts on the disk, in bytes, as Windows puts
/// its first.
const START: u64 = 1 << 20;

/// Writes `image` to `path` as a fixed VHD of one MBR partition, of type
/// 7, from [`START`] on: the disk's sectors, then the 512-byte footer.
/// Zeros are left as holes. `serial` tells the disks apart.
fn write_vhd(path: &std::path::Path, image: &[u8], serial: u32) {
    let size = START + image.len() as u64;
    let file = std::fs::File::create(path).unwrap();
    file.set_len(size + 512).unwrap();
    // The master boot record: a disk signature and one partition, its
    // first and last sectors given by number only (CHS 1023/254/63).
    let mut mbr = [0u8; 512];
    mbr[440..444].copy_from_slice(&(0x5244_5800 | serial).to_le_bytes());
    let entry = &mut mbr[446..462];
    entry[1..4].copy_from_slice(&[0xfe, 0xff, 0xff]);
    entry[4] = 0x07;
    entry[5..8].copy_from_slice(&[0xfe, 0xff, 0xff]);
    entry[8..12].copy_from_slice(&((START / 512) as u32).to_le_bytes());
    entry[12..16].copy_from_slice(&((image.len() / 512) as u32).to_le_bytes());
    mbr[510] = 0x55;
    mbr[511] = 0xaa;
    file.write_all_at(&mbr, 0).unwrap();
    for (i, chunk) in image.chunks(64 << 10).enumerate() {
        if chunk.iter().any(|&b| b != 0) {
            file.write_all_at(chunk, START + (i as u64) * (64 << 10)).unwrap();
        }
    }
    file.write_all_at(&footer(size, serial), size).unwrap();
}

/// A fixed VHD's footer, for a disk of `size` bytes.
fn footer(size: u64, serial: u32) -> Vec<u8> {
    let mut f = vec![0u8; 512];
    f[0..8].copy_from_slice(b"conectix");
    // Features: the reserved bit, always set.
    f[8..12].copy_from_slice(&2u32.to_be_bytes());
    f[12..16].copy_from_slice(&0x0001_0000u32.to_be_bytes());
    // A fixed disk has no data offset.
    f[16..24].copy_from_slice(&u64::MAX.to_be_bytes());
    f[28..32].copy_from_slice(b"rdx ");
    f[32..36].copy_from_slice(&0x0001_0000u32.to_be_bytes());
    f[36..40].copy_from_slice(b"Wi2k");
    f[40..48].copy_from_slice(&size.to_be_bytes());
    f[48..56].copy_from_slice(&size.to_be_bytes());
    let (cylinders, heads, sectors) = geometry(size / 512);
    f[56..58].copy_from_slice(&cylinders.to_be_bytes());
    f[58] = heads;
    f[59] = sectors;
    // A fixed hard disk.
    f[60..64].copy_from_slice(&2u32.to_be_bytes());
    f[68..72].copy_from_slice(b"RDX-");
    f[72..76].copy_from_slice(&serial.to_be_bytes());
    let sum = f.iter().fold(0u32, |sum, &b| sum.wrapping_add(u32::from(b)));
    f[64..68].copy_from_slice(&(!sum).to_be_bytes());
    f
}

/// The CHS geometry the specification's appendix gives a disk of
/// `total` sectors.
fn geometry(total: u64) -> (u16, u8, u8) {
    let total = total.min(65535 * 16 * 255);
    let (sectors, heads, cylinders_times_heads) = if total >= 65535 * 16 * 63 {
        (255, 16, total / 255)
    } else {
        let mut sectors = 17;
        let mut cth = total / sectors;
        let mut heads = cth.div_ceil(1024).max(4);
        if cth >= heads * 1024 || heads > 16 {
            sectors = 31;
            heads = 16;
            cth = total / sectors;
        }
        if cth >= heads * 1024 {
            sectors = 63;
            heads = 16;
            cth = total / sectors;
        }
        (sectors, heads, cth)
    };
    ((cylinders_times_heads / heads) as u16, heads as u8, sectors as u8)
}

/// A kept volume is a VHD whose footer adds up, of the disk's size, the
/// partition from [`START`] holding the volume; and QEMU, if here, reads
/// it as that disk.
#[test]
fn a_kept_volume_is_a_vhd() {
    use std::process::Command;

    use crate::tests::{Image, scratch};
    let image = Image::mkntfs(16, 512, 4096).bytes();
    let path = scratch("check.vhd");
    write_vhd(&path, &image, 1);
    let vhd = std::fs::read(&path).unwrap();
    let size = START as usize + image.len();
    assert_eq!(vhd.len(), size + 512);
    let foot = &vhd[size..];
    assert_eq!(&foot[0..8], b"conectix");
    let sum = foot
        .iter()
        .enumerate()
        .filter(|(i, _)| !(64..68).contains(i))
        .fold(0u32, |sum, (_, &b)| sum.wrapping_add(u32::from(b)));
    assert_eq!(u32::from_be_bytes(foot[64..68].try_into().unwrap()), !sum);
    let (c, h, s) = geometry(size as u64 / 512);
    assert!(u64::from(c) * u64::from(h) * u64::from(s) <= size as u64 / 512);
    assert_eq!(&vhd[510..512], &[0x55, 0xaa]);
    assert_eq!(vhd[446 + 4], 0x07);
    assert!(vhd[START as usize..size] == image[..]);
    // QEMU takes a VHD's size from its geometry unless told otherwise.
    let raw = scratch("check.raw");
    let opts = format!("driver=vpc,force_size_calc=current_size,file.filename={}", path.display());
    let read = Command::new("qemu-img")
        .args(["convert", "--image-opts", &opts, "-O", "raw"])
        .arg(&raw)
        .status();
    let _ = std::fs::remove_file(&path);
    if let Ok(status) = read {
        assert!(status.success(), "qemu-img convert");
        let disk = std::fs::read(&raw).unwrap();
        let _ = std::fs::remove_file(&raw);
        assert!(disk[..] == vhd[..size], "QEMU reads the disk as written");
    }
}
