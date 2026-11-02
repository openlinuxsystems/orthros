// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! FAT12, FAT16 and FAT32, made as Microsoft's specification lays them
//! out ("FAT: General Overview of On-Disk Format", version 1.03): the
//! boot sector and its parameters, FAT32's FSInfo sector and backup boot
//! sector, the allocation tables, and an empty root directory, with the
//! volume's label in it.
//!
//! Sizes follow the specification's tables: FAT12 for volumes of up to
//! 4.1 MB, FAT16 below 512 MiB and FAT32 from there, as dosfstools
//! chooses, and the cluster size its tables give each. The data area
//! starts on a whole cluster, so clusters fall on the boundaries of the
//! pages and erase blocks under a partition that starts on a whole MiB.
//! [`Bpb::parse`] reads what [`format`] writes, for fsck-fat(8).

use std::fmt;

use crate::device::Device;
use crate::rng::Rng;
use crate::signatures;

/// The parameters of a FAT volume, as its boot sector gives them.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Bpb {
    /// The size of a sector: 512 to 4096 bytes.
    pub sector_size: u32,
    /// The sectors of a cluster.
    pub cluster_sectors: u32,
    /// The sectors before the first allocation table.
    pub reserved: u32,
    /// How many allocation tables.
    pub fats: u32,
    /// FAT12 and FAT16's root directory entries; 0 for FAT32.
    pub root_entries: u32,
    /// The sectors of the volume.
    pub total_sectors: u64,
    /// The sectors of each allocation table.
    pub fat_sectors: u32,
    /// The media byte: 0xF8 for a fixed disk.
    pub media: u8,
    /// Where the volume says it starts on its disk.
    pub hidden: u32,
    /// FAT32's first cluster of the root directory.
    pub root_cluster: u32,
    /// FAT32's FSInfo sector.
    pub fsinfo: u32,
    /// FAT32's backup boot sector.
    pub backup_boot: u32,
    /// FAT32's flags: whether the tables are mirrored, and which is
    /// active if not.
    pub ext_flags: u16,
    /// Its serial number.
    pub volume_id: u32,
    /// The label in the boot sector: 11 bytes, `NO NAME    ` for none.
    pub label: [u8; 11],
    /// 12, 16 or 32, by the count of clusters, as the specification has
    /// it decided.
    pub bits: u8,
    /// How many data clusters there are: 2 to `clusters + 1`.
    pub clusters: u32,
}

impl Bpb {
    /// The sectors of FAT12 and FAT16's root directory.
    pub fn root_sectors(&self) -> u32 {
        (self.root_entries * 32).div_ceil(self.sector_size)
    }

    /// The first sector of the data area: cluster 2's.
    pub fn first_data(&self) -> u64 {
        u64::from(self.reserved)
            + u64::from(self.fats) * u64::from(self.fat_sectors)
            + u64::from(self.root_sectors())
    }

    /// The bytes of a cluster.
    pub fn cluster_bytes(&self) -> u32 {
        self.cluster_sectors * self.sector_size
    }

    /// The byte where cluster `n` starts.
    pub fn cluster_offset(&self, n: u32) -> u64 {
        (self.first_data() + u64::from(n - 2) * u64::from(self.cluster_sectors))
            * u64::from(self.sector_size)
    }

