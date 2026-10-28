// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! The boot region: where everything is (the specification's "Main and
//! Backup Boot Regions").
//!
//! An exFAT volume starts with twelve sectors of its own sector size: the
//! boot sector, which says how big sectors and clusters are and where the
//! table, the cluster heap and the root directory are; eight extended
//! boot sectors; OEM parameters; a reserved sector; and a sector of
//! checksums, each the checksum of the eleven before it. A backup of the
//! region follows. The checksum leaves out the volume flags and the
//! percentage in use, which change while the volume is used, so writing
//! them needs no new checksum; nothing else in the region ever changes.
//! Every number is checked before use, as it comes from the disk: a
//! volume that is not exFAT is refused as [`Error::UnknownFilesystem`],
//! and one whose numbers do not add up as [`Error::Corrupt`].

use radix_vfs::{Error, Result};

/// The bytes of the boot sector that say a volume is exFAT.
const NAME: &[u8; 8] = b"EXFAT   ";

/// Where the volume flags are in the boot sector.
pub const FLAGS_AT: u64 = 106;

/// Bits of the volume flags.
pub mod flags {
    /// The second table and bitmap are the ones in use (TexFAT).
    pub const ACTIVE_FAT: u16 = 1 << 0;
    /// The volume may not be consistent: it was not cleanly unmounted.
    pub const DIRTY: u16 = 1 << 1;
    /// The medium reported a failure.
    pub const MEDIA_FAILURE: u16 = 1 << 2;
}

/// The sectors of a boot region, the checksum's last.
pub const REGION_SECTORS: u64 = 12;

/// Where an exFAT volume's parts are, in bytes from its start.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Geometry {
    /// The size of a sector, from 512 bytes to 4 KiB.
    pub sector_size: u32,
    /// The size of a cluster, from a sector to 32 MiB.
    pub cluster_size: u32,
    /// Where the first table starts.
    pub fat_start: u64,
    /// The size of one table.
    pub fat_size: u64,
    /// How many tables (and bitmaps) there are: 1, or 2 for TexFAT.
    pub fats: u8,
    /// Where cluster 2, the first, starts.
    pub heap_start: u64,
    /// How many clusters there are: clusters 2 to `clusters + 1`.
    pub clusters: u32,
    /// The root directory's first cluster.
    pub root_cluster: u32,
    /// The volume's serial number.
    pub serial: u32,
    /// The volume flags as the boot sector had them.
    pub flags: u16,
}

fn u16_at(b: &[u8], at: usize) -> u16 {
    u16::from_le_bytes([b[at], b[at + 1]])
}

fn u32_at(b: &[u8], at: usize) -> u32 {
    u32::from_le_bytes([b[at], b[at + 1], b[at + 2], b[at + 3]])
}

fn u64_at(b: &[u8], at: usize) -> u64 {
    let mut bytes = [0u8; 8];
    bytes.copy_from_slice(&b[at..at + 8]);
    u64::from_le_bytes(bytes)
}

