// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! The marks by which blkid, the kernel and volumed know a filesystem or
//! a swap area, at fixed places of a device: what a new table or
//! filesystem must wipe where it does not write over them, or the device
//! reads as two things at once, as util-linux's wipefs wipes them.

use crate::device::Device;

/// A mark found.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Signature {
    /// What it marks, as blkid names it: `vfat`, `btrfs`, `swap`.
    pub name: &'static str,
    /// Where it is, in bytes.
    pub offset: u64,
    /// How long it is.
    pub len: usize,
}

/// Where each mark is, and what it is: the boot sector's names of FAT,
/// exFAT, NTFS and BitLocker, Btrfs's superblocks (the three copies),
/// ext2 to ext4's magic, Linux's swap area and ISO 9660's descriptor.
const MARKS: [(&str, u64, &[u8]); 11] = [
    ("exfat", 3, b"EXFAT   "),
    ("ntfs", 3, b"NTFS    "),
    ("BitLocker", 3, b"-FVE-FS-"),
    ("vfat", 54, b"FAT1"),
    ("vfat", 82, b"FAT32   "),
    ("btrfs", (64 << 10) + 64, b"_BHRfS_M"),
    ("btrfs", (64 << 20) + 64, b"_BHRfS_M"),
    ("btrfs", (256 << 30) + 64, b"_BHRfS_M"),
    ("ext4", 1080, &[0x53, 0xef]),
    ("swap", 4086, b"SWAPSPACE2"),
    ("iso9660", 32769, b"CD001"),
];

/// The marks on `device`.
pub fn find(device: &Device) -> Vec<Signature> {
    let mut found = Vec::new();
    let mut first = [0u8; 512];
    let boot = device.read_at(0, &mut first).is_ok() && first[510..512] == [0x55, 0xaa];
    for (name, offset, magic) in MARKS {
        // A boot sector's names count only in a boot sector.
        if offset < 512 && !boot {
            continue;
        }
        let mut bytes = vec![0u8; magic.len()];
        if device.read_at(offset, &mut bytes).is_ok() && bytes == magic {
            found.push(Signature { name, offset, len: magic.len() });
        }
    }
    found
}

/// Zeros each mark on `device` but those of a filesystem named `keep`,
/// which was just made there.
///
/// # Errors
///
/// If one cannot be written.
pub fn wipe(device: &Device, keep: &str) -> std::io::Result<Vec<Signature>> {
    let mut wiped = Vec::new();
    for found in find(device) {
        if found.name == keep {
            continue;
        }
        device.write_at(found.offset, &vec![0u8; found.len])?;
        wiped.push(found);
    }
    Ok(wiped)
}