    /// The parameters in `boot`, a volume's first sector.
    ///
    /// # Errors
    ///
    /// In words, if it is not a FAT boot sector, or its parameters do not
    /// describe a volume that can be.
    pub fn parse(boot: &[u8]) -> Result<Bpb, String> {
        if boot.len() < 512 {
            return Err("the boot sector is short".to_string());
        }
        let u16_at = |at: usize| u32::from(u16::from_le_bytes([boot[at], boot[at + 1]]));
        let u32_at =
            |at: usize| u32::from_le_bytes([boot[at], boot[at + 1], boot[at + 2], boot[at + 3]]);
        if boot[510..512] != [0x55, 0xaa] {
            return Err("the boot sector does not end with 0x55 0xAA".to_string());
        }
        if !matches!(boot[0], 0xeb | 0xe9) {
            return Err("the boot sector does not start with a jump".to_string());
        }
        let sector_size = u16_at(11);
        if !matches!(sector_size, 512 | 1024 | 2048 | 4096) {
            return Err(format!("its sectors are of {sector_size} bytes"));
        }
        let cluster_sectors = u32::from(boot[13]);
        if !cluster_sectors.is_power_of_two() || cluster_sectors > 128 {
            return Err(format!("its clusters are of {cluster_sectors} sectors"));
        }
        if cluster_sectors * sector_size > 256 << 10 {
            return Err("its clusters are larger than 256 KiB".to_string());
        }
        let reserved = u16_at(14);
        let fats = u32::from(boot[16]);
        if reserved == 0 || fats == 0 {
            return Err("it has no reserved sectors, or no allocation table".to_string());
        }
        let root_entries = u16_at(17);
        let total16 = u16_at(19);
        let total_sectors = if total16 != 0 { u64::from(total16) } else { u64::from(u32_at(32)) };
        let fat16 = u16_at(22);
        let fat_sectors = if fat16 != 0 { fat16 } else { u32_at(36) };
        if fat_sectors == 0 {
            return Err("its allocation tables have no sectors".to_string());
        }
        let mut bpb = Bpb {
            sector_size,
            cluster_sectors,
            reserved,
            fats,
            root_entries,
            total_sectors,
            fat_sectors,
            media: boot[21],
            hidden: u32_at(28),
            root_cluster: 0,
            fsinfo: 0,
            backup_boot: 0,
            ext_flags: 0,
            volume_id: 0,
            label: [b' '; 11],
            bits: 0,
            clusters: 0,
        };
        let first_data = bpb.first_data();
        if first_data >= total_sectors {
            return Err("its tables leave no room for data".to_string());
        }
        let clusters = (total_sectors - first_data) / u64::from(cluster_sectors);
        bpb.clusters =
            u32::try_from(clusters).map_err(|_| "it has too many clusters".to_string())?;
        bpb.bits = match clusters {
            0..4085 => 12,
            4085..65525 => 16,
            _ => 32,
        };
        let ebpb = if bpb.bits == 32 {
            if fat16 != 0 || root_entries != 0 || total16 != 0 {
                return Err("it has FAT32's clusters but FAT16's fields".to_string());
            }
            bpb.ext_flags = u16_at(40) as u16;
            bpb.root_cluster = u32_at(44);
            bpb.fsinfo = u16_at(48);
            bpb.backup_boot = u16_at(50);
            if bpb.clusters > MAX_FAT32_CLUSTERS {
                return Err("it has more clusters than FAT32 counts".to_string());
            }
            64
        } else {
            if root_entries == 0 {
                return Err("it has no root directory".to_string());
            }
            36
        };
        if boot[ebpb + 2] == 0x29 {
            bpb.volume_id = u32_at(ebpb + 3);
            bpb.label.copy_from_slice(&boot[ebpb + 7..ebpb + 18]);
        }
        let needed = (u64::from(bpb.clusters) + 2) * u64::from(bpb.bits) / 8;
        if u64::from(fat_sectors) * u64::from(sector_size) < needed {
            return Err("its allocation tables are too small for its clusters".to_string());
        }
        Ok(bpb)
    }
}

/// The most clusters FAT32 counts: its entries hold 28 bits, of which
/// 0x0FFFFFF7 is a bad cluster and those above the end of a chain.
pub const MAX_FAT32_CLUSTERS: u32 = 0x0fff_fff5;

