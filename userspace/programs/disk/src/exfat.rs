// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! exFAT, made as Microsoft's specification lays it out ("exFAT file
//! system specification", 2019): the main and backup boot regions of 12
//! sectors each with their checksums, one allocation table, and in the
//! cluster heap the allocation bitmap, the up-case table and the root
//! directory, which names them and holds the volume's label.
//!
//! Cluster sizes are Windows' defaults: 4 KiB up to 256 MiB, 32 KiB up
//! to 32 GiB, and 128 KiB above. The table and the heap start on whole
//! MiB, as SD cards and exfatprogs have them, so clusters fall on the
//! boundaries of the flash beneath.

use std::fmt;

use crate::device::Device;
use crate::rng::Rng;
use crate::signatures;

/// What to make.
#[derive(Clone, Debug, Default)]
pub struct Options {
    /// The size of a cluster, or Windows' default for the volume's size.
    pub cluster_bytes: Option<u32>,
    /// The volume's label: up to 11 UTF-16 units.
    pub label: Option<String>,
    /// Its serial number, or a random one.
    pub volume_id: Option<u32>,
    /// Where the volume starts on its disk, in its sectors (the boot
    /// sector's PartitionOffset).
    pub hidden: Option<u64>,
}

/// What was made.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Made {
    /// The size of its clusters.
    pub cluster_bytes: u32,
    /// How many clusters its heap has.
    pub clusters: u32,
    /// Its serial number.
    pub volume_id: u32,
    /// Its label.
    pub label: Option<String>,
    /// The up-case table's checksum, as its directory entry gives it.
    pub upcase_checksum: u32,
}

impl fmt::Display for Made {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "exFAT, {} clusters of {}, serial {}",
            self.clusters,
            crate::size::human(u64::from(self.cluster_bytes)),
            crate::fat::volume_id_text(self.volume_id)
        )?;
        if let Some(label) = &self.label {
            write!(f, ", label {label}")?;
        }
        Ok(())
    }
}

/// The checksum of the boot region's first 11 sectors, which its 12th
/// repeats: every byte but the volume flags and the percent in use, which
/// change as the volume is used.
pub fn boot_checksum(sectors: &[u8], sector: usize) -> u32 {
    let mut sum = 0u32;
    for (i, &b) in sectors[..11 * sector].iter().enumerate() {
        if matches!(i, 106 | 107 | 112) {
            continue;
        }
        sum = sum.rotate_right(1).wrapping_add(u32::from(b));
    }
    sum
}

/// The checksum of the up-case table's bytes.
pub fn table_checksum(bytes: &[u8]) -> u32 {
    bytes.iter().fold(0u32, |sum, &b| sum.rotate_right(1).wrapping_add(u32::from(b)))
}

/// The up-case table, compressed as the specification allows: each
/// UTF-16 unit's upper case, and 0xFFFF and a count for a run of units
/// that are their own. The upper cases are Unicode's, as Rust's
/// `to_uppercase` gives them, where that is one unit; surrogates and
/// units whose upper case is longer are their own.
pub fn upcase_table() -> Vec<u8> {
    let upper = |u: u32| -> u32 {
        let Some(c) = char::from_u32(u) else { return u };
        let mut it = c.to_uppercase();
        match (it.next(), it.next()) {
            (Some(up), None) if (up as u32) <= 0xffff => up as u32,
            _ => u,
        }
    };
    let mut units: Vec<u16> = Vec::new();
    let mut u = 0u32;
    while u <= 0xffff {
        if upper(u) == u {
            let start = u;
            while u <= 0xffff && upper(u) == u {
                u += 1;
            }
            let run = u - start;
            // A short run costs less written out; 0xFFFF itself never is.
            if run > 2 || u > 0xffff {
                units.push(0xffff);
                units.push(run as u16);
            } else {
                units.extend((start..u).map(|v| v as u16));
            }
        } else {
            units.push(upper(u) as u16);
            u += 1;
        }
    }
    units.iter().flat_map(|u| u.to_le_bytes()).collect()
}

/// Windows' default cluster for a volume of `bytes`.
fn default_cluster(bytes: u64) -> u32 {
    if bytes <= 256 << 20 {
        4 << 10
    } else if bytes <= 32 << 30 {
        32 << 10
    } else {
        128 << 10
    }
}

/// The label `text` in UTF-16, as its entry holds it.
///
/// # Errors
///
/// If it has more than 11 units, or a character a name may not hold.
pub fn label_units(text: &str) -> Result<Vec<u16>, String> {
    if let Some(c) = text.chars().find(|&c| c < ' ' || "\"*/:<>?\\|".contains(c)) {
        return Err(format!("an exFAT label may not hold {c:?}"));
    }
    let units: Vec<u16> = text.encode_utf16().collect();
    if units.len() > 11 {
        return Err(format!("an exFAT label holds at most 11 characters, and \"{text}\" has more"));
    }
    Ok(units)
}