impl Geometry {
    /// The geometry the boot sector's first 512 bytes `sector` describe,
    /// for a device of `device_bytes` bytes.
    pub fn parse(sector: &[u8; 512], device_bytes: u64) -> Result<Geometry> {
        if sector[0..3] != [0xeb, 0x76, 0x90]
            || &sector[3..11] != NAME
            || sector[510] != 0x55
            || sector[511] != 0xaa
        {
            return Err(Error::UnknownFilesystem);
        }
        // Where FAT keeps its parameters, exFAT must have zeros, so that
        // no FAT driver takes it for FAT.
        if sector[11..64].iter().any(|&b| b != 0) {
            return Err(Error::Corrupt);
        }
        let volume_sectors = u64_at(sector, 72);
        let fat_offset = u32_at(sector, 80);
        let fat_length = u32_at(sector, 84);
        let heap_offset = u32_at(sector, 88);
        let clusters = u32_at(sector, 92);
        let root_cluster = u32_at(sector, 96);
        let serial = u32_at(sector, 100);
        let revision = u16_at(sector, 104);
        let flags = u16_at(sector, 106);
        let sector_shift = u32::from(sector[108]);
        let cluster_shift = u32::from(sector[109]);
        let fats = sector[110];
        // A later major revision may change the format: the specification
        // says not to mount it.
        if revision >> 8 != 1 {
            return Err(Error::NotSupported);
        }
        if !(9..=12).contains(&sector_shift)
            || cluster_shift > 25 - sector_shift
            || !matches!(fats, 1 | 2)
        {
            return Err(Error::Corrupt);
        }
        let ss = 1u64 << sector_shift;
        let per_cluster = 1u64 << cluster_shift;
        // The table follows the boot regions, holds an entry for every
        // cluster (and for 0 and 1), and the heap follows the tables; the
        // heap's clusters fit in the volume, and the volume on the device.
        let table_bytes = (u64::from(clusters) + 2) * 4;
        let fats_end = u64::from(fat_offset) + u64::from(fat_length) * u64::from(fats);
        let heap_end = u64::from(heap_offset) + u64::from(clusters) * per_cluster;
        if u64::from(fat_offset) < 2 * REGION_SECTORS
            || u64::from(fat_length) * ss < table_bytes
            || u64::from(heap_offset) < fats_end
            || clusters == 0
            || clusters > 0xffff_fff5
            || heap_end > volume_sectors
            || volume_sectors.checked_mul(ss).is_none_or(|bytes| bytes > device_bytes)
            || !(2..u64::from(clusters) + 2).contains(&u64::from(root_cluster))
        {
            return Err(Error::Corrupt);
        }
        Ok(Geometry {
            sector_size: ss as u32,
            cluster_size: (ss * per_cluster) as u32,
            fat_start: u64::from(fat_offset) * ss,
            fat_size: u64::from(fat_length) * ss,
            fats,
            heap_start: u64::from(heap_offset) * ss,
            clusters,
            root_cluster,
            serial,
            flags,
        })
    }

    /// Whether `cluster` is a cluster of the heap.
    pub fn valid(&self, cluster: u32) -> bool {
        (2..self.clusters.saturating_add(2)).contains(&cluster)
    }

    /// Where `cluster` starts on the device.
    pub fn cluster_offset(&self, cluster: u32) -> u64 {
        self.heap_start + u64::from(cluster - 2) * u64::from(self.cluster_size)
    }
}

/// Adds `bytes`, the bytes from `at` of a boot region, to the region's
/// checksum `sum`: the volume flags (106 and 107) and the percentage in
/// use (112) of the boot sector are left out.
pub fn checksum(mut sum: u32, at: u64, bytes: &[u8]) -> u32 {
    for (i, &byte) in bytes.iter().enumerate() {
        if matches!(at + i as u64, 106 | 107 | 112) {
            continue;
        }
        sum = sum.rotate_right(1).wrapping_add(u32::from(byte));
    }
    sum
}

#[cfg(kani)]
mod proofs {
    use super::*;

    /// Whatever its boot sector says, a volume that mounts keeps its
    /// tables before its heap, an entry in them for every cluster, and
    /// every cluster on the device.
    #[kani::proof]
    #[kani::unwind(60)]
    fn clusters_stay_on_the_device() {
        let sector: [u8; 512] = kani::any();
        let device: u64 = kani::any();
        if let Ok(geo) = Geometry::parse(&sector, device) {
            assert!(geo.fat_start + geo.fat_size * u64::from(geo.fats) <= geo.heap_start);
            assert!((u64::from(geo.clusters) + 2) * 4 <= geo.fat_size);
            let cluster: u32 = kani::any();
            if geo.valid(cluster) {
                assert!(geo.cluster_offset(cluster) + u64::from(geo.cluster_size) <= device);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The checksum is the specification's: each byte added to the sum
    /// turned right by a bit, with the flags left out.
    #[test]
    fn checksum_skips_the_flags() {
        let mut region = [0u8; 120];
        region[0] = 1;
        region[106] = 0xff;
        region[107] = 0xff;
        region[112] = 0x42;
        let mut sum = 0u32;
        for (i, &b) in region.iter().enumerate() {
            if !matches!(i, 106 | 107 | 112) {
                sum = (if sum & 1 != 0 { 0x8000_0000 } else { 0 }) + (sum >> 1) + u32::from(b);
            }
        }
        assert_eq!(checksum(0, 0, &region), sum);
        // In pieces, as the mount reads it.
        let half = checksum(0, 0, &region[..100]);
        assert_eq!(checksum(half, 100, &region[100..]), sum);
    }
}