/// What to make.
#[derive(Clone, Debug, Default)]
pub struct Options {
    /// FAT12, FAT16 or FAT32, or by the volume's size.
    pub bits: Option<u8>,
    /// The size of a cluster, or by the specification's tables.
    pub cluster_bytes: Option<u32>,
    /// The volume's label: up to 11 characters, kept in upper case.
    pub label: Option<String>,
    /// Its serial number, or a random one.
    pub volume_id: Option<u32>,
    /// How many allocation tables: 2, or 1.
    pub fats: Option<u8>,
    /// FAT12 and FAT16's root directory entries: 512.
    pub root_entries: Option<u32>,
    /// Where the volume starts on its disk, in its sectors, for the boot
    /// sector's "hidden sectors": the partition's first sector.
    pub hidden: Option<u64>,
    /// When it is made, in seconds since 1970 (UTC), for the label's
    /// entry; now if not given.
    pub now: Option<u64>,
}

/// What was made.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Made {
    /// 12, 16 or 32.
    pub bits: u8,
    /// The size of its sectors.
    pub sector_size: u32,
    /// The size of its clusters.
    pub cluster_bytes: u32,
    /// How many data clusters it has.
    pub clusters: u32,
    /// The sectors of each allocation table.
    pub fat_sectors: u32,
    /// Its reserved sectors, before the first table.
    pub reserved: u32,
    /// Its serial number.
    pub volume_id: u32,
    /// Its label, in upper case, as it was written.
    pub label: Option<String>,
}

impl fmt::Display for Made {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "FAT{}, {} clusters of {}, serial {}",
            self.bits,
            self.clusters,
            crate::size::human(u64::from(self.cluster_bytes)),
            volume_id_text(self.volume_id)
        )?;
        if let Some(label) = &self.label {
            write!(f, ", label {label}")?;
        }
        Ok(())
    }
}

/// A serial number as Windows and blkid write a FAT one: `1234-ABCD`.
pub fn volume_id_text(id: u32) -> String {
    format!("{:04X}-{:04X}", id >> 16, id & 0xffff)
}

/// The label `text` as a FAT volume keeps it, 11 bytes padded with
/// spaces, in upper case.
///
/// # Errors
///
/// If it is longer, or holds a character a short name may not.
pub fn label_bytes(text: &str) -> Result<[u8; 11], String> {
    let upper = text.to_ascii_uppercase();
    if upper.len() > 11 {
        return Err(format!("a FAT label holds at most 11 characters, and \"{text}\" has more"));
    }
    if let Some(c) =
        upper.chars().find(|&c| !c.is_ascii() || c < ' ' || "\"*+,./:;<=>?[\\]|".contains(c))
    {
        return Err(format!("a FAT label may not hold \"{c}\""));
    }
    let mut label = [b' '; 11];
    label[..upper.len()].copy_from_slice(upper.as_bytes());
    Ok(label)
}

/// The layout of a volume.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Geometry {
    bits: u8,
    cluster_sectors: u32,
    reserved: u32,
    fat_sectors: u32,
    root_entries: u32,
    clusters: u32,
}

/// The kind of FAT a volume of `bytes` gets.
fn default_bits(bytes: u64) -> u8 {
    if bytes <= 8400 * 512 {
        12
    } else if bytes < 512 << 20 {
        16
    } else {
        32
    }
}

/// The cluster size the specification's tables give a volume of `bytes`
/// with `bits`; for FAT12, the smallest that counts its clusters.
fn default_cluster(bytes: u64, bits: u8) -> u32 {
    let sectors = bytes / 512;
    match bits {
        12 => {
            let mut cluster = 512;
            while sectors * 512 / u64::from(cluster) > 4084 && cluster < 64 << 10 {
                cluster *= 2;
            }
            cluster
        }
        16 => match sectors {
            0..=32_680 => 1 << 10,
            32_681..=262_144 => 2 << 10,
            262_145..=524_288 => 4 << 10,
            524_289..=1_048_576 => 8 << 10,
            1_048_577..=2_097_152 => 16 << 10,
            2_097_153..=4_194_304 => 32 << 10,
            _ => 64 << 10,
        },
        _ => match sectors {
            0..=532_480 => 512,
            532_481..=16_777_216 => 4 << 10,
            16_777_217..=33_554_432 => 8 << 10,
            33_554_433..=67_108_864 => 16 << 10,
            _ => 32 << 10,
        },
    }
}