/// Makes an exFAT filesystem on `device`, as `options` ask.
///
/// # Errors
///
/// In words, if the volume cannot have what was asked, or it cannot be
/// written.
pub fn format(device: &Device, options: &Options, rng: &mut Rng) -> Result<Made, String> {
    let sector = device.sector_size();
    let s = sector as usize;
    let total = device.sectors();
    let bytes = total * u64::from(sector);
    let cluster = options.cluster_bytes.unwrap_or_else(|| default_cluster(bytes));
    if !cluster.is_power_of_two() || cluster < sector || cluster > 32 << 20 {
        return Err(format!(
            "a cluster is a power of two from a sector ({sector} bytes) to 32 MiB, not {cluster} bytes"
        ));
    }
    let label = options.label.as_deref().map(label_units).transpose()?;
    let per_cluster = u64::from(cluster / sector);
    // The table and the heap start on whole MiB, or on whole clusters on
    // a volume too small to give up two MiB.
    let boundary = if bytes >= 64 << 20 { ((1u64 << 20) / u64::from(sector)).max(per_cluster) } else { per_cluster };
    let fat_offset = 24u64.div_ceil(boundary) * boundary;
    let mut fat_length = 1u64;
    let (heap_offset, clusters) = loop {
        let heap = (fat_offset + fat_length).div_ceil(boundary) * boundary;
        if heap >= total {
            return Err(format!("{} is too small for exFAT", crate::size::human(bytes)));
        }
        let clusters = ((total - heap) / per_cluster).min(0xffff_fff5);
        let needed = ((clusters + 2) * 4).div_ceil(u64::from(sector));
        if needed <= fat_length {
            break (heap, clusters);
        }
        fat_length = needed;
    };
    if clusters < 3 + 2 {
        return Err(format!("{} is too small for exFAT", crate::size::human(bytes)));
    }
    let clusters32 = clusters as u32;
    let volume_id = options.volume_id.unwrap_or_else(|| rng.next_u32());
    let hidden = options.hidden.unwrap_or(device.base() / u64::from(sector));

    // The heap's first clusters: the bitmap, the up-case table and the
    // root directory, each a chain of its own.
    let cluster64 = u64::from(cluster);
    let bitmap_bytes = clusters.div_ceil(8);
    let bitmap_clusters = bitmap_bytes.div_ceil(cluster64);
    let upcase = upcase_table();
    let upcase_clusters = (upcase.len() as u64).div_ceil(cluster64);
    let bitmap_first = 2u64;
    let upcase_first = bitmap_first + bitmap_clusters;
    let root_first = upcase_first + upcase_clusters;
    let used = bitmap_clusters + upcase_clusters + 1;
    if used + 2 > clusters {
        return Err(format!("{} is too small for exFAT", crate::size::human(bytes)));
    }

    let mut boot = vec![0u8; s];
    boot[0..3].copy_from_slice(&[0xeb, 0x76, 0x90]);
    boot[3..11].copy_from_slice(b"EXFAT   ");
    boot[64..72].copy_from_slice(&hidden.to_le_bytes());
    boot[72..80].copy_from_slice(&total.to_le_bytes());
    boot[80..84].copy_from_slice(&(fat_offset as u32).to_le_bytes());
    boot[84..88].copy_from_slice(&(fat_length as u32).to_le_bytes());
    boot[88..92].copy_from_slice(&(heap_offset as u32).to_le_bytes());
    boot[92..96].copy_from_slice(&clusters32.to_le_bytes());
    boot[96..100].copy_from_slice(&(root_first as u32).to_le_bytes());
    boot[100..104].copy_from_slice(&volume_id.to_le_bytes());
    boot[104..106].copy_from_slice(&0x0100u16.to_le_bytes());
    boot[108] = sector.trailing_zeros() as u8;
    boot[109] = (cluster / sector).trailing_zeros() as u8;
    boot[110] = 1;
    boot[111] = 0x80;
    boot[112] = (used * 100 / clusters) as u8;
    // Boot code that halts: nothing boots from it.
    boot[120..123].copy_from_slice(&[0xf4, 0xeb, 0xfd]);
    boot[510] = 0x55;
    boot[511] = 0xaa;
    let mut region = vec![0u8; 12 * s];
    region[..s].copy_from_slice(&boot);
    for n in 1..=8 {
        region[(n + 1) * s - 4..(n + 1) * s].copy_from_slice(&0xaa55_0000u32.to_le_bytes());
    }
    let sum = boot_checksum(&region, s);
    for word in region[11 * s..].as_chunks_mut::<4>().0 {
        *word = sum.to_le_bytes();
    }

    let write = |offset: u64, bytes: &[u8]| {
        device.write_at(offset, bytes).map_err(|e| format!("cannot write the volume: {e}"))
    };
    let sector64 = u64::from(sector);
    // Everything before the heap, and the heap's first clusters, start
    // as zeros.
    device
        .zero(0, heap_offset * sector64)
        .and_then(|()| device.zero(heap_offset * sector64, used * cluster64))
        .map_err(|e| format!("cannot write the volume: {e}"))?;
    // The backup first, then the main region, as a write cut short then
    // leaves no main region that checks without a table.
    write(12 * sector64, &region)?;

    let mut fat = vec![0u8; ((root_first + 1) * 4) as usize];
    let mut set = |n: u64, v: u32| fat[(n * 4) as usize..(n * 4 + 4) as usize].copy_from_slice(&v.to_le_bytes());
    set(0, 0xffff_fff8);
    set(1, 0xffff_ffff);
    for (first, count) in [(bitmap_first, bitmap_clusters), (upcase_first, upcase_clusters), (root_first, 1)] {
        for n in first..first + count {
            set(n, if n + 1 == first + count { 0xffff_ffff } else { (n + 1) as u32 });
        }
    }
    write(fat_offset * sector64, &fat)?;

    let heap = |n: u64| heap_offset * sector64 + (n - 2) * cluster64;
    let mut bitmap = vec![0u8; bitmap_bytes as usize];
    for n in 0..used {
        bitmap[(n / 8) as usize] |= 1 << (n % 8);
    }
    write(heap(bitmap_first), &bitmap)?;
    write(heap(upcase_first), &upcase)?;
    let checksum = table_checksum(&upcase);
    let mut root = vec![0u8; 32 * 3];
    let mut at = 0;
    if let Some(units) = &label {
        root[0] = 0x83;
        root[1] = units.len() as u8;
        for (i, u) in units.iter().enumerate() {
            root[2 + 2 * i..4 + 2 * i].copy_from_slice(&u.to_le_bytes());
        }
        at = 32;
    }
    root[at] = 0x81;
    root[at + 20..at + 24].copy_from_slice(&(bitmap_first as u32).to_le_bytes());
    root[at + 24..at + 32].copy_from_slice(&bitmap_bytes.to_le_bytes());
    at += 32;
    root[at] = 0x82;
    root[at + 4..at + 8].copy_from_slice(&checksum.to_le_bytes());
    root[at + 20..at + 24].copy_from_slice(&(upcase_first as u32).to_le_bytes());
    root[at + 24..at + 32].copy_from_slice(&(upcase.len() as u64).to_le_bytes());
    write(heap(root_first), &root)?;
    device.sync().map_err(|e| format!("cannot write the volume: {e}"))?;
    write(0, &region)?;
    signatures::wipe(device, "exfat").map_err(|e| format!("cannot write the volume: {e}"))?;
    device.sync().map_err(|e| format!("cannot write the volume: {e}"))?;
    Ok(Made {
        cluster_bytes: cluster,
        clusters: clusters32,
        volume_id,
        label: options.label.clone(),
        upcase_checksum: checksum,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_upcase_table_maps_what_it_should() {
        let table = upcase_table();
        let units: Vec<u16> = table.as_chunks::<2>().0.iter().map(|&c| u16::from_le_bytes(c)).collect();
        let mut map = Vec::new();
        let mut i = 0;
        while i < units.len() {
            if units[i] == 0xffff {
                let run = units[i + 1];
                for _ in 0..run {
                    map.push(map.len() as u16);
                }
                i += 2;
            } else {
                map.push(units[i]);
                i += 1;
            }
        }
        assert_eq!(map.len(), 65536);
        assert_eq!(map[usize::from(b'a')], u16::from(b'A'));
        assert_eq!(map[usize::from(b'Z')], u16::from(b'Z'));
        assert_eq!(map[0xe9], 0xc9);
        assert_eq!(map[0x3c9], 0x3a9);
        assert_eq!(map[0xdf], 0xdf);
        assert!(table.len() < 8192, "{}", table.len());
    }

    #[test]
    fn labels() {
        assert_eq!(label_units("Stick"), Ok("Stick".encode_utf16().collect()));
        assert!(label_units("twelve chars").is_err());
        assert!(label_units("a/b").is_err());
    }
}
