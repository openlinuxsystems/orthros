// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! The boot sector's BIOS parameter block: where everything is.
//!
//! The first sector of a FAT volume says how big its sectors and clusters
//! are, where the tables, the root directory (fixed in size on FAT12 and
//! FAT16, a cluster chain on FAT32) and the data are, and how many
//! clusters there are, which decides between FAT12 and FAT16 (FAT32 says
//! so by where it gives its table's size). Every number is
//! checked before use, as it comes from the disk: a volume that does not
//! add up is refused as [`Error::UnknownFilesystem`] (not FAT) or
//! [`Error::Corrupt`].

use radix_vfs::{Error, Result};

use crate::table::Width;

/// Where a FAT volume's parts are, in bytes from its start.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Geometry {
    /// The table's width.
    pub width: Width,
    /// The size of a sector, as the volume counts them.
    pub sector_size: u32,
    /// The size of a cluster.
    pub cluster_size: u32,
    /// Where the first table starts.
    pub fat_start: u64,
    /// The size of one table.
    pub fat_size: u64,
    /// How many copies of the table there are.
    pub fats: u32,
    /// Where the fixed root directory starts (FAT12 and FAT16).
    pub root_start: u64,
    /// How many entries it has (0 on FAT32).
    pub root_entries: u32,
    /// The root directory's first cluster (FAT32).
    pub root_cluster: u32,
    /// Where cluster 2, the first, starts.
    pub data_start: u64,
    /// How many data clusters there are: clusters 2 to `clusters + 1`.
    pub clusters: u32,
    /// The FSInfo sector's offset (FAT32), if it has one.
    pub fsinfo: Option<u64>,
    /// The volume's serial number.
    pub serial: u32,
    /// The label in the boot sector, padded with spaces.
    pub label: [u8; 11],
}

fn u16_at(b: &[u8], at: usize) -> u32 {
    u32::from(u16::from_le_bytes([b[at], b[at + 1]]))
}

fn u32_at(b: &[u8], at: usize) -> u32 {
    u32::from_le_bytes([b[at], b[at + 1], b[at + 2], b[at + 3]])
}

impl Geometry {
    /// The geometry the boot sector `sector` describes, for a device of
    /// `device_bytes` bytes.
    pub fn parse(sector: &[u8; 512], device_bytes: u64) -> Result<Geometry> {
        let jump = sector[0] == 0xeb && sector[2] == 0x90 || sector[0] == 0xe9;
        if !jump || sector[510] != 0x55 || sector[511] != 0xaa {
            return Err(Error::UnknownFilesystem);
        }
        let sector_size = u16_at(sector, 11);
        let per_cluster = u32::from(sector[13]);
        let reserved = u16_at(sector, 14);
        let fats = u32::from(sector[16]);
        let root_entries = u16_at(sector, 17);
        let total16 = u16_at(sector, 19);
        let fat16 = u16_at(sector, 22);
        let total32 = u32_at(sector, 32);
        let fat32 = u32_at(sector, 36);
        if !matches!(sector_size, 512 | 1024 | 2048 | 4096)
            || !per_cluster.is_power_of_two()
            || per_cluster > 128
            || reserved == 0
            || fats == 0
            || fats > 4
        {
            return Err(Error::UnknownFilesystem);
        }
        let cluster_size = sector_size * per_cluster;
        if cluster_size > 64 * 1024 {
            return Err(Error::UnknownFilesystem);
        }
        let total = u64::from(if total16 != 0 { total16 } else { total32 });
        let fat_sectors = u64::from(if fat16 != 0 { fat16 } else { fat32 });
        let root_sectors = (u64::from(root_entries) * 32).div_ceil(u64::from(sector_size));
        let meta = u64::from(reserved) + u64::from(fats) * fat_sectors + root_sectors;
        if fat_sectors == 0 || total <= meta {
            return Err(Error::Corrupt);
        }
        let ss = u64::from(sector_size);
        if total * ss > device_bytes {
            return Err(Error::Corrupt);
        }
        let clusters = ((total - meta) / u64::from(per_cluster)).min(0x0fff_fff5) as u32;
        // A volume whose table size is only in the FAT32 field is FAT32,
        // whatever its cluster count: `mkfs.fat -F 32` makes small ones,
        // and Linux mounts them. Otherwise the count decides between
        // FAT12 and FAT16, and one too big for FAT16 is refused.
        let width = if fat16 == 0 {
            Width::Fat32
        } else {
            match Width::of(clusters) {
                Width::Fat32 => return Err(Error::Corrupt),
                narrow => narrow,
            }
        };
        // The table must hold an entry for every cluster, and for 0 and 1.
        let needed = crate::table::offset(width, clusters + 1) + crate::table::span(width) as u64;
        if needed > fat_sectors * ss {
            return Err(Error::Corrupt);
        }
        let (root_cluster, fsinfo, serial_at, label_at) = if width == Width::Fat32 {
            if root_entries != 0 {
                return Err(Error::Corrupt);
            }
            let root = u32_at(sector, 44);
            let info = u16_at(sector, 48);
            let fsinfo =
                (info != 0 && info != 0xffff && info < reserved).then_some(u64::from(info) * ss);
            (root, fsinfo, 67, 71)
        } else {
            if root_entries == 0 {
                return Err(Error::Corrupt);
            }
            (0, None, 39, 43)
        };
        if width == Width::Fat32 && !(2..clusters + 2).contains(&root_cluster) {
            return Err(Error::Corrupt);
        }
        // The extended boot signature says the serial is there, and with
        // 0x29 the label too (0x28 is the older form, without it).
        let signature = sector[if width == Width::Fat32 { 66 } else { 38 }];
        let mut label = [b' '; 11];
        let mut serial = 0;
        if matches!(signature, 0x28 | 0x29) {
            serial = u32_at(sector, serial_at);
        }
        if signature == 0x29 {
            label.copy_from_slice(&sector[label_at..label_at + 11]);
        }
        Ok(Geometry {
            width,
            sector_size,
            cluster_size,
            fat_start: u64::from(reserved) * ss,
            fat_size: fat_sectors * ss,
            fats,
            root_start: (u64::from(reserved) + u64::from(fats) * fat_sectors) * ss,
            root_entries,
            root_cluster,
            data_start: meta * ss,
            clusters,
            fsinfo,
            serial,
            label,
        })
    }

    /// Whether `cluster` is a data cluster of the volume.
    pub fn valid(&self, cluster: u32) -> bool {
        cluster >= 2 && cluster - 2 < self.clusters
    }

    /// Where cluster `cluster` starts, in bytes.
    pub fn cluster_offset(&self, cluster: u32) -> u64 {
        self.data_start + u64::from(cluster - 2) * u64::from(self.cluster_size)
    }
}