/// The range of cluster counts a kind of FAT has.
fn counts(bits: u8) -> (u64, u64) {
    match bits {
        12 => (1, 4084),
        16 => (4085, 65_524),
        _ => (65_525, u64::from(MAX_FAT32_CLUSTERS)),
    }
}

/// The layout of `total` sectors of `sector` bytes with clusters of
/// `cluster_sectors`: the tables made large enough for the clusters left
/// after them, and the reserved sectors grown so that the data area
/// starts on a whole cluster.
fn geometry(
    total: u64,
    sector: u32,
    bits: u8,
    cluster_sectors: u32,
    fats: u32,
    root_entries: u32,
) -> Option<Geometry> {
    let reserved_min = if bits == 32 { 32 } else { 1 };
    let root_entries = if bits == 32 { 0 } else { root_entries };
    let root_sectors = u64::from((root_entries * 32).div_ceil(sector));
    let c = u64::from(cluster_sectors);
    let mut fat = 1u64;
    loop {
        let meta = reserved_min + u64::from(fats) * fat + root_sectors;
        let data = meta.div_ceil(c) * c;
        if data >= total {
            return None;
        }
        let clusters = (total - data) / c;
        let needed = ((clusters + 2) * u64::from(bits)).div_ceil(8).div_ceil(u64::from(sector));
        if needed <= fat {
            let reserved = data - u64::from(fats) * fat - root_sectors;
            return Some(Geometry {
                bits,
                cluster_sectors,
                reserved: u32::try_from(reserved).ok().filter(|&r| r <= 0xffff)?,
                fat_sectors: u32::try_from(fat).ok()?,
                root_entries,
                clusters: u32::try_from(clusters).ok()?,
            });
        }
        fat = needed;
    }
}

/// The layout `options` ask for on `total` sectors of `sector` bytes.
fn plan(total: u64, sector: u32, options: &Options) -> Result<Geometry, String> {
    let bytes = total * u64::from(sector);
    let bits = options.bits.unwrap_or_else(|| default_bits(bytes));
    if !matches!(bits, 12 | 16 | 32) {
        return Err(format!("FAT is FAT12, FAT16 or FAT32, not FAT{bits}"));
    }
    let fats = u32::from(options.fats.unwrap_or(2));
    if !(1..=2).contains(&fats) {
        return Err("a FAT volume has 1 or 2 allocation tables".to_string());
    }
    let root_entries = options.root_entries.unwrap_or(512);
    if bits != 32 && (root_entries == 0 || root_entries > 0xffff) {
        return Err("the root directory holds 1 to 65535 entries".to_string());
    }
    let cluster = match options.cluster_bytes {
        Some(c) if !c.is_power_of_two() || c < sector || c > 64 << 10 => {
            return Err(format!(
                "a cluster is a power of two from a sector ({sector} bytes) to 64 KiB, not {c} \
                 bytes"
            ));
        }
        Some(c) => c,
        None => default_cluster(bytes, bits).max(sector),
    };
    let (low, high) = counts(bits);
    let mut cluster_sectors = cluster / sector;
    loop {
        let g = geometry(total, sector, bits, cluster_sectors, fats, root_entries)
            .ok_or_else(|| format!("{} is too small for FAT{bits}", crate::size::human(bytes)))?;
        let count = u64::from(g.clusters);
        if count > high && options.cluster_bytes.is_none() && cluster_sectors * sector < 64 << 10 {
            cluster_sectors *= 2;
            continue;
        }
        if count < low || count > high {
            let size = crate::size::human(bytes);
            let cluster = crate::size::human(u64::from(cluster_sectors * sector));
            return Err(format!(
                "{size} in clusters of {cluster} makes {count} clusters, and FAT{bits} has {low} \
                 to {high}; {}",
                if count < low {
                    "pick a smaller cluster or FAT kind"
                } else {
                    "pick a larger cluster or FAT kind"
                }
            ));
        }
        return Ok(g);
    }
}

