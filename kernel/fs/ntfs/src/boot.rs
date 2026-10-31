// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! The boot sector: how big sectors, clusters and records are, and where
//! the master file table is.
//!
//! An NTFS volume's first sector holds a BIOS parameter block much like
//! FAT's, with the FAT fields zero, and NTFS's own after it: the volume's
//! size in sectors, the clusters of the master file table (`$MFT`) and of
//! its mirror, the size of a file record and of an index block, and the
//! serial number. A size of a record or an index block is in clusters
//! when positive, and when negative, -n, is 2^n bytes, which is how
//! records smaller than a cluster are given. A copy of the sector is kept
//! in the volume's last sector. Every number is checked before use, as
//! it comes from the disk: a volume that is not NTFS is refused as
//! [`Error::UnknownFilesystem`], and one whose numbers do not add up as
//! [`Error::Corrupt`].

use radix_vfs::{Error, Result};

/// The bytes of the boot sector that say a volume is NTFS.
const NAME: &[u8; 8] = b"NTFS    ";

/// The largest cluster Windows makes: 2 MiB.
pub const MAX_CLUSTER: u32 = 2 << 20;

/// The largest file record or index block Radix reads: 64 KiB. Windows
/// makes records of 1 KiB, or 4 KiB on disks of 4 KiB sectors, and index
/// blocks of 4 KiB.
pub const MAX_RECORD: u32 = 64 << 10;

/// Where an NTFS volume's parts are.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Geometry {
    /// The size of a sector, from 512 bytes to 4 KiB.
    pub sector_size: u32,
    /// The size of a cluster, from a sector to [`MAX_CLUSTER`].
    pub cluster_size: u32,
    /// How many clusters the volume has.
    pub clusters: u64,
    /// The first cluster of the master file table.
    pub mft: u64,
    /// The first cluster of its mirror, which copies its first records.
    pub mft_mirror: u64,
    /// The size of a file record.
    pub record_size: u32,
    /// The size of a directory's index block.
    pub index_size: u32,
    /// The volume's serial number.
    pub serial: u64,
}

/// The size a record or index block field `value` gives, with clusters of
/// `cluster` bytes.
fn size_of(value: u8, cluster: u32) -> Option<u32> {
    let size = match value as i8 {
        n @ 1.. => cluster.checked_mul(n as u32)?,
        n @ -31..=-1 => 1u32 << -n,
        _ => return None,
    };
    (size.is_power_of_two() && (512..=MAX_RECORD).contains(&size)).then_some(size)
}

fn u16_at(b: &[u8], at: usize) -> u16 {
    u16::from_le_bytes([b[at], b[at + 1]])
}

fn u64_at(b: &[u8], at: usize) -> u64 {
    let mut bytes = [0u8; 8];
    bytes.copy_from_slice(&b[at..at + 8]);
    u64::from_le_bytes(bytes)
}

impl Geometry {
    /// Reads the boot sector `boot`, of a volume on a device of `bytes`
    /// bytes.
    pub fn parse(boot: &[u8; 512], bytes: u64) -> Result<Geometry> {
        if &boot[3..11] != NAME || boot[510..512] != [0x55, 0xaa] {
            return Err(Error::UnknownFilesystem);
        }
        let sector_size = u32::from(u16_at(boot, 11));
        if !sector_size.is_power_of_two() || !(512..=4096).contains(&sector_size) {
            return Err(Error::Corrupt);
        }
        // Sectors per cluster: up to 128 as they are, and above, as 2^n
        // for a byte of -n, as Windows gives clusters of 128 KiB and more.
        let per_cluster = match boot[13] {
            n @ 1..=0x80 => u32::from(n),
            n @ 0xf4..=0xff => 1 << (256 - u32::from(n)),
            _ => return Err(Error::Corrupt),
        };
        let cluster_size = sector_size.checked_mul(per_cluster).ok_or(Error::Corrupt)?;
        if !per_cluster.is_power_of_two() || cluster_size > MAX_CLUSTER {
            return Err(Error::Corrupt);
        }
        let sectors = u64_at(boot, 40);
        // The volume, and the copy of its boot sector after it, fit the
        // device.
        let volume_bytes = sectors.checked_mul(u64::from(sector_size)).ok_or(Error::Corrupt)?;
        if volume_bytes > bytes {
            return Err(Error::Corrupt);
        }
        let clusters = sectors / u64::from(per_cluster);
        let mft = u64_at(boot, 48);
        let mft_mirror = u64_at(boot, 56);
        let record_size = size_of(boot[64], cluster_size).ok_or(Error::Corrupt)?;
        let index_size = size_of(boot[68], cluster_size).ok_or(Error::Corrupt)?;
        // A record is read in sectors, and its first records must lie in
        // the volume.
        let first_records = (u64::from(record_size) * 16).div_ceil(u64::from(cluster_size));
        let mft_end = mft.checked_add(first_records);
        if record_size < sector_size || clusters == 0 || mft_end.is_none_or(|end| end > clusters) {
            return Err(Error::Corrupt);
        }
        Ok(Geometry {
            sector_size,
            cluster_size,
            clusters,
            mft,
            mft_mirror,
            record_size,
            index_size,
            serial: u64_at(boot, 72),
        })
    }

    /// The byte offset of cluster `lcn`.
    pub fn cluster_offset(&self, lcn: u64) -> u64 {
        lcn * u64::from(self.cluster_size)
    }

    /// The size of the units an index block's number counts: a cluster,
    /// or 512 bytes when blocks are smaller than clusters.
    pub fn index_vcn_size(&self) -> u32 {
        if self.index_size >= self.cluster_size { self.cluster_size } else { 512 }
    }
}

#[cfg(kani)]
mod proofs {
    use super::*;

    /// Whatever its boot sector says, a volume that mounts has every
    /// cluster on the device, the master file table's first cluster among
    /// them, and records of whole sectors.
    #[kani::proof]
    fn clusters_stay_on_the_device() {
        let sector: [u8; 512] = kani::any();
        let device: u64 = kani::any();
        if let Ok(geo) = Geometry::parse(&sector, device) {
            assert!(geo.cluster_offset(geo.clusters) <= device);
            assert!(geo.mft < geo.clusters);
            assert!(geo.record_size % geo.sector_size == 0);
            assert!(geo.index_vcn_size() <= geo.cluster_size);
        }
    }
}