/// A date and time in FAT's form, in UTC: the date's year from 1980,
/// month and day, and the time's hours, minutes and two-second count.
pub fn dos_time(unix: u64) -> (u16, u16) {
    let days = unix / 86_400;
    let secs = unix % 86_400;
    let (year, month, day) = civil(days as i64);
    if year < 1980 {
        return (0x21, 0);
    }
    let date = (((year - 1980).min(127) as u16) << 9) | ((month as u16) << 5) | day as u16;
    let time =
        ((secs / 3600) as u16) << 11 | (((secs / 60) % 60) as u16) << 5 | ((secs % 60) / 2) as u16;
    (date, time)
}

/// The year, month and day of `days` after 1970-01-01 (Howard Hinnant's
/// `civil_from_days`).
fn civil(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let month = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let year = yoe + era * 400 + i64::from(month <= 2);
    (year, month, day)
}

/// The seconds since 1970 now.
pub fn unix_now() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs())
}

/// The boot code of a volume nothing boots from: it says so, waits for a
/// key and asks the firmware for the next boot device (INT 19h). Real
/// mode, loaded at 0:7C00; `MESSAGE_AT` is patched with where the text
/// is.
const BOOT_CODE: [u8; 31] = [
    0x31, 0xc0, // xor ax, ax
    0x8e, 0xd8, // mov ds, ax
    0xbe, 0x00, 0x00, // mov si, message
    0xfc, // cld
    0xac, // .print: lodsb
    0x84, 0xc0, // test al, al
    0x74, 0x09, // jz .key
    0xb4, 0x0e, // mov ah, 0x0e
    0xbb, 0x07, 0x00, // mov bx, 7
    0xcd, 0x10, // int 0x10
    0xeb, 0xf2, // jmp .print
    0x31, 0xc0, // .key: xor ax, ax
    0xcd, 0x16, // int 0x16
    0xcd, 0x19, // int 0x19
    0xf4, // hlt
    0xeb, 0xfd, // jmp hlt
];
const MESSAGE_AT: usize = 5;
const MESSAGE: &[u8] = b"This is not a bootable disk. Press a key to try the next one.\r\n\0";

/// Makes a FAT filesystem on `device`, as `options` ask.
///
/// # Errors
///
/// In words, if the volume cannot have what was asked, or it cannot be
/// written.
pub fn format(device: &Device, options: &Options, rng: &mut Rng) -> Result<Made, String> {
    let sector = device.sector_size();
    let total = device.sectors();
    let g = plan(total, sector, options)?;
    let label = options.label.as_deref().map(label_bytes).transpose()?;
    let volume_id = options.volume_id.unwrap_or_else(|| rng.next_u32());
    let now = options.now.unwrap_or_else(unix_now);
    let hidden = options.hidden.unwrap_or(device.base() / u64::from(sector));
    let fats = u32::from(options.fats.unwrap_or(2));
    let s = sector as usize;

    let mut boot = vec![0u8; s];
    let code_at = if g.bits == 32 { 90 } else { 62 };
    boot[0..3].copy_from_slice(&[0xeb, (code_at - 2) as u8, 0x90]);
    boot[3..11].copy_from_slice(b"MSWIN4.1");
    boot[11..13].copy_from_slice(&(sector as u16).to_le_bytes());
    boot[13] = g.cluster_sectors as u8;
    boot[14..16].copy_from_slice(&(g.reserved as u16).to_le_bytes());
    boot[16] = fats as u8;
    boot[17..19].copy_from_slice(&(g.root_entries as u16).to_le_bytes());
    if g.bits != 32 && total < 0x10000 {
        boot[19..21].copy_from_slice(&(total as u16).to_le_bytes());
    } else {
        let total32 = u32::try_from(total).map_err(|_| "the volume is larger than FAT counts")?;
        boot[32..36].copy_from_slice(&total32.to_le_bytes());
    }
    boot[21] = 0xf8;
    boot[24..26].copy_from_slice(&63u16.to_le_bytes());
    boot[26..28].copy_from_slice(&255u16.to_le_bytes());
    boot[28..32].copy_from_slice(&u32::try_from(hidden).unwrap_or(u32::MAX).to_le_bytes());
    let ebpb = if g.bits == 32 {
        boot[36..40].copy_from_slice(&g.fat_sectors.to_le_bytes());
        boot[44..48].copy_from_slice(&2u32.to_le_bytes());
        boot[48..50].copy_from_slice(&1u16.to_le_bytes());
        boot[50..52].copy_from_slice(&6u16.to_le_bytes());
        64
    } else {
        boot[22..24].copy_from_slice(&(g.fat_sectors as u16).to_le_bytes());
        36
    };
    boot[ebpb] = 0x80;
    boot[ebpb + 2] = 0x29;
    boot[ebpb + 3..ebpb + 7].copy_from_slice(&volume_id.to_le_bytes());
    boot[ebpb + 7..ebpb + 18].copy_from_slice(&label.unwrap_or(*b"NO NAME    "));
    let kind: &[u8; 8] = match g.bits {
        12 => b"FAT12   ",
        16 => b"FAT16   ",
        _ => b"FAT32   ",
    };
    boot[ebpb + 18..ebpb + 26].copy_from_slice(kind);
    let mut code = BOOT_CODE;
    let message = (0x7c00 + code_at + BOOT_CODE.len()) as u16;
    code[MESSAGE_AT..MESSAGE_AT + 2].copy_from_slice(&message.to_le_bytes());
    boot[code_at..code_at + code.len()].copy_from_slice(&code);
    boot[code_at + code.len()..code_at + code.len() + MESSAGE.len()].copy_from_slice(MESSAGE);
    boot[510] = 0x55;
    boot[511] = 0xaa;

    let write = |offset: u64, bytes: &[u8]| {
        device.write_at(offset, bytes).map_err(|e| format!("cannot write the volume: {e}"))
    };
    let zero = |offset: u64, len: u64| {
        device.zero(offset, len).map_err(|e| format!("cannot write the volume: {e}"))
    };
    let sector64 = u64::from(sector);
    // The reserved sectors, the tables and FAT12/16's root directory
    // start as zeros.
    let root_sectors = u64::from((g.root_entries * 32).div_ceil(sector));
    let meta = u64::from(g.reserved) + u64::from(fats) * u64::from(g.fat_sectors) + root_sectors;
    zero(0, meta * sector64)?;
    if g.bits == 32 {
        let mut info = vec![0u8; s];
        info[0..4].copy_from_slice(&0x4161_5252u32.to_le_bytes());
        info[484..488].copy_from_slice(&0x6141_7272u32.to_le_bytes());
        info[488..492].copy_from_slice(&(g.clusters - 1).to_le_bytes());
        info[492..496].copy_from_slice(&3u32.to_le_bytes());
        info[508..512].copy_from_slice(&0xaa55_0000u32.to_le_bytes());
        for at in [0u64, 6] {
            write(at * sector64, &boot)?;
            write((at + 1) * sector64, &info)?;
        }
        // The third sector of each boot area ends as a boot sector does,
        // as Windows' format writes it.
        let mut third = vec![0u8; s];
        third[510..512].copy_from_slice(&[0x55, 0xaa]);
        write(2 * sector64, &third)?;
        write(8 * sector64, &third)?;
    } else {
        write(0, &boot)?;
    }
    // Each table: the media byte and the end-of-chain marks of clusters
    // 0 and 1, which also say the volume was cleanly unmounted, and for
    // FAT32 the root directory's cluster, 2, a chain of one.
    let head: Vec<u8> = match g.bits {
        12 => vec![0xf8, 0xff, 0xff],
        16 => vec![0xf8, 0xff, 0xff, 0xff],
        _ => vec![0xf8, 0xff, 0xff, 0x0f, 0xff, 0xff, 0xff, 0x0f, 0xff, 0xff, 0xff, 0x0f],
    };
    for n in 0..u64::from(fats) {
        write((u64::from(g.reserved) + n * u64::from(g.fat_sectors)) * sector64, &head)?;
    }
    let root_at = if g.bits == 32 {
        let at = (meta) * sector64;
        zero(at, u64::from(g.cluster_sectors) * sector64)?;
        at
    } else {
        (u64::from(g.reserved) + u64::from(fats) * u64::from(g.fat_sectors)) * sector64
    };
    if let Some(label) = label {
        let mut entry = [0u8; 32];
        entry[..11].copy_from_slice(&label);
        entry[11] = 0x08;
        let (date, time) = dos_time(now);
        entry[22..24].copy_from_slice(&time.to_le_bytes());
        entry[24..26].copy_from_slice(&date.to_le_bytes());
        write(root_at, &entry)?;
    }
    signatures::wipe(device, "vfat").map_err(|e| format!("cannot write the volume: {e}"))?;
    device.sync().map_err(|e| format!("cannot write the volume: {e}"))?;
    Ok(Made {
        bits: g.bits,
        sector_size: sector,
        cluster_bytes: g.cluster_sectors * sector,
        clusters: g.clusters,
        fat_sectors: g.fat_sectors,
        reserved: g.reserved,
        volume_id,
        label: options.label.as_ref().map(|l| l.to_ascii_uppercase()),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kinds_and_clusters_follow_the_specification() {
        let mib = |n: u64| (n << 20) / 512;
        let g = plan(2880, 512, &Options::default()).unwrap();
        assert_eq!((g.bits, g.cluster_sectors), (12, 1));
        let g = plan(mib(64), 512, &Options::default()).unwrap();
        assert_eq!((g.bits, g.cluster_sectors), (16, 4));
        let g = plan(mib(512), 512, &Options::default()).unwrap();
        assert_eq!((g.bits, g.cluster_sectors), (32, 8));
        let g = plan(mib(256), 512, &Options { bits: Some(32), ..Options::default() }).unwrap();
        assert_eq!((g.bits, g.cluster_sectors), (32, 1));
        assert!(plan(mib(16), 512, &Options { bits: Some(32), ..Options::default() }).is_err());
        let g = plan(mib(512), 4096, &Options::default()).unwrap();
        assert_eq!((g.bits, g.cluster_sectors), (32, 1));
        for total in [2880, mib(16), mib(100), mib(600), mib(9000)] {
            let g = plan(total, 512, &Options::default()).unwrap();
            let data = u64::from(g.reserved)
                + 2 * u64::from(g.fat_sectors)
                + u64::from((g.root_entries * 32).div_ceil(512));
            assert_eq!(data % u64::from(g.cluster_sectors), 0, "{total}");
            let needed = (u64::from(g.clusters) + 2) * u64::from(g.bits);
            assert!(u64::from(g.fat_sectors) * 512 * 8 >= needed);
        }
    }

    #[test]
    fn labels_and_times() {
        assert_eq!(&label_bytes("Dukovo").unwrap(), b"DUKOVO     ");
        assert!(label_bytes("way too long a label").is_err());
        assert!(label_bytes("a/b").is_err());
        assert!(label_bytes("ünï").is_err());
        assert_eq!(dos_time(0), (0x21, 0));
        // 2026-11-02 01:15:52 UTC.
        assert_eq!(
            dos_time(1_793_582_152),
            ((46 << 9) + (11 << 5) + 2, (1 << 11) + (15 << 5) + 26)
        );
        assert_eq!(volume_id_text(0x1234_abcd), "1234-ABCD");
    }

    #[test]
    fn the_boot_code_jumps_where_it_says() {
        // jz .key from offset 11 lands on the `xor` at 22; jmp .print
        // from 20 lands on the lodsb at 8.
        assert_eq!(11 + 2 + BOOT_CODE[12] as usize, 22);
        assert_eq!(20 + 2 - (256 - BOOT_CODE[21] as usize), 8);
        assert_eq!(28 + 3 - (256 - BOOT_CODE[30] as usize), 28);
    }
}
